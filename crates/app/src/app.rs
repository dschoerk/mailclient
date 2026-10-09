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
use slint::{ComponentHandle, ModelRc, SharedString, VecModel};

use crate::compose::Composer;
use crate::format;
use crate::{AppWindow, AttachmentItem, ConversationItem, FolderItem, MessageItem, SetupData};

/// Most messages shown in one folder list.
const LIST_LIMIT: usize = 5000;

/// Account colors (RGB) for the bar in the message list, by the order accounts were added.
const ACCOUNT_COLORS: [u32; 8] = [0x2f6fde, 0xe8730c, 0x2e9e5b, 0x8e44ad, 0xd64545, 0x1a9ba1, 0xd6457f, 0x9a6b3f];

/// Color of account `index` out of `count`; none with a single account, where there is nothing to tell apart.
fn account_color(index: usize, count: usize) -> slint::Color {
    if count < 2 {
        return slint::Color::from_argb_encoded(0);
    }
    slint::Color::from_argb_encoded(0xff00_0000 | ACCOUNT_COLORS[index % ACCOUNT_COLORS.len()])
}

thread_local! {
    static CONTROLLER: RefCell<Option<Rc<Controller>>> = const { RefCell::new(None) };
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
        on!(on_refresh, |c| {
            for account in &c.state.borrow().accounts {
                account.engine.send(Command::SyncAll);
            }
        });
        on!(on_compose, |c| c.open_composer(c.compose_account(), Draft::default(), None));
        on!(on_reply, |c| c.reply(ReplyKind::Reply));
        on!(on_reply_all, |c| c.reply(ReplyKind::ReplyAll));
        on!(on_forward, |c| c.reply(ReplyKind::Forward));
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

    #[cfg(feature = "servo")]
    fn init_webview(self: &Rc<Self>, window: &AppWindow) {
        use crate::webview::{MailView, delegate::Hooks};

        let weak = Rc::downgrade(self);
        let initialized = std::cell::Cell::new(false);
        let result = window.window().set_rendering_notifier(move |state, graphics_api| {
            if !matches!(state, slint::RenderingState::RenderingSetup) || initialized.get() {
                return;
            }
            let slint::GraphicsAPI::WGPU30 { device, queue, .. } = graphics_api else {
                log::error!("renderer is not wgpu 30; HTML view disabled");
                return;
            };
            let Some(controller) = weak.upgrade() else { return };
            let link_target = weak.clone();
            let status_target = weak.clone();
            let hooks = Hooks {
                on_link: Box::new(move |url| {
                    if let Some(c) = link_target.upgrade() {
                        c.open_link(url.as_str());
                    }
                }),
                on_status: Box::new(move |text| {
                    if let Some(c) = status_target.upgrade() {
                        let engine_status = c.state.borrow().engine_status.clone();
                        c.window().set_status(text.unwrap_or(engine_status).into());
                    }
                }),
            };
            let view = MailView::new(&controller.window(), device.clone(), queue.clone(), hooks);
            *controller.viewer.borrow_mut() = Some(view);
            initialized.set(true);
            // A message may have been selected before the view existed.
            controller.render_selected();
        });
        if let Err(e) = result {
            log::error!("cannot install rendering notifier: {e:?}");
        }
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

    // ---- setup ----

    fn setup_email_edited(&self, email: &str) {
        let window = self.window();
        let mut data = window.get_setup();
        let guess = AccountConfig::guess(email);
        data.username = guess.username.into();
        data.imap_host = guess.imap_host.into();
        data.imap_port = guess.imap_port.to_string().into();
        data.imap_starttls = guess.imap_security == Security::StartTls;
        data.smtp_host = guess.smtp_host.into();
        data.smtp_port = guess.smtp_port.to_string().into();
        data.smtp_starttls = guess.smtp_security == Security::StartTls;
        data.oauth = guess.auth != Auth::Password;
        // Follow the address while the name still looks auto-filled (typing arrives per character).
        let name = data.name.to_string();
        if name.is_empty() || guess.name.starts_with(&name) || name.starts_with(&guess.name) {
            data.name = guess.name.into();
        }
        window.set_setup(data);
    }

    fn account_from_setup(data: &SetupData) -> Result<AccountConfig> {
        let port = |s: &SharedString, what: &str| s.trim().parse::<u16>().map_err(|_| anyhow!("invalid {what} port"));
        let security = |starttls: bool| if starttls { Security::StartTls } else { Security::Tls };
        let account = AccountConfig {
            name: data.name.trim().to_string(),
            email: data.email.trim().to_string(),
            username: data.username.trim().to_string(),
            imap_host: data.imap_host.trim().to_string(),
            imap_port: port(&data.imap_port, "IMAP")?,
            imap_security: security(data.imap_starttls),
            smtp_host: data.smtp_host.trim().to_string(),
            smtp_port: port(&data.smtp_port, "SMTP")?,
            smtp_security: security(data.smtp_starttls),
            auth: if data.oauth { Auth::Microsoft } else { Auth::Password },
        };
        if !account.email.contains('@') {
            return Err(anyhow!("enter a valid email address"));
        }
        if account.imap_host.is_empty() || account.smtp_host.is_empty() {
            return Err(anyhow!("enter the IMAP and SMTP servers"));
        }
        if account.auth == Auth::Password && data.password.is_empty() {
            return Err(anyhow!("enter your password"));
        }
        Ok(account)
    }

    fn setup_submit(&self) {
        let window = self.window();
        let data = window.get_setup();
        let account = match Self::account_from_setup(&data) {
            Ok(a) => a,
            Err(e) => return window.set_setup_error(e.to_string().into()),
        };
        if self.state.borrow().accounts.iter().any(|a| a.config.email.eq_ignore_ascii_case(&account.email)) {
            return window.set_setup_error("This account is already set up".into());
        }
        window.set_setup_error("".into());
        window.set_setup_busy(true);
        let password = data.password.to_string();
        // Verify the login before saving anything.
        std::thread::spawn(move || {
            let result = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(anyhow::Error::from)
                .and_then(|rt| {
                    rt.block_on(async {
                        // (secret to log in with now, secret to keep in the keyring)
                        let (login, keep) = match account.auth {
                            Auth::Password => (password.clone(), password),
                            Auth::Microsoft => {
                                let sign_in = mailcore::auth::SignIn::start(&account.email).await?;
                                open_url(&sign_in.url)?;
                                let tokens = tokio::time::timeout(std::time::Duration::from_secs(300), sign_in.finish())
                                    .await
                                    .map_err(|_| anyhow!("sign-in timed out; try again"))??;
                                let refresh = tokens.refresh.ok_or_else(|| anyhow!("Microsoft returned no refresh token"))?;
                                (tokens.access, refresh)
                            }
                        };
                        let imap = mailcore::imap::Imap::connect(&account, &login).await?;
                        imap.logout().await;
                        anyhow::Ok(keep)
                    })
                })
                .and_then(|keep| account.store_password(&keep))
                .and_then(|()| config::save_account(&account));
            with_controller(move |c| {
                let window = c.window();
                window.set_setup_busy(false);
                match result.and_then(|()| c.start(account)) {
                    Ok(()) => {
                        window.set_needs_setup(false);
                        window.set_setup(SetupData::default());
                    }
                    Err(e) => window.set_setup_error(format!("{e:#}").into()),
                }
            });
        });
    }

    fn start(&self, account: AccountConfig) -> Result<()> {
        let db = config::data_dir()?.join(format!("{}.sqlite", account.email));
        let store = Store::open(&db).with_context(|| format!("opening {}", db.display()))?;
        let index = self.state.borrow().accounts.len();
        let engine = Engine::start(account.clone(), db, move |event| with_controller(move |c| c.on_event(index, event)));
        let label = {
            let mut state = self.state.borrow_mut();
            state.accounts.push(Account { config: account, engine, store, folders: Vec::new(), online: true });
            // Back to the default view: the unified inbox once there are several accounts.
            state.current = None;
            match state.accounts.as_slice() {
                [only] => format::person(&only.config.name, &only.config.email),
                all => format!("{} accounts", all.len()),
            }
        };
        self.window().set_account_label(label.into());
        self.window().set_has_account(true);
        // Show the cache immediately; the engine syncs in the background.
        self.reload_folders();
        Ok(())
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
                if let Some(composer) = self.composer.borrow().as_ref().filter(|c| c.id() == Some(id)) {
                    composer.send_failed(&error);
                }
            }
        }
    }

    // ---- folders and lists ----

    fn reload_folders(&self) {
        let loaded: Result<Vec<Vec<Folder>>> = self.state.borrow().accounts.iter().map(|a| a.store.folders()).collect();
        let loaded = match loaded {
            Ok(f) => f,
            Err(e) => return self.show_error(&e),
        };
        let count = loaded.len();
        let mut rows = Vec::new();
        let mut items = Vec::new();
        if count > 1 {
            rows.push(Row::Unified);
            items.push(FolderItem {
                role: "unified".into(),
                label: "All inboxes".into(),
                unread: loaded.iter().flatten().filter(|f| f.role == Role::Inbox).map(|f| f.unread as i32).sum(),
                ..Default::default()
            });
        }
        let needs_initial_folder = {
            let mut state = self.state.borrow_mut();
            for (i, (account, folders)) in state.accounts.iter_mut().zip(loaded).enumerate() {
                if count > 1 {
                    rows.push(Row::Account(i));
                    items.push(FolderItem {
                        role: "account".into(),
                        label: account.config.email.clone().into(),
                        color: account_color(i, count),
                        ..Default::default()
                    });
                }
                for f in &folders {
                    rows.push(Row::Folder(i, f.name.clone()));
                    items.push(FolderItem {
                        name: f.name.clone().into(),
                        role: f.role.as_str().into(),
                        label: f.display_name().into(),
                        unread: if matches!(f.role, Role::Sent | Role::Drafts | Role::Trash | Role::Junk) {
                            0
                        } else {
                            f.unread as i32
                        },
                        depth: f.depth() as i32,
                        color: account_color(i, count),
                    });
                }
                account.folders = folders;
            }
            state.rows = rows;
            if state.current.as_ref().is_some_and(|c| !state.rows.contains(c)) {
                state.current = None;
            }
            state.current.is_none() && state.rows.iter().any(|r| matches!(r, Row::Unified | Row::Folder(..)))
        };
        self.folder_model.set_vec(items);
        if needs_initial_folder {
            let start = {
                let state = self.state.borrow();
                let inbox = |r: &Row| match r {
                    Row::Unified => true,
                    Row::Folder(a, f) => state.is_inbox(*a, f),
                    Row::Account(_) => false,
                };
                state
                    .rows
                    .iter()
                    .position(inbox)
                    .or_else(|| state.rows.iter().position(|r| matches!(r, Row::Folder(..))))
                    .unwrap_or(0)
            };
            self.select_folder(start);
        } else {
            self.update_folder_selection();
        }
    }

    fn update_folder_selection(&self) {
        let state = self.state.borrow();
        let index = state.current.as_ref().and_then(|c| state.rows.iter().position(|r| r == c));
        let window = self.window();
        window.set_current_folder(index.map(|i| i as i32).unwrap_or(-1));
        if state.search.is_empty() {
            let title = match &state.current {
                Some(Row::Unified) => "All inboxes".to_string(),
                Some(Row::Folder(a, f)) => state.folder(*a, f).map(|f| f.display_name()).unwrap_or_default(),
                _ => String::new(),
            };
            window.set_list_title(title.into());
        }
    }

    fn select_folder(&self, index: usize) {
        let Some(row) = self.state.borrow().rows.get(index).cloned() else { return };
        if matches!(row, Row::Account(_)) {
            return;
        }
        {
            let mut state = self.state.borrow_mut();
            state.current = Some(row.clone());
            state.search.clear();
        }
        self.window().set_searching(false);
        self.update_folder_selection();
        self.reload_messages();
        // Inboxes, all the unified view shows, are synced by every engine anyway.
        if let Row::Folder(account, name) = row {
            self.send(account, Command::OpenFolder(name));
        }
    }

    /// Account that new messages are sent from: the one being browsed, else the first.
    fn compose_account(&self) -> usize {
        match &self.state.borrow().current {
            Some(Row::Folder(a, _)) => *a,
            _ => 0,
        }
    }

    fn search(&self, text: &str) {
        self.state.borrow_mut().search = text.trim().to_string();
        let searching = !text.trim().is_empty();
        let window = self.window();
        window.set_searching(searching);
        if searching {
            window.set_list_title(format!("Search: {}", text.trim()).into());
        } else {
            self.update_folder_selection();
        }
        self.reload_messages();
    }

    fn reload_messages(&self) {
        let rows = {
            let state = self.state.borrow();
            let limit = if state.search.is_empty() { LIST_LIMIT } else { 500 };
            let mut rows = Vec::new();
            for (i, account) in state.accounts.iter().enumerate() {
                let store = &account.store;
                let found = if !state.search.is_empty() {
                    store.search(&state.search, limit)
                } else {
                    match &state.current {
                        Some(Row::Folder(a, folder)) if *a == i => store.messages(folder, limit),
                        Some(Row::Unified) => match store.folder_by_role(Role::Inbox) {
                            Ok(Some(inbox)) => store.messages(&inbox, limit),
                            other => other.map(|_| Vec::new()),
                        },
                        _ => Ok(Vec::new()),
                    }
                };
                // shortcut: threads the whole cache on every reload; fine for a few thousand
                // cached messages, store conversation ids at sync time when caches grow larger.
                let all = match found {
                    Ok(listed) if listed.is_empty() => continue,
                    Ok(listed) => store.all_messages().map(|all| (listed, all)),
                    Err(e) => Err(e),
                };
                let (listed, all) = match all {
                    Ok(found) => found,
                    Err(e) => return self.show_error(&e),
                };
                let role = |folder: &str| state.folder(i, folder).map(|f| f.role).unwrap_or(Role::Other);
                rows.extend(conversation_rows(listed, &all, role).into_iter().map(|(m, c)| (i, m, c)));
            }
            // Each account lists newest first; a stable sort merges them and keeps that order.
            rows.sort_by_key(|(_, m, _)| Reverse(m.date));
            rows.truncate(limit);
            rows
        };
        let items: Vec<MessageItem> = {
            let state = self.state.borrow();
            let count = state.accounts.len();
            let show_recipients = |account: usize, folder: &str| {
                state.folder(account, folder).is_some_and(|f| matches!(f.role, Role::Sent | Role::Drafts))
            };
            rows.iter()
                .map(|(a, m, conversation)| MessageItem {
                    folder: m.folder.clone().into(),
                    uid: m.uid as i32,
                    sender: if show_recipients(*a, &m.folder) {
                        format!("To: {}", m.to)
                    } else if m.from_name.is_empty() {
                        m.from_addr.clone()
                    } else {
                        m.from_name.clone()
                    }
                    .into(),
                    subject: if m.subject.is_empty() { "(no subject)".into() } else { m.subject.clone().into() },
                    snippet: m.snippet.clone().into(),
                    date: format::list_date(m.date).into(),
                    seen: conversation.iter().all(|c| c.seen),
                    flagged: conversation.iter().any(|c| c.flagged),
                    answered: m.answered,
                    attachments: conversation.iter().any(|c| c.has_attachments),
                    color: account_color(*a, count),
                    count: conversation.len() as i32,
                })
                .collect()
        };
        self.message_model.set_vec(items);
        let selected_index = {
            let mut state = self.state.borrow_mut();
            (state.messages, state.conversations) = rows.into_iter().map(|(a, m, c)| ((a, m), c)).unzip();
            let index = state.selected_row();
            if index.is_none() && state.selected.is_some() {
                // The selected message is gone (moved elsewhere or deleted on another device).
                state.selected = None;
                state.body = None;
            }
            index
        };
        self.window().set_current_message(selected_index.map(|i| i as i32).unwrap_or(-1));
        if selected_index.is_none() {
            if self.window().get_has_message() {
                self.clear_reader();
            }
        } else {
            self.update_reader_header();
        }
    }

    // ---- reading ----

    /// Shows a message file read-only, without connecting to any account.
    fn open_file(&self, path: &std::path::Path) -> Result<()> {
        let raw = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
        let summary = mime::summary_from_headers("", 0, &raw, None);
        {
            let mut state = self.state.borrow_mut();
            state.selected = Some((0, String::new(), 0));
            state.standalone = Some((summary, raw));
        }
        let window = self.window();
        window.set_list_title(path.file_name().unwrap_or_default().to_string_lossy().as_ref().into());
        window.set_account_label("Viewing file — no account".into());
        self.render_selected();
        Ok(())
    }

    fn raw_of(&self, account: usize, m: &MessageSummary) -> Result<Option<Vec<u8>>> {
        let state = self.state.borrow();
        if let Some((_, raw)) = &state.standalone {
            return Ok(Some(raw.clone()));
        }
        match state.accounts.get(account) {
            Some(a) => a.store.raw(&m.folder, m.uid),
            None => Ok(None),
        }
    }

    /// The selected message and its account.
    fn selected_summary(&self) -> Option<(usize, MessageSummary)> {
        let state = self.state.borrow();
        if let Some((summary, _)) = &state.standalone {
            return Some((0, summary.clone()));
        }
        let (account, folder, uid) = state.selected.as_ref()?;
        let row = state.selected_row()?;
        let m = state.conversations[row].iter().find(|m| &m.folder == folder && m.uid == *uid)?;
        Some((*account, m.clone()))
    }

    fn select_message(&self, index: usize) {
        let Some((account, summary)) = self.state.borrow().messages.get(index).cloned() else { return };
        let key = (account, summary.folder.clone(), summary.uid);
        if self.state.borrow().selected.as_ref() == Some(&key) {
            return;
        }
        {
            let mut state = self.state.borrow_mut();
            state.selected = Some(key);
            state.body = None;
        }
        self.window().set_current_message(index as i32);
        // Opening a conversation reads all of it.
        let unread: Vec<MessageSummary> =
            self.state.borrow().conversations[index].iter().filter(|m| !m.seen).cloned().collect();
        if !unread.is_empty() {
            self.apply(account, &unread, PendingOp::SetSeen(true));
        }
        self.render_selected();
    }

    /// Shows another message of the open conversation.
    fn select_in_conversation(&self, index: usize) {
        let found = {
            let state = self.state.borrow();
            state.selected_row().and_then(|row| Some((state.messages[row].0, state.conversations[row].get(index)?.clone())))
        };
        let Some((account, m)) = found else { return };
        {
            let mut state = self.state.borrow_mut();
            state.selected = Some((account, m.folder.clone(), m.uid));
            state.body = None;
        }
        if !m.seen {
            self.apply(account, std::slice::from_ref(&m), PendingOp::SetSeen(true));
        }
        self.render_selected();
    }

    fn close_message(&self) {
        {
            let mut state = self.state.borrow_mut();
            state.selected = None;
            state.body = None;
        }
        self.window().set_current_message(-1);
        self.clear_reader();
    }

    fn clear_reader(&self) {
        let window = self.window();
        window.set_has_message(false);
        window.set_msg_loading(false);
        self.attachment_model.set_vec(Vec::new());
        self.conversation_model.set_vec(Vec::new());
        #[cfg(feature = "servo")]
        if let Some(view) = self.viewer.borrow().as_ref() {
            view.clear();
        }
    }

    fn update_reader_header(&self) {
        let Some((_, m)) = self.selected_summary() else { return };
        let window = self.window();
        window.set_has_message(true);
        window.set_msg_subject(if m.subject.is_empty() { "(no subject)".into() } else { m.subject.clone().into() });
        window.set_msg_from(format::person(&m.from_name, &m.from_addr).into());
        window.set_msg_to(m.to.clone().into());
        window.set_msg_cc(m.cc.clone().into());
        window.set_msg_date(format::full_date(m.date).into());
        window.set_msg_seen(m.seen);
        window.set_msg_flagged(m.flagged);
        let state = self.state.borrow();
        let conversation = state.selected_row().map(|row| state.conversations[row].as_slice()).unwrap_or_default();
        // A single message needs no conversation strip.
        let items: Vec<ConversationItem> = if conversation.len() < 2 {
            Vec::new()
        } else {
            conversation
                .iter()
                .map(|c| ConversationItem {
                    sender: if c.from_name.is_empty() { c.from_addr.clone() } else { c.from_name.clone() }.into(),
                    date: format::list_date(c.date).into(),
                    snippet: c.snippet.clone().into(),
                    seen: c.seen,
                })
                .collect()
        };
        let position = conversation.iter().position(|c| c.folder == m.folder && c.uid == m.uid);
        window.set_current_in_conversation(position.map(|i| i as i32).unwrap_or(-1));
        self.conversation_model.set_vec(items);
    }

    /// Shows the selected message with the rest of its conversation, downloading missing bodies.
    fn render_selected(&self) {
        let Some((account, summary)) = self.selected_summary() else { return };
        self.update_reader_header();
        let window = self.window();
        let conversation = {
            let state = self.state.borrow();
            match state.selected_row() {
                Some(row) if state.standalone.is_none() => state.conversations[row].clone(),
                _ => vec![summary.clone()],
            }
        };
        let current = conversation.iter().position(|m| m.folder == summary.folder && m.uid == summary.uid).unwrap_or(0);
        let mut raws = Vec::new();
        for m in &conversation {
            match self.raw_of(account, m) {
                Ok(raw) => raws.push(raw),
                Err(e) => return self.show_error(&e),
            }
        }
        // Each arrives with BodyReady, which renders again.
        for (m, _) in conversation.iter().zip(&raws).filter(|(_, raw)| raw.is_none()) {
            if self.state.borrow_mut().fetching.insert((account, m.folder.clone(), m.uid)) {
                self.send(account, Command::FetchBody { folder: m.folder.clone(), uid: m.uid });
            }
        }
        if raws[current].is_none() {
            window.set_msg_loading(true);
            window.set_msg_text("".into());
            window.set_msg_remote_blocked(false);
            self.attachment_model.set_vec(Vec::new());
            #[cfg(feature = "servo")]
            if let Some(view) = self.viewer.borrow().as_ref() {
                view.clear();
            }
            return;
        }
        window.set_msg_loading(false);
        let bodies: Vec<Option<Body>> = raws.iter().map(|raw| raw.as_deref().map(mime::body_from_raw)).collect();
        let body = bodies[current].clone().expect("checked above");
        let allow_remote = {
            let state = self.state.borrow();
            conversation.iter().all(|m| state.remote_allowed.contains(&(account, m.folder.clone(), m.uid)))
        };
        let has_remote = bodies.iter().flatten().any(|b| b.has_remote_content);
        window.set_msg_remote_blocked(has_remote && !allow_remote && cfg!(feature = "servo"));
        let headings: Vec<String> = conversation
            .iter()
            .map(|m| format!("{} — {}", format::person(&m.from_name, &m.from_addr), format::full_date(m.date)))
            .collect();
        window.set_msg_text(
            if conversation.len() == 1 {
                body.text.clone()
            } else {
                headings
                    .iter()
                    .zip(&bodies)
                    .map(|(h, b)| format!("{h}\n\n{}", b.as_ref().map_or("Loading…", |b| b.text.as_str())))
                    .collect::<Vec<_>>()
                    .join("\n\n────────\n\n")
            }
            .into(),
        );
        self.attachment_model.set_vec(
            body.attachments
                .iter()
                .map(|a| AttachmentItem {
                    index: a.index as i32,
                    name: a.name.clone().into(),
                    size: format::size(a.size).into(),
                })
                .collect::<Vec<_>>(),
        );
        #[cfg(feature = "servo")]
        if let Some(view) = self.viewer.borrow().as_ref() {
            let (document, anchor) = if conversation.len() == 1 {
                (mime::render_document(&body), String::new())
            } else {
                let parts: Vec<(String, Option<&Body>)> = headings.into_iter().zip(bodies.iter().map(Option::as_ref)).collect();
                (mime::render_conversation(&parts, current), format!("m{current}"))
            };
            view.show(crate::webview::delegate::MessageContent {
                document,
                csp: mime::content_security_policy(allow_remote),
                // shortcut: one map for all messages; Content-IDs are meant to be globally unique.
                inline_parts: bodies.iter().flatten().flat_map(|b| b.inline_parts.clone()).collect(),
                allow_remote,
                anchor,
            });
        }
        self.state.borrow_mut().body = Some(body);
    }

    /// Allows remote images for the open conversation.
    fn load_remote_images(&self) {
        let Some(key) = self.state.borrow().selected.clone() else { return };
        let keys: Vec<Key> = {
            let state = self.state.borrow();
            match state.selected_row() {
                Some(row) => state.conversations[row].iter().map(|m| (key.0, m.folder.clone(), m.uid)).collect(),
                None => vec![key],
            }
        };
        self.state.borrow_mut().remote_allowed.extend(keys);
        self.render_selected();
    }

    #[cfg_attr(not(feature = "servo"), allow(dead_code))]
    fn open_link(&self, url: &str) {
        if let Some(mailto) = url.strip_prefix("mailto:") {
            let (to, query) = mailto.split_once('?').unwrap_or((mailto, ""));
            let mut draft = Draft { to: mime::percent_decode(to), ..Default::default() };
            for pair in query.split('&') {
                if let Some((k, v)) = pair.split_once('=') {
                    let v = mime::percent_decode(&v.replace('+', " "));
                    match k.to_ascii_lowercase().as_str() {
                        "subject" => draft.subject = v,
                        "cc" => draft.cc = v,
                        "body" => draft.body = v,
                        _ => {}
                    }
                }
            }
            self.open_composer(self.compose_account(), draft, None);
        } else if (url.starts_with("https://") || url.starts_with("http://"))
            && let Err(e) = open_url(url)
        {
            self.show_error(&e);
        }
    }

    fn save_attachment(&self, index: usize) {
        // shortcut: no file picker on Android yet (needs the Storage Access Framework via JNI).
        #[cfg(target_os = "android")]
        {
            let _ = index;
            return self.set_status("Saving attachments is not supported on Android yet");
        }
        #[cfg(not(target_os = "android"))]
        self.save_attachment_desktop(index);
    }

    #[cfg(not(target_os = "android"))]
    fn save_attachment_desktop(&self, index: usize) {
        let Some((account, summary)) = self.selected_summary() else { return };
        let Ok(Some(raw)) = self.raw_of(account, &summary) else { return };
        let Some(name) = self
            .state
            .borrow()
            .body
            .as_ref()
            .and_then(|b| b.attachments.iter().find(|a| a.index == index))
            .map(|a| a.name.clone())
        else {
            return;
        };
        let weak = self.window.clone();
        let _ = slint::spawn_local(async move {
            let Some(file) = rfd::AsyncFileDialog::new().set_file_name(&name).save_file().await else { return };
            let result = mime::attachment_data(&raw, index)
                .ok_or_else(|| anyhow!("attachment not found"))
                .and_then(|data| std::fs::write(file.path(), data).map_err(Into::into));
            if let Some(window) = weak.upgrade() {
                window.set_status(match result {
                    Ok(()) => format!("Saved {}", file.path().display()).into(),
                    Err(e) => format!("⚠ saving attachment: {e:#}").into(),
                });
            }
        });
    }

    // ---- changing messages ----

    /// Applies a change locally right away and queues it for the server.
    fn apply(&self, account: usize, messages: &[MessageSummary], op: PendingOp) {
        let result = match self.state.borrow_mut().accounts.get_mut(account) {
            Some(a) => messages.iter().try_for_each(|m| a.store.apply_local(&m.folder, m.uid, &op)),
            None => return,
        };
        if let Err(e) = result {
            return self.show_error(&e);
        }
        self.reload_messages();
        self.reload_folders();
        self.send(account, Command::Flush);
    }

    fn toggle_selected(&self, flag: bool) {
        let Some((account, m)) = self.selected_summary() else { return };
        let op = if flag { PendingOp::SetFlagged(!m.flagged) } else { PendingOp::SetSeen(!m.seen) };
        self.apply(account, &[m], op);
    }

    /// Archives or deletes the open conversation: its messages in the folder of its list row.
    fn remove_selected(&self, archive: bool) {
        let Some((account, m)) = self.selected_summary() else { return };
        let index = self.window().get_current_message().max(0) as usize;
        let (folder, targets) = {
            let state = self.state.borrow();
            match (state.messages.get(index), state.conversations.get(index)) {
                (Some((_, row)), Some(conversation)) => {
                    (row.folder.clone(), conversation.iter().filter(|c| c.folder == row.folder).cloned().collect())
                }
                _ => (m.folder.clone(), vec![m]),
            }
        };
        let op = {
            let state = self.state.borrow();
            let Some(store) = state.accounts.get(account).map(|a| &a.store) else { return };
            if archive { engine::archive_op(store, &folder) } else { engine::delete_op(store, &folder) }
        };
        let op = match op {
            Ok(op) => op,
            Err(e) => return self.show_error(&e),
        };
        // Select the message that takes this one's place, like most mail clients.
        self.apply(account, &targets, op);
        let count = self.state.borrow().messages.len();
        if count > 0 {
            self.select_message(index.min(count - 1));
        }
    }

    // ---- composing ----

    fn reply(&self, kind: ReplyKind) {
        let Some((account, m)) = self.selected_summary() else { return };
        let me = self.state.borrow().accounts.get(account).map(|a| a.config.email.clone()).unwrap_or_default();
        let text = self.state.borrow().body.as_ref().map(|b| b.text.clone()).unwrap_or_default();
        let replied_to = (!matches!(kind, ReplyKind::Forward)).then(|| (m.folder.clone(), m.uid));
        let forwarding = matches!(kind, ReplyKind::Forward);
        let mut draft = mime::reply_draft(kind, &m, &text, &me);
        if forwarding {
            // Forward attachments too: write them to a temp dir and attach from there.
            if let Err(e) = self.attach_original_files(account, &m, &mut draft) {
                self.show_error(&e);
            }
        }
        self.open_composer(account, draft, replied_to);
    }

    fn attach_original_files(&self, account: usize, m: &MessageSummary, draft: &mut Draft) -> Result<()> {
        let Some(attachments) = self.state.borrow().body.as_ref().map(|b| b.attachments.clone()) else { return Ok(()) };
        if attachments.is_empty() {
            return Ok(());
        }
        let raw = self.raw_of(account, m)?.ok_or_else(|| anyhow!("message not downloaded"))?;
        let dir = std::env::temp_dir().join(format!("mailclient-fwd-{}-{}", std::process::id(), m.uid));
        std::fs::create_dir_all(&dir)?;
        for att in &attachments {
            let data = mime::attachment_data(&raw, att.index).ok_or_else(|| anyhow!("attachment {} missing", att.name))?;
            let safe_name: String = att.name.chars().map(|c| if c == '/' || c == '\\' { '_' } else { c }).collect();
            let path = dir.join(safe_name);
            std::fs::write(&path, data)?;
            draft.attachments.push(path);
        }
        Ok(())
    }

    /// `account`: who the message is sent from (and whose message `replied_to` is).
    fn open_composer(&self, account: usize, draft: Draft, replied_to: Option<(String, u32)>) {
        if self.state.borrow().accounts.get(account).is_none() {
            return;
        }
        if self.composer.borrow().is_some() {
            // One draft at a time; never throw away what the user is writing.
            return self.set_status("Send or discard the message you are writing first");
        }
        let weak_self = CONTROLLER.with(|c| c.borrow().as_ref().map(Rc::downgrade));
        let Some(weak_self) = weak_self else { return };
        let on_send: Box<dyn Fn(Draft, Option<(String, u32)>) -> u64> = {
            let weak = weak_self.clone();
            Box::new(move |draft, replied_to| {
                let Some(c) = weak.upgrade() else { return 0 };
                let id = c.next_send_id.get();
                c.next_send_id.set(id + 1);
                c.send(account, Command::Send { id, draft, replied_to });
                id
            })
        };
        let on_close: Box<dyn Fn(&Composer)> = Box::new(move |_| {
            if let Some(c) = weak_self.upgrade() {
                c.composer.borrow_mut().take();
            }
        });
        let composer = Composer::open(&self.window(), draft, replied_to, on_send, on_close);
        *self.composer.borrow_mut() = Some(composer);
    }
}

