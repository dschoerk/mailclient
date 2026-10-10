//! Glue between the Slint UI, the local store and the sync engine.

use std::cell::RefCell;
use std::cmp::Reverse;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use anyhow::{Context, Result, anyhow};
use mailcore::config::{self, AccountConfig, Auth, Security};
use mailcore::engine::{self, Command, Engine, Event};
use mailcore::mime::{self, Body, Draft, ReplyKind};
use mailcore::store::{Folder, MessageSummary, PendingOp, Role, Store};
use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};

use crate::compose::Composer;
use crate::format;
mod actions;
mod folders;
mod reader;
mod setup;

use crate::{AppWindow, AttachmentItem, ConversationItem, FolderItem, MessageItem, ReaderPart, SetupData};

thread_local! {
    static CONTROLLER: RefCell<Option<Rc<Controller>>> = const { RefCell::new(None) };
}

/// Opens a link from the HTML view (any thread): browser, or the composer for `mailto:`.
#[cfg(target_os = "android")]
pub(crate) fn open_link_later(url: String) {
    with_controller(move |c| c.open_link(&url));
}

/// The Back key, pressed while the HTML view had key focus (any thread).
#[cfg(target_os = "android")]
pub(crate) fn back_later() {
    with_controller(|c| {
        use slint::platform::{Key, WindowEvent};
        let window = c.window();
        window.window().dispatch_event(WindowEvent::KeyPressed { text: Key::Back.into() });
        window.window().dispatch_event(WindowEvent::KeyReleased { text: Key::Back.into() });
    });
}

/// Runs `f` on the UI thread with the controller, from any thread.
fn with_controller(f: impl FnOnce(&Rc<Controller>) + Send + 'static) {
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(c) = CONTROLLER.with(|c| c.borrow().clone()) {
            f(&c);
        }
    });
}

struct Account {
    config: AccountConfig,
    engine: Engine,
    store: Store,
    folders: Vec<Folder>,
    online: bool,
}

/// A row of the folder sidebar. Accounts are only ever appended, so their index is stable.
#[derive(Clone, PartialEq)]
enum Row {
    /// The inboxes of all accounts in one list.
    Unified,
    /// Heading above an account's folders.
    Account(usize),
    Folder(usize, String),
}

/// A message: (account index, folder, uid).
type Key = (usize, String, u32);

#[derive(Default)]
struct State {
    accounts: Vec<Account>,
    rows: Vec<Row>,
    current: Option<Row>,
    /// One per list row: its account and newest listed message.
    messages: Vec<(usize, MessageSummary)>,
    /// The conversation of each row, oldest first.
    conversations: Vec<Vec<MessageSummary>>,
    selected: Option<Key>,
    /// Ticked rows, by one of their messages (a row's newest message changes when replies arrive).
    checked: HashSet<Key>,
    body: Option<Body>,
    search: String,
    remote_allowed: HashSet<Key>,
    /// Bodies requested from the engine and not there yet.
    fetching: HashSet<Key>,
    engine_status: String,
    /// A message opened from a file (`mailclient message.eml`), shown without an account.
    standalone: Option<(MessageSummary, Vec<u8>)>,
}

impl State {
    fn folder(&self, account: usize, name: &str) -> Option<&Folder> {
        self.accounts.get(account)?.folders.iter().find(|f| f.name == name)
    }

    fn is_inbox(&self, account: usize, name: &str) -> bool {
        self.folder(account, name).is_some_and(|f| f.role == Role::Inbox)
    }

    fn is_checked(&self, account: usize, conversation: &[MessageSummary]) -> bool {
        conversation.iter().any(|m| self.checked.contains(&(account, m.folder.clone(), m.uid)))
    }

    /// The list row whose conversation holds the selected message.
    fn selected_row(&self) -> Option<usize> {
        let (account, folder, uid) = self.selected.as_ref()?;
        self.messages.iter().zip(&self.conversations).position(|((a, _), conversation)| {
            a == account && conversation.iter().any(|m| &m.folder == folder && m.uid == *uid)
        })
    }
}

pub struct Controller {
    window: slint::Weak<AppWindow>,
    state: RefCell<State>,
    folder_model: Rc<VecModel<FolderItem>>,
    message_model: Rc<VecModel<MessageItem>>,
    attachment_model: Rc<VecModel<AttachmentItem>>,
    conversation_model: Rc<VecModel<ConversationItem>>,
    composer: RefCell<Option<Rc<Composer>>>,
    next_send_id: std::cell::Cell<u64>,
    #[cfg(feature = "servo")]
    viewer: RefCell<Option<Rc<crate::webview::MailView>>>,
}

