//! Glue between the Slint UI, the local store and the sync engine.

use std::cell::RefCell;
use std::collections::HashSet;
use std::rc::Rc;

use anyhow::{Context, Result, anyhow};
use mailcore::config::{self, AccountConfig, Security};
use mailcore::engine::{self, Command, Engine, Event};
use mailcore::mime::{self, Body, Draft, ReplyKind};
use mailcore::store::{Folder, MessageSummary, PendingOp, Role, Store};
use slint::{ComponentHandle, ModelRc, SharedString, VecModel};

use crate::compose::Composer;
use crate::format;
use crate::{AppWindow, AttachmentItem, FolderItem, MessageItem, SetupData};

/// Most messages shown in one folder list.
const LIST_LIMIT: usize = 5000;

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

#[derive(Default)]
struct State {
    account: Option<AccountConfig>,
    engine: Option<Engine>,
    store: Option<Store>,
    folders: Vec<Folder>,
    current_folder: Option<String>,
    messages: Vec<MessageSummary>,
    selected: Option<(String, u32)>,
    body: Option<Body>,
    search: String,
    remote_allowed: HashSet<(String, u32)>,
    engine_status: String,
    /// A message opened from a file (`mailclient message.eml`), shown without an account.
    standalone: Option<(MessageSummary, Vec<u8>)>,
}