/// Groups `listed` (newest first) into list rows, one per conversation: its newest listed
/// message and the whole conversation from `all`, oldest first. Copies of a message (one sent to
/// oneself is also in Sent) appear once, and Trash/Junk only when the row is there itself.
fn conversation_rows(
    listed: Vec<MessageSummary>,
    all: &[MessageSummary],
    role: impl Fn(&str) -> Role,
) -> Vec<(MessageSummary, Vec<MessageSummary>)> {
    let numbers = mailcore::thread::conversations(all);
    let number: HashMap<(&str, u32), usize> =
        all.iter().zip(&numbers).map(|(m, n)| ((m.folder.as_str(), m.uid), *n)).collect();
    let mut members: HashMap<usize, Vec<&MessageSummary>> = HashMap::new();
    for (m, n) in all.iter().zip(&numbers) {
        members.entry(*n).or_default().push(m);
    }
    let mut done = HashSet::new();
    let mut rows = Vec::new();
    for m in listed {
        let n = number.get(&(m.folder.as_str(), m.uid)).copied();
        if n.is_some_and(|n| !done.insert(n)) {
            continue;
        }
        let mut conversation: Vec<MessageSummary> = match n.and_then(|n| members.get(&n)) {
            Some(members) => members
                .iter()
                .filter(|c| c.folder == m.folder || !matches!(role(&c.folder), Role::Trash | Role::Junk))
                .map(|c| (*c).clone())
                .collect(),
            None => vec![m.clone()],
        };
        // Keep the copy in the row's folder, where changes to it show.
        conversation.sort_by_key(|c| c.folder != m.folder);
        let mut ids = HashSet::new();
        conversation.retain(|c| c.message_id.is_empty() || ids.insert(c.message_id.clone()));
        conversation.sort_by_key(|c| c.date);
        rows.push((m, conversation));
    }
    rows
}