impl Controller {
    /// `eml`: a message file to show instead of an account (`mailclient message.eml`).
    pub fn install(window: &AppWindow, eml: Option<std::path::PathBuf>) -> Result<()> {
        let controller = Rc::new(Controller {
            window: window.as_weak(),
            state: RefCell::new(State::default()),
            folder_model: Rc::new(VecModel::default()),
            message_model: Rc::new(VecModel::default()),
            attachment_model: Rc::new(VecModel::default()),
            conversation_model: Rc::new(VecModel::default()),
            composer: RefCell::new(None),
            next_send_id: std::cell::Cell::new(1),
            #[cfg(feature = "servo")]
            viewer: RefCell::new(None),
        });
        CONTROLLER.with(|c| *c.borrow_mut() = Some(controller.clone()));

        window.set_folders(ModelRc::from(controller.folder_model.clone()));
        window.set_messages(ModelRc::from(controller.message_model.clone()));
        window.set_msg_attachments(ModelRc::from(controller.attachment_model.clone()));
        window.set_conversation(ModelRc::from(controller.conversation_model.clone()));
        window.set_use_webview(cfg!(feature = "servo"));
        window.set_html_overlay(cfg!(target_os = "android"));
        let settings = config::load_settings().unwrap_or_else(|e| {
            log::warn!("cannot read settings: {e:#}");
            config::Settings::default()
        });
        window.set_left_handed(settings.left_handed);
        window.set_floating_buttons(settings.floating_buttons);
        controller.connect_callbacks(window);
        #[cfg(feature = "servo")]
        controller.init_webview(window);

        let accounts = config::load_accounts()?;
        match eml {
            Some(path) => controller.open_file(&path)?,
            None if accounts.is_empty() => window.set_needs_setup(true),
            None => {
                for account in accounts {
                    controller.start(account)?;
                }
            }
        }
        Ok(())
    }

    /// Tears down the controller before the window goes away.
    pub fn shutdown() {
        let Some(controller) = CONTROLLER.with(|c| c.borrow_mut().take()) else { return };
        #[cfg(feature = "servo")]
        if let Some(view) = controller.viewer.borrow_mut().take() {
            view.shutdown();
        }
        drop(controller);
    }

    #[cfg(test)]
    pub(crate) fn current() -> Rc<Controller> {
        CONTROLLER.with(|c| c.borrow().clone()).expect("controller installed")
    }

    #[cfg(test)]
    pub(crate) fn composer(&self) -> Option<Rc<Composer>> {
        self.composer.borrow().clone()
    }

    fn window(&self) -> AppWindow {
        self.window.upgrade().expect("main window alive")
    }

    fn connect_callbacks(self: &Rc<Self>, window: &AppWindow) {
        macro_rules! on {
            ($setter:ident, |$c:ident $(, $arg:ident)*| $body:expr) => {{
                let weak = Rc::downgrade(self);
                window.$setter(move |$($arg),*| {
                    if let Some($c) = weak.upgrade() {
                        $body
                    }
                });
            }};
        }
        on!(on_setup_email_edited, |c, email| c.setup_email_edited(&email));
        on!(on_setup_submit, |c| c.setup_submit());
        on!(on_setup_cancel, |c| c.window().set_needs_setup(false));
        on!(on_add_account, |c| {
            let w = c.window();
            w.set_setup(SetupData::default());
            w.set_setup_error("".into());
            w.set_needs_setup(true);
        });
        on!(on_folder_selected, |c, index| c.select_folder(index as usize));
        on!(on_message_selected, |c, index| c.select_message(index as usize));
        on!(on_conversation_selected, |c, index| c.select_in_conversation(index as usize));
        on!(on_search, |c, text| c.search(&text));
        on!(on_show_sender, |c| {
            let Some((_, m)) = c.selected_summary() else { return };
            if m.from_addr.is_empty() {
                return;
            }
            let query = format!("from:{}", m.from_addr);
            c.window().set_search_text(query.as_str().into());
            c.search(&query);
        });
        on!(on_refresh, |c| {
            for account in &c.state.borrow().accounts {
                account.engine.send(Command::SyncAll);
            }
        });
        on!(on_compose, |c| c.open_composer(c.compose_account(), Draft::default(), None));
        on!(on_reply, |c| c.reply(ReplyKind::Reply));
        on!(on_reply_all, |c| c.reply(ReplyKind::ReplyAll));
        on!(on_forward, |c| c.reply(ReplyKind::Forward));
        on!(on_edit_draft, |c| c.edit_draft());
        on!(on_toggle_checked, |c, index| c.toggle_checked(index as usize));
        on!(on_bulk_action, |c, action| c.bulk(&action));
        on!(on_row_swiped, |c, index, archive| c.swiped(index as usize, archive));
        #[cfg(target_os = "android")]
        on!(on_html_area_changed, |c| {
            // Slint lays out in logical pixels; the WebView is placed in physical ones.
            let w = c.window();
            let scale = w.window().scale_factor();
            let px = |v: f32| (v * scale).round() as i32;
            crate::platform::html::place(px(w.get_html_x()), px(w.get_html_y()), px(w.get_html_width()), px(w.get_html_height()), w.get_html_visible());
        });
        on!(on_archive, |c| c.remove_selected(true));
        on!(on_delete, |c| c.remove_selected(false));
        on!(on_toggle_seen, |c| c.toggle_selected(false));
        on!(on_toggle_flag, |c| c.toggle_selected(true));
        on!(on_load_remote_images, |c| c.load_remote_images());
        on!(on_save_attachment, |c, index| c.save_attachment(index as usize));
        on!(on_open_attachment, |c, index| c.save_attachment(index as usize));
        on!(on_close_message, |c| c.close_message());
        on!(on_settings_changed, |c| {
            let w = c.window();
            let settings = config::Settings { left_handed: w.get_left_handed(), floating_buttons: w.get_floating_buttons() };
            if let Err(e) = config::save_settings(settings) {
                c.show_error(&e);
            }
        });
    }