pub struct Controller {
    window: slint::Weak<AppWindow>,
    state: RefCell<State>,
    folder_model: Rc<VecModel<FolderItem>>,
    message_model: Rc<VecModel<MessageItem>>,
    attachment_model: Rc<VecModel<AttachmentItem>>,
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
            composer: RefCell::new(None),
            next_send_id: std::cell::Cell::new(1),
            #[cfg(feature = "servo")]
            viewer: RefCell::new(None),
        });
        CONTROLLER.with(|c| *c.borrow_mut() = Some(controller.clone()));

        window.set_folders(ModelRc::from(controller.folder_model.clone()));
        window.set_messages(ModelRc::from(controller.message_model.clone()));
        window.set_msg_attachments(ModelRc::from(controller.attachment_model.clone()));
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

        match (config::load_account()?, eml) {
            (_, Some(path)) => controller.open_file(&path)?,
            (Some(account), None) => controller.start(account)?,
            (None, None) => window.set_needs_setup(true),
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
        on!(on_folder_selected, |c, index| c.select_folder(index as usize));
        on!(on_message_selected, |c, index| c.select_message(index as usize));
        on!(on_search, |c, text| c.search(&text));
        on!(on_refresh, |c| c.send(Command::SyncAll));
        on!(on_compose, |c| c.open_composer(Draft::default(), None));
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

    fn send(&self, command: Command) {
        if let Some(engine) = &self.state.borrow().engine {
            engine.send(command);
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
        };
        if !account.email.contains('@') {
            return Err(anyhow!("enter a valid email address"));
        }
        if account.imap_host.is_empty() || account.smtp_host.is_empty() {
            return Err(anyhow!("enter the IMAP and SMTP servers"));
        }
        if data.password.is_empty() {
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
                        let imap = mailcore::imap::Imap::connect(&account, &password).await?;
                        imap.logout().await;
                        anyhow::Ok(())
                    })
                })
                .and_then(|()| account.store_password(&password))
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
        self.window().set_account_label(format::person(&account.name, &account.email).into());
        self.window().set_has_account(true);
        let engine = Engine::start(account.clone(), db, |event| with_controller(move |c| c.on_event(event)));
        {
            let mut state = self.state.borrow_mut();
            state.account = Some(account);
            state.store = Some(store);
            state.engine = Some(engine);
        }
        // Show the cache immediately; the engine syncs in the background.
        self.reload_folders();
        Ok(())
    }

    // ---- engine events ----

    fn on_event(&self, event: Event) {
        match event {
            Event::Status(s) => self.set_status(s),
            Event::Error(e) => self.set_status(format!("⚠ {e}")),
            Event::Online(online) => self.window().set_online(online),
            Event::FoldersChanged => self.reload_folders(),
            Event::MessagesChanged(folder) => {
                let state = self.state.borrow();
                let relevant = !state.search.is_empty() || state.current_folder.as_deref() == Some(folder.as_str());
                drop(state);
                if relevant {
                    self.reload_messages();
                }
            }
            Event::BodyReady { folder, uid } => {
                if self.state.borrow().selected == Some((folder, uid)) {
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
        let folders = match self.state.borrow().store.as_ref().map(|s| s.folders()) {
            Some(Ok(f)) => f,
            Some(Err(e)) => return self.show_error(&e),
            None => return,
        };
        let items: Vec<FolderItem> = folders
            .iter()
            .map(|f| FolderItem {
                name: f.name.clone().into(),
                role: f.role.as_str().into(),
                label: f.display_name().into(),
                unread: if matches!(f.role, Role::Sent | Role::Drafts | Role::Trash | Role::Junk) { 0 } else { f.unread as i32 },
                depth: f.depth() as i32,
            })
            .collect();
        let needs_initial_folder = {
            let mut state = self.state.borrow_mut();
            state.folders = folders;
            if state.current_folder.as_ref().is_some_and(|c| !state.folders.iter().any(|f| &f.name == c)) {
                state.current_folder = None;
            }
            state.current_folder.is_none() && !state.folders.is_empty()
        };
        self.folder_model.set_vec(items);
        if needs_initial_folder {
            let inbox = {
                let state = self.state.borrow();
                state.folders.iter().position(|f| f.role == Role::Inbox).unwrap_or(0)
            };
            self.select_folder(inbox);
        } else {
            self.update_folder_selection();
        }
    }

    fn update_folder_selection(&self) {
        let state = self.state.borrow();
        let index = state.folders.iter().position(|f| Some(&f.name) == state.current_folder.as_ref());
        let window = self.window();
        window.set_current_folder(index.map(|i| i as i32).unwrap_or(-1));
        if state.search.is_empty() {
            let title = index.map(|i| state.folders[i].display_name()).unwrap_or_default();
            window.set_list_title(title.into());
        }
    }

    fn select_folder(&self, index: usize) {
        let Some(name) = self.state.borrow().folders.get(index).map(|f| f.name.clone()) else { return };
        {
            let mut state = self.state.borrow_mut();
            state.current_folder = Some(name.clone());
            state.search.clear();
        }
        self.window().set_searching(false);
        self.update_folder_selection();
        self.reload_messages();
        self.send(Command::OpenFolder(name));
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
        let result = {
            let state = self.state.borrow();
            let Some(store) = state.store.as_ref() else { return };
            if !state.search.is_empty() {
                store.search(&state.search, 500)
            } else if let Some(folder) = &state.current_folder {
                store.messages(folder, LIST_LIMIT)
            } else {
                Ok(Vec::new())
            }
        };
        let messages = match result {
            Ok(m) => m,
            Err(e) => return self.show_error(&e),
        };
        let items: Vec<MessageItem> = {
            let state = self.state.borrow();
            let show_recipients = |folder: &str| {
                state.folders.iter().any(|f| f.name == folder && matches!(f.role, Role::Sent | Role::Drafts))
            };
            messages
                .iter()
                .map(|m| MessageItem {
                    folder: m.folder.clone().into(),
                    uid: m.uid as i32,
                    sender: if show_recipients(&m.folder) {
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
                    seen: m.seen,
                    flagged: m.flagged,
                    answered: m.answered,
                    attachments: m.has_attachments,
                })
                .collect()
        };
        self.message_model.set_vec(items);
        let selected_index = {
            let mut state = self.state.borrow_mut();
            state.messages = messages;
            let index = state
                .selected
                .as_ref()
                .and_then(|(f, u)| state.messages.iter().position(|m| &m.folder == f && m.uid == *u));
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
            state.selected = Some((String::new(), 0));
            state.standalone = Some((summary, raw));
        }
        let window = self.window();
        window.set_list_title(path.file_name().unwrap_or_default().to_string_lossy().as_ref().into());
        window.set_account_label("Viewing file — no account".into());
        self.render_selected();
        Ok(())
    }

    fn raw_of(&self, m: &MessageSummary) -> Result<Option<Vec<u8>>> {
        let state = self.state.borrow();
        if let Some((_, raw)) = &state.standalone {
            return Ok(Some(raw.clone()));
        }
        match state.store.as_ref() {
            Some(store) => store.raw(&m.folder, m.uid),
            None => Ok(None),
        }
    }

    fn selected_summary(&self) -> Option<MessageSummary> {
        let state = self.state.borrow();
        if let Some((summary, _)) = &state.standalone {
            return Some(summary.clone());
        }
        let (folder, uid) = state.selected.as_ref()?;
        state.messages.iter().find(|m| &m.folder == folder && m.uid == *uid).cloned()
    }

    fn select_message(&self, index: usize) {
        let Some(summary) = self.state.borrow().messages.get(index).cloned() else { return };
        let key = (summary.folder.clone(), summary.uid);
        if self.state.borrow().selected.as_ref() == Some(&key) {
            return;
        }
        {
            let mut state = self.state.borrow_mut();
            state.selected = Some(key);
            state.body = None;
        }
        self.window().set_current_message(index as i32);
        if !summary.seen {
            self.apply(&summary, PendingOp::SetSeen(true));
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
        #[cfg(feature = "servo")]
        if let Some(view) = self.viewer.borrow().as_ref() {
            view.clear();
        }
    }

    fn update_reader_header(&self) {
        let Some(m) = self.selected_summary() else { return };
        let window = self.window();
        window.set_has_message(true);
        window.set_msg_subject(if m.subject.is_empty() { "(no subject)".into() } else { m.subject.clone().into() });
        window.set_msg_from(format::person(&m.from_name, &m.from_addr).into());
        window.set_msg_to(m.to.clone().into());
        window.set_msg_cc(m.cc.clone().into());
        window.set_msg_date(format::full_date(m.date).into());
        window.set_msg_seen(m.seen);
        window.set_msg_flagged(m.flagged);
    }

    /// Shows the selected message, downloading its body first if needed.
    fn render_selected(&self) {
        let Some(summary) = self.selected_summary() else { return };
        self.update_reader_header();
        let window = self.window();
        let raw = match self.raw_of(&summary) {
            Ok(Some(raw)) => raw,
            Ok(None) => {
                window.set_msg_loading(true);
                window.set_msg_text("".into());
                window.set_msg_remote_blocked(false);
                self.attachment_model.set_vec(Vec::new());
                #[cfg(feature = "servo")]
                if let Some(view) = self.viewer.borrow().as_ref() {
                    view.clear();
                }
                self.send(Command::FetchBody { folder: summary.folder.clone(), uid: summary.uid });
                return;
            }
            Err(e) => return self.show_error(&e),
        };
        window.set_msg_loading(false);
        let body = mime::body_from_raw(&raw);
        let allow_remote = self.state.borrow().remote_allowed.contains(&(summary.folder.clone(), summary.uid));
        window.set_msg_remote_blocked(body.has_remote_content && !allow_remote && cfg!(feature = "servo"));
        window.set_msg_text(body.text.clone().into());
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
            view.show(crate::webview::delegate::MessageContent {
                document: mime::render_document(&body),
                csp: mime::content_security_policy(allow_remote),
                inline_parts: body.inline_parts.clone(),
                allow_remote,
            });
        }
        self.state.borrow_mut().body = Some(body);
    }

    fn load_remote_images(&self) {
        let Some(key) = self.state.borrow().selected.clone() else { return };
        self.state.borrow_mut().remote_allowed.insert(key);
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
            self.open_composer(draft, None);
        } else if url.starts_with("https://") || url.starts_with("http://") {
            // shortcut: Android needs an ACTION_VIEW intent via JNI; not wired up yet.
            #[cfg(not(target_os = "android"))]
            if let Err(e) = open::that_detached(url) {
                self.show_error(&anyhow!("opening {url}: {e}"));
            }
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
        let Some(summary) = self.selected_summary() else { return };
        let Ok(Some(raw)) = self.raw_of(&summary) else { return };
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
    fn apply(&self, summary: &MessageSummary, op: PendingOp) {
        let result = self.state.borrow_mut().store.as_mut().map(|s| s.apply_local(&summary.folder, summary.uid, &op));
        if let Some(Err(e)) = result {
            return self.show_error(&e);
        }
        self.reload_messages();
        self.reload_folders();
        self.send(Command::Flush);
    }

    fn toggle_selected(&self, flag: bool) {
        let Some(m) = self.selected_summary() else { return };
        let op = if flag { PendingOp::SetFlagged(!m.flagged) } else { PendingOp::SetSeen(!m.seen) };
        self.apply(&m, op);
    }

    fn remove_selected(&self, archive: bool) {
        let Some(m) = self.selected_summary() else { return };
        let op = {
            let state = self.state.borrow();
            let Some(store) = state.store.as_ref() else { return };
            if archive { engine::archive_op(store, &m.folder) } else { engine::delete_op(store, &m.folder) }
        };
        let op = match op {
            Ok(op) => op,
            Err(e) => return self.show_error(&e),
        };
        // Select the message that takes this one's place, like most mail clients.
        let index = self.window().get_current_message().max(0) as usize;
        self.apply(&m, op);
        let count = self.state.borrow().messages.len();
        if count > 0 {
            self.select_message(index.min(count - 1));
        }
    }

    // ---- composing ----

    fn reply(&self, kind: ReplyKind) {
        let Some(m) = self.selected_summary() else { return };
        let me = self.state.borrow().account.as_ref().map(|a| a.email.clone()).unwrap_or_default();
        let text = self.state.borrow().body.as_ref().map(|b| b.text.clone()).unwrap_or_default();
        let replied_to = (!matches!(kind, ReplyKind::Forward)).then(|| (m.folder.clone(), m.uid));
        let forwarding = matches!(kind, ReplyKind::Forward);
        let mut draft = mime::reply_draft(kind, &m, &text, &me);
        if forwarding {
            // Forward attachments too: write them to a temp dir and attach from there.
            if let Err(e) = self.attach_original_files(&m, &mut draft) {
                self.show_error(&e);
            }
        }
        self.open_composer(draft, replied_to);
    }

    fn attach_original_files(&self, m: &MessageSummary, draft: &mut Draft) -> Result<()> {
        let Some(attachments) = self.state.borrow().body.as_ref().map(|b| b.attachments.clone()) else { return Ok(()) };
        if attachments.is_empty() {
            return Ok(());
        }
        let raw = self.raw_of(m)?.ok_or_else(|| anyhow!("message not downloaded"))?;
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

    fn open_composer(&self, draft: Draft, replied_to: Option<(String, u32)>) {
        if self.state.borrow().engine.is_none() {
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
                c.send(Command::Send { id, draft, replied_to });
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