#[cfg(test)]
#[test]
fn groups_list_into_conversations() {
    let msg = |folder: &str, uid, id: &str, reply_to: &str, date| MessageSummary {
        folder: folder.into(),
        uid,
        message_id: id.into(),
        in_reply_to: reply_to.into(),
        date,
        ..Default::default()
    };
    let all = [
        msg("INBOX", 1, "<a>", "", 1),
        msg("Sent", 1, "<b>", "<a>", 2),
        msg("INBOX", 2, "<c>", "<b>", 3),
        msg("INBOX", 3, "<other>", "", 4),
        msg("Trash", 1, "<d>", "<c>", 5),
        // A message to oneself: in Sent and in the inbox.
        msg("Sent", 2, "<self>", "", 6),
        msg("INBOX", 4, "<self>", "", 6),
    ];
    let role = |f: &str| match f {
        "Trash" => Role::Trash,
        "Sent" => Role::Sent,
        _ => Role::Inbox,
    };
    let mut listed: Vec<MessageSummary> = all.iter().filter(|m| m.folder == "INBOX").cloned().collect();
    listed.sort_by_key(|m| Reverse(m.date));
    let rows = conversation_rows(listed, &all, role);
    let shape: Vec<(u32, Vec<(&str, u32)>)> =
        rows.iter().map(|(m, c)| (m.uid, c.iter().map(|c| (c.folder.as_str(), c.uid)).collect())).collect();
    assert_eq!(
        shape,
        [
            (4, vec![("INBOX", 4)]),
            (3, vec![("INBOX", 3)]),
            (2, vec![("INBOX", 1), ("Sent", 1), ("INBOX", 2)]),
        ]
    );
}