    fn send(&self, account: usize, command: Command) {
        if let Some(a) = self.state.borrow().accounts.get(account) {
            a.engine.send(command);
        }
    }

    fn set_status(&self, text: impl Into<SharedString>) {
        let text = text.into();
        self.state.borrow_mut().engine_status = text.to_string();
        self.window().set_status(text);
    }

    fn show_error(&self, e: &anyhow::Error) {
        log::warn!("{e:#}");
        self.set_status(format!("⚠ {e:#}"));
    }

    // ---- engine events ----

    fn on_event(&self, account: usize, event: Event) {
        match event {
            Event::Status(s) => self.set_status(s),
            Event::Error(e) => self.set_status(format!("⚠ {e}")),
            Event::Online(online) => {
                let all_online = {
                    let mut state = self.state.borrow_mut();
                    if let Some(a) = state.accounts.get_mut(account) {
                        a.online = online;
                    }
                    state.accounts.iter().all(|a| a.online)
                };
                self.window().set_online(all_online);
            }
            Event::FoldersChanged => self.reload_folders(),
            Event::NewMail(messages) => {
                let (title, body) = match messages.as_slice() {
                    [m] => (format::person(&m.from_name, &m.from_addr), m.subject.clone()),
                    _ => (
                        format!("{} new messages", messages.len()),
                        messages.iter().map(|m| m.subject.as_str()).collect::<Vec<_>>().join("\n"),
                    ),
                };
                crate::platform::notify(&title, &body);
            }
            Event::MessagesChanged(_) => {
                let state = self.state.borrow();
                // Any folder can add to the listed conversations (replies sit in Sent).
                let relevant = !state.search.is_empty()
                    || match &state.current {
                        Some(Row::Folder(a, _)) => *a == account,
                        Some(Row::Unified) => true,
                        _ => false,
                    };
                drop(state);
                if relevant {
                    self.reload_messages();
                }
            }
            Event::BodyReady { folder, uid } => {
                let shown = {
                    let mut state = self.state.borrow_mut();
                    state.fetching.remove(&(account, folder.clone(), uid));
                    state.selected_row().is_some_and(|row| {
                        state.messages[row].0 == account
                            && state.conversations[row].iter().any(|m| m.folder == folder && m.uid == uid)
                    })
                };
                if shown {
                    self.render_selected();
                }
            }
            Event::Sent(id) => {
                let composer = self.composer.borrow().clone().filter(|c| c.id() == Some(id));
                if let Some(composer) = composer {
                    composer.close();
                }
            }
            Event::SendFailed(id, error) => {
                let composer = self.composer.borrow().clone();
                if !composer.is_some_and(|c| c.failed(id, &error)) {
                    // A save from a composer already closed by an earlier save.
                    self.set_status(format!("⚠ {error}"));
                }
            }
            Event::DraftSaved(id) => {
                let composer = self.composer.borrow().clone();
                if let Some(composer) = composer {
                    composer.draft_saved(id);
                }
            }
        }
    }
}