/// Opens `url` in the system browser.
fn open_url(url: &str) -> Result<()> {
    #[cfg(not(target_os = "android"))]
    open::that_detached(url).with_context(|| format!("opening {url}"))?;
    #[cfg(target_os = "android")]
    android_view(url).map_err(|e| anyhow!("opening {url}: {e}"))?;
    Ok(())
}

/// Starts an ACTION_VIEW intent, which Android routes to the default browser.
#[cfg(target_os = "android")]
fn android_view(url: &str) -> jni::errors::Result<()> {
    let ctx = ndk_context::android_context();
    // SAFETY: android-activity fills ndk_context with the process's JavaVM and the running activity.
    let (vm, activity) = unsafe {
        (jni::JavaVM::from_raw(ctx.vm().cast())?, jni::objects::JObject::from_raw(ctx.context().cast()))
    };
    let mut env = vm.attach_current_thread()?;
    let url = env.new_string(url)?;
    let uri = env
        .call_static_method("android/net/Uri", "parse", "(Ljava/lang/String;)Landroid/net/Uri;", &[(&url).into()])?
        .l()?;
    let action = env.new_string("android.intent.action.VIEW")?;
    let intent =
        env.new_object("android/content/Intent", "(Ljava/lang/String;Landroid/net/Uri;)V", &[(&action).into(), (&uri).into()])?;
    env.call_method(&activity, "startActivity", "(Landroid/content/Intent;)V", &[(&intent).into()])?;
    Ok(())
}
