//! Background sync engine. Runs on its own thread with a tokio runtime; the UI
//! talks to it through [`Command`]s and receives [`Event`]s via a callback.
//!
//! Local changes (flags, moves) are written to the cache by the UI first, which
//! keeps the UI instant, and queued in `pending_ops`. The engine pushes the
//! queue to the server before every sync, so a sync never resurrects a message
//! that was moved locally.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, anyhow};
use tokio::sync::mpsc;

use crate::auth::Credentials;
use crate::backend::Backend;
use crate::config::AccountConfig;
use crate::imap::Imap;
use crate::mime::{self, Draft};
use crate::store::{MessageSummary, PendingOp, Role, Store};

/// Messages fetched when a folder is synced for the first time.
const INITIAL_WINDOW: u32 = 500;
/// Messages fetched per request for older mail (scrolling to the end of a list).
const OLDER_BATCH: u32 = 500;
/// Bodies downloaded ahead of time per sync, for offline reading and search.
const PREFETCH_BODIES: usize = 40;
/// Bodies kept per folder, newest first; older ones keep their search text and download again when opened.
const KEEP_BODIES: usize = 2000;
/// Server search hits per folder, newest first, whose headers get cached.
const SEARCH_HITS: usize = 200;
/// Bodies per FETCH: each batch is held in memory whole, and attachments can be large.
const BODY_BATCH: usize = 5;
const POLL_INTERVAL: Duration = Duration::from_secs(5 * 60);
const IDLE_TIMEOUT: Duration = Duration::from_secs(25 * 60);
const CALENDAR_INTERVAL: Duration = Duration::from_secs(15 * 60);

// Commands are few and short-lived; boxing the event variants would buy nothing.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone)]
pub enum Command {
    /// Refresh the folder list, the inbox, Sent and the active folder.
    SyncAll,
    /// Make `folder` the active folder and sync it.
    OpenFolder(String),
    /// Fetch the headers of messages older than any synced in `folder`.
    FetchOlder(String),
    /// Search the server, in `folder` or where most mail lives; answered with [`Event::SearchHits`].
    Search { query: String, folder: Option<String> },
    /// Download a message body that is not cached yet.
    FetchBody { folder: String, uid: u32 },
    /// Push locally queued changes to the server.
    Flush,
    /// `id` is echoed back in [`Event::Sent`] / [`Event::SendFailed`].
    Send {
        id: u64,
        draft: Draft,
        replied_to: Option<(String, u32)>,
    },
    /// Files the draft in Drafts, replacing older copies with its Message-ID.
    /// `id` is echoed back in [`Event::DraftSaved`] / [`Event::SendFailed`].
    SaveDraft { id: u64, draft: Draft },
    /// Removes the saved copies of a discarded draft.
    DeleteDraft { message_id: String },
    /// Fetches the account's calendars.
    SyncCalendars,
    /// Puts a VCALENDAR into the account's calendar; `invitation`: it arrived by mail.
    SaveEvent { ics: String, invitation: bool },
    /// Changes a stored event to `new`'s values, telling its attendees.
    UpdateEvent { target: crate::store::StoredEvent, new: crate::ical::Event },
    /// Deletes a stored event (a repeating one as a whole), telling its attendees.
    DeleteEvent { target: crate::store::StoredEvent },
    /// Finishes the queued commands, logs out and ends the engine thread. Sent on drop.
    Stop,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    Status(String),
    Error(String),
    Online(bool),
    FoldersChanged,
    MessagesChanged(String),
    /// Unread messages that arrived in the inbox since the last sync (not on the first sync).
    NewMail(Vec<MessageSummary>),
    BodyReady { folder: String, uid: u32 },
    /// (folder, uid) of the messages a server search for `query` found, all cached now.
    SearchHits { query: String, hits: Vec<(String, u32)> },
    Sent(u64),
    /// A send or a draft save failed.
    SendFailed(u64, String),
    DraftSaved(u64),
    CalendarChanged,
    /// A calendar sync or change failed; the next success clears it.
    CalendarFailed(String),
}

pub struct Engine {
    tx: mpsc::UnboundedSender<Command>,
}

impl Engine {
    pub fn start(
        account: AccountConfig,
        db_path: PathBuf,
        on_event: impl Fn(Event) + Send + Sync + 'static,
    ) -> Engine {
        let (tx, rx) = mpsc::unbounded_channel();
        let idle_tx = tx.clone();
        std::thread::Builder::new()
            .name("mail-engine".into())
            .spawn(move || {
                // Network-bound with one connection at a time: one thread per account is plenty.
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("tokio runtime");
                let on_event: Arc<dyn Fn(Event) + Send + Sync> = Arc::new(on_event);
                runtime.block_on(async move {
                    let creds = match Credentials::load(&account) {
                        Ok(c) => Arc::new(c),
                        Err(e) => {
                            // Without a password the account cannot connect: the UI shows why.
                            on_event(Event::Online(false));
                            on_event(Event::Error(e.to_string()));
                            return;
                        }
                    };
                    let store = match Store::open(&db_path) {
                        Ok(s) => s,
                        Err(e) => {
                            on_event(Event::Error(format!("opening cache: {e}")));
                            return;
                        }
                    };
                    // The only backend so far; a protocol choice in AccountConfig would branch here.
                    tokio::spawn(idle_loop::<Imap>(account.clone(), creds.clone(), idle_tx));
                    Worker::<Imap> {
                        account,
                        creds,
                        store,
                        conn: None,
                        active: None,
                        on_event,
                    }
                    .run(rx)
                    .await;
                });
            })
            .expect("spawning engine thread");
        Engine { tx }
    }

    pub fn send(&self, command: Command) {
        let _ = self.tx.send(command);
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        // The IDLE task holds a sender too, so the channel never closes by itself.
        self.send(Command::Stop);
    }
}

/// Syncs the inbox once, without an engine thread: a background check for new mail
/// (Android's periodic job). Returns the unread messages that arrived since the last sync.
pub async fn check_inbox(account: &AccountConfig, db_path: &std::path::Path) -> Result<Vec<MessageSummary>> {
    let store = Store::open(db_path)?;
    // Never synced: the app's first sync fills the cache, without notifying.
    let Some(inbox) = store.folder_by_role(Role::Inbox)? else { return Ok(Vec::new()) };
    let arrived = Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = arrived.clone();
    let mut worker = Worker::<Imap> {
        account: account.clone(),
        creds: Arc::new(Credentials::load(account)?),
        store,
        conn: None,
        active: None,
        on_event: Arc::new(move |e| {
            if let Event::NewMail(m) = e {
                sink.lock().unwrap().extend(m);
            }
        }),
    };
    let result = worker.sync_folder(&inbox).await;
    if let Some(conn) = worker.conn.take() {
        conn.logout().await;
    }
    result?;
    Ok(std::mem::take(&mut *arrived.lock().unwrap()))
}

/// What deleting a message in `folder` means: move to Trash, or erase when it
/// is already in Trash (or there is no Trash folder).
pub fn delete_op(store: &Store, folder: &str) -> Result<PendingOp> {
    if store.folder_role(folder)? == Role::Trash {
        return Ok(PendingOp::Expunge);
    }
    Ok(match store.folder_by_role(Role::Trash)? {
        Some(trash) => PendingOp::Move(trash),
        None => PendingOp::Expunge,
    })
}

/// What moving `m` to `dest` means. Gmail's All Mail holds every message once, labelled with the
/// folders it shows in: from there a move takes the labels of those folders off and puts `dest`'s
/// on (none for All Mail itself: archiving). Elsewhere, and into Trash or Spam (outside All Mail),
/// it is a plain move.
pub fn move_op(store: &Store, gmail: bool, m: &MessageSummary, dest: &str) -> Result<PendingOp> {
    if !gmail || store.folder_role(&m.folder)? != Role::All || matches!(store.folder_role(dest)?, Role::Trash | Role::Junk) {
        return Ok(PendingOp::Move(dest.to_string()));
    }
    let add: Vec<String> = label_of_folder(store, dest)?.into_iter().collect();
    let remove = store
        .labels(&m.folder, m.uid)?
        .into_iter()
        // Where it shows: the inbox and the user's labels, not Important or Starred.
        .filter(|l| (l == "\\Inbox" || !l.starts_with('\\')) && !add.contains(l))
        .collect();
    Ok(PendingOp::Relabel { add, remove })
}

/// The Gmail label that puts a message in `folder`; None for All Mail.
fn label_of_folder(store: &Store, folder: &str) -> Result<Option<String>> {
    Ok(match store.folder_role(folder)? {
        Role::All => None,
        Role::Inbox => Some("\\Inbox".into()),
        Role::Sent => Some("\\Sent".into()),
        Role::Drafts => Some("\\Draft".into()),
        Role::Flagged => Some("\\Starred".into()),
        _ => Some(folder.to_string()),
    })
}

/// The folder showing the messages with this Gmail label, if there is one.
fn folder_of_label(store: &Store, label: &str) -> Result<Option<String>> {
    let role = match label {
        "\\Inbox" => Role::Inbox,
        "\\Sent" => Role::Sent,
        "\\Draft" => Role::Drafts,
        "\\Starred" => Role::Flagged,
        l if l.starts_with('\\') => return Ok(None),
        l => return Ok(Some(l.to_string())),
    };
    store.folder_by_role(role)
}

pub fn archive_op(store: &Store, folder: &str) -> Result<PendingOp> {
    let dest = match store.folder_by_role(Role::Archive)? {
        Some(a) => a,
        None => store
            .folder_by_role(Role::All)?
            .ok_or_else(|| anyhow!("no Archive folder on this account"))?,
    };
    // Gmail: archiving from All Mail takes its labels off (see `move_op`).
    if dest == folder && store.folder_role(folder)? != Role::All {
        return Err(anyhow!("message is already archived"));
    }
    Ok(PendingOp::Move(dest))
}

struct Worker<B: Backend> {
    account: AccountConfig,
    creds: Arc<Credentials>,
    store: Store,
    conn: Option<B>,
    active: Option<String>,
    on_event: Arc<dyn Fn(Event) + Send + Sync>,
}

impl<B: Backend> Worker<B> {
    fn emit(&self, event: Event) {
        (self.on_event)(event);
    }

    async fn run(mut self, mut rx: mpsc::UnboundedReceiver<Command>) {
        let mut poll = tokio::time::interval(POLL_INTERVAL);
        let mut calendar_poll = tokio::time::interval(CALENDAR_INTERVAL);
        // A command taken from the queue while collapsing a burst, handled next.
        let mut queued = None;
        loop {
            let mut command = match queued.take() {
                Some(c) => c,
                None => tokio::select! {
                    c = rx.recv() => match c {
                        Some(c) => c,
                        None => break,
                    },
                    _ = poll.tick() => Command::SyncAll,
                    _ = calendar_poll.tick() => Command::SyncCalendars,
                },
            };
            if matches!(command, Command::Stop) {
                break;
            }
            // Collapse bursts: syncs (IDLE notifications plus a poll tick), and searches typed
            // faster than the server answers, where the last one wins.
            if matches!(command, Command::SyncAll | Command::Search { .. }) {
                while let Ok(next) = rx.try_recv() {
                    match (&command, next) {
                        (Command::SyncAll, Command::SyncAll) => {}
                        (Command::Search { .. }, next @ Command::Search { .. }) => command = next,
                        (_, next) => {
                            queued = Some(next);
                            break;
                        }
                    }
                }
            }
            self.execute(command).await;
        }
        if let Some(conn) = self.conn.take() {
            conn.logout().await;
        }
    }

    /// Runs one command, retrying once on a dropped connection, and reports a failure as events.
    async fn execute(&mut self, command: Command) {
        // The variant only: drafts and events are private.
        let name = format!("{command:?}").split([' ', '(', '{']).next().unwrap_or_default().to_string();
        let started = std::time::Instant::now();
        log::debug!("engine: {name} started");
        // HTTP, not the IMAP session: a failure says nothing about the mail connection.
        if matches!(command, Command::SyncCalendars | Command::SaveEvent { .. } | Command::UpdateEvent { .. } | Command::DeleteEvent { .. }) {
            if let Err(e) = self.calendar(command).await {
                log::warn!("calendar: {e:#}");
                self.emit(Event::CalendarFailed(format!("{e:#}")));
            }
            log::debug!("engine: {name} done in {:?}", started.elapsed());
            return;
        }
        let draft_id = match &command {
            Command::SaveDraft { id, .. } => Some(*id),
            _ => None,
        };
        // Sending is never retried: SMTP may already have delivered it.
        let retry = (!matches!(command, Command::Send { .. })).then(|| command.clone());
        let mut result = self.handle(command).await;
        if let (Err(e), Some(command)) = (&result, retry)
            && !B::is_rejection(e)
        {
            // Usually a connection the server dropped while idle: reconnect and try again once.
            log::info!("engine: {e:#}; reconnecting");
            self.conn = None;
            result = self.handle(command).await;
        }
        log::debug!("engine: {name} done in {:?}", started.elapsed());
        if let Err(e) = result {
            log::warn!("engine: {e:#}");
            if let Some(id) = draft_id {
                self.emit(Event::SendFailed(id, format!("Draft not saved: {e:#}")));
            }
            if !B::is_rejection(&e) {
                // Assume the connection is broken; reconnect on next use.
                self.conn = None;
                self.emit(Event::Online(false));
            }
            self.emit(Event::Error(format!("{e:#}")));
        }
    }

    async fn conn(&mut self) -> Result<&mut B> {
        if self.conn.is_none() {
            self.emit(Event::Status(format!(
                "Connecting to {}…",
                self.account.imap_host
            )));
            let secret = self.creds.secret().await?;
            self.conn = Some(B::connect(&self.account, &secret).await?);
            self.emit(Event::Online(true));
        }
        Ok(self.conn.as_mut().unwrap())
    }

    async fn handle(&mut self, command: Command) -> Result<()> {
        match command {
            Command::SyncAll => {
                self.flush().await?;
                self.emit(Event::Status("Syncing folders…".into()));
                let folders = self.conn().await?.list_folders().await?;
                if self.store.sync_folders(&folders)? {
                    self.emit(Event::FoldersChanged);
                }
                let mut targets = Vec::new();
                // Sent too: the user's replies belong in the inbox's conversations. Gmail: All Mail
                // as well, where a search of all folders looks, with the labels saying where each is.
                let all_mail = self.account.is_gmail().then_some(Role::All);
                for role in [Role::Inbox, Role::Sent].into_iter().chain(all_mail) {
                    targets.extend(self.store.folder_by_role(role)?);
                }
                if let Some(active) = self.active.clone().filter(|a| !targets.contains(a)) {
                    targets.push(active);
                }
                for folder in targets {
                    self.sync_folder(&folder).await?;
                }
                self.emit(Event::Status(String::new()));
            }
            Command::OpenFolder(folder) => {
                self.active = Some(folder.clone());
                self.flush().await?;
                self.sync_folder(&folder).await?;
                self.emit(Event::Status(String::new()));
            }
            Command::FetchOlder(folder) => {
                // Not synced yet: the first sync fetches the newest messages.
                let Some(from) = self.store.synced_from(&folder)? else { return Ok(()) };
                self.emit(Event::Status(format!("Fetching older messages in {folder}…")));
                let older = self.conn().await?.fetch_older(&folder, from, OLDER_BATCH).await?;
                if let Some(oldest) = older.iter().map(|m| m.uid).min() {
                    self.store.upsert_messages(&older)?;
                    self.store.set_synced_from(&folder, oldest)?;
                    self.emit(Event::MessagesChanged(folder));
                    self.emit(Event::FoldersChanged);
                }
                self.emit(Event::Status(String::new()));
            }
            Command::Search { query, folder } => self.search(query, folder).await?,
            Command::FetchBody { folder, uid } => {
                self.download_bodies(&folder, &[uid]).await?;
                self.emit(Event::BodyReady { folder, uid });
            }
            Command::Flush => self.flush().await?,
            Command::Send {
                id,
                draft,
                replied_to,
            } => {
                let sent = std::cell::Cell::new(false);
                if let Err(e) = self.send(id, &draft, replied_to, &sent).await {
                    if sent.get() {
                        // Delivered, but filing it in Sent failed: not a send failure.
                        return Err(
                            e.context("message sent, but saving it to the Sent folder failed")
                        );
                    }
                    self.emit(Event::SendFailed(id, format!("{e:#}")));
                    self.emit(Event::Status(String::new()));
                }
            }
            Command::SaveDraft { id, draft } => {
                self.save_draft(&draft).await?;
                self.emit(Event::DraftSaved(id));
            }
            Command::DeleteDraft { message_id } => {
                if let Some(drafts) = self.store.folder_by_role(Role::Drafts)? {
                    self.remove_drafts(&drafts, &message_id, 0).await?;
                }
            }
            Command::Stop | Command::SyncCalendars | Command::SaveEvent { .. } | Command::UpdateEvent { .. } | Command::DeleteEvent { .. } => {}
        }
        Ok(())
    }

    async fn calendar(&mut self, command: Command) -> Result<()> {
        let notices = match &command {
            Command::SaveEvent { ics, invitation } => {
                crate::calendar::save(&self.account, &self.creds, &mut self.store, ics, *invitation).await?;
                Vec::new()
            }
            Command::UpdateEvent { target, new } => crate::calendar::update(&self.account, &self.creds, &mut self.store, target, new).await?,
            Command::DeleteEvent { target } => crate::calendar::delete(&self.account, &self.creds, &mut self.store, target).await?,
            _ => Vec::new(),
        };
        if !matches!(command, Command::SyncCalendars) {
            self.emit(Event::CalendarChanged);
        }
        for notice in notices {
            let quoted = |a: &crate::ical::Attendee| match a.name.as_str() {
                "" => a.email.clone(),
                name => format!("\"{}\" <{}>", name.replace('"', ""), a.email),
            };
            let e = &notice.event;
            let (subject, what) = match notice.method {
                "CANCEL" => (format!("Cancelled: {}", e.summary), "was cancelled"),
                _ => (format!("Updated invitation: {}", e.summary), "was changed"),
            };
            let draft = Draft {
                to: e.attendees.iter().map(quoted).collect::<Vec<_>>().join(", "),
                subject,
                body: format!("{} {what}.\n\n{}\n", e.summary, e.description),
                message_id: mime::new_message_id(&self.account.email),
                calendar: Some((notice.method.to_string(), notice.ics)),
                ..Default::default()
            };
            // Not retried, like every send: the server change already happened.
            self.send(0, &draft, None, &std::cell::Cell::new(false)).await?;
        }
        crate::calendar::sync(&self.account, &self.creds, &mut self.store).await?;
        self.emit(Event::CalendarChanged);
        Ok(())
    }

    /// The Drafts folder, created when the server has none.
    async fn drafts_folder(&mut self) -> Result<String> {
        for create in [false, true] {
            if let Some(drafts) = self.store.folder_by_role(Role::Drafts)? {
                return Ok(drafts);
            }
            if create {
                self.conn().await?.create_folder("Drafts").await?;
            }
            let folders = self.conn().await?.list_folders().await?;
            self.store.sync_folders(&folders)?;
            self.emit(Event::FoldersChanged);
        }
        self.store.folder_by_role(Role::Drafts)?.ok_or_else(|| anyhow!("no Drafts folder on this account"))
    }

    async fn save_draft(&mut self, draft: &Draft) -> Result<()> {
        let mut draft = draft.clone();
        draft.from_name = self.account.name.clone();
        draft.from_addr = self.account.email.clone();
        let built = mime::build_draft(&draft)?;
        let drafts = self.drafts_folder().await?;
        self.conn().await?.save_draft(&drafts, &built.raw).await?;
        // The copy just saved has the highest UID; older ones go.
        self.remove_drafts(&drafts, &draft.message_id, 1).await?;
        if self.active.as_deref() == Some(drafts.as_str()) {
            self.sync_folder(&drafts).await?;
        }
        Ok(())
    }

    /// Erases the copies of a draft in `folder`, all but the newest `keep`.
    async fn remove_drafts(&mut self, folder: &str, message_id: &str, keep: usize) -> Result<()> {
        if message_id.is_empty() {
            return Ok(());
        }
        let uids = self.conn().await?.find_message_id(folder, message_id).await?;
        let old = &uids[..uids.len().saturating_sub(keep)];
        for uid in old {
            self.conn().await?.apply(folder, *uid, &PendingOp::Expunge).await?;
        }
        if !old.is_empty() {
            self.store.delete_messages(folder, old)?;
            self.emit(Event::MessagesChanged(folder.to_string()));
        }
        Ok(())
    }

    async fn flush(&mut self) -> Result<()> {
        let (mut touched, mut done) = (Vec::new(), Vec::new());
        let result = self.apply_pending(&mut touched, &mut done).await;
        // Whatever the server did is mirrored by syncing those folders now, not by guessing: Gmail
        // keeps a message moved out of All Mail there and drops a trashed one from every label.
        // Moved copies have new UIDs in their destination. Not announced as new mail: the user
        // put them there. (After a connection error the next sync does it.)
        if result.is_ok() {
            touched.sort();
            touched.dedup();
            for folder in touched {
                if let Err(e) = self.sync_folder_as(&folder, false).await {
                    log::warn!("cannot sync {folder} after moving mail: {e:#}");
                }
            }
        }
        // Only now: until its folder is synced, a moved message stays hidden where it was.
        for id in done {
            self.store.remove_pending_op(id)?;
        }
        result
    }

    /// Sends the queued changes. A sent move or delete adds its id to `done` and the folders it
    /// may have changed to `touched`; the rest are settled here.
    async fn apply_pending(&mut self, touched: &mut Vec<String>, done: &mut Vec<i64>) -> Result<()> {
        let ops = self.store.pending_ops()?;
        for (id, folder, uid, op) in ops {
            // Before the change: where the message is (Gmail: also All Mail and its labels).
            let copies = match op {
                PendingOp::Move(_) | PendingOp::Expunge => self.store.folders_with_copies(&folder, uid)?,
                _ => Vec::new(),
            };
            let result = self.conn().await?.apply(&folder, uid, &op).await;
            match result {
                Ok(()) => {
                    if let PendingOp::Move(_) | PendingOp::Expunge = op {
                        touched.extend(copies);
                        if let PendingOp::Move(dest) = &op {
                            touched.push(dest.clone());
                        }
                        // A folder spanning the others (Gmail's All Mail) changes with them.
                        touched.extend(self.store.folder_by_role(Role::All)?);
                        done.push(id);
                    } else if let PendingOp::Relabel { add, remove } = &op {
                        // The message enters and leaves the labels' folders; its own copy gets the new labels.
                        touched.push(folder.clone());
                        for label in add.iter().chain(remove) {
                            touched.extend(folder_of_label(&self.store, label)?);
                        }
                        done.push(id);
                    } else {
                        self.store.remove_pending_op(id)?;
                    }
                }
                // The server refused (message gone, folder missing): retrying won't help.
                Err(e) if B::is_rejection(&e) => {
                    log::warn!("dropping {op:?} on {folder}/{uid}: {e:#}");
                    // Undo the local change: the server's copy is the truth.
                    if let PendingOp::Move(_) | PendingOp::Expunge = op {
                        // A connection error leaves the op queued, so the restore is retried with it.
                        match self.conn().await?.fetch_headers(&folder, &[uid]).await {
                            Ok(headers) => self.store.upsert_messages(&headers)?,
                            Err(e) if B::is_rejection(&e) => log::warn!("cannot restore {folder}/{uid}: {e:#}"),
                            Err(e) => return Err(e),
                        }
                    } else {
                        // The next sync then fetches all flags instead of trusting the unchanged mark.
                        self.store.set_sync_mark(&folder, None)?;
                    }
                    self.store.remove_pending_op(id)?;
                    self.emit(Event::MessagesChanged(folder));
                }
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    async fn sync_folder(&mut self, folder: &str) -> Result<()> {
        self.sync_folder_as(folder, true).await
    }

    /// `announce`: unread arrivals in the inbox are reported as [`Event::NewMail`].
    async fn sync_folder_as(&mut self, folder: &str, announce: bool) -> Result<()> {
        self.emit(Event::Status(format!("Syncing {folder}…")));
        let state = self.conn().await?.select(folder).await?;
        // The UI reloads on every change event; a sync that found nothing new sends none.
        let mut changed = false;
        if let Some(v) = state.uid_validity
            && self.store.check_uidvalidity(folder, v)?
        {
            log::info!("UIDVALIDITY of {folder} changed, cache reset");
            changed = true;
        }

        // An arrival, flag change or expunge moves one of the two (an expunge may only lower the count).
        let mark = state.highest_modseq.map(|m| (m, state.exists));
        match self.store.synced_from(folder)? {
            Some(_) if mark.is_some() && self.store.sync_mark(folder)? == mark => {}
            Some(from) => {
                let cached = self.store.uids(folder)?;
                let max = cached.iter().copied().max().unwrap_or(from.saturating_sub(1));
                let new = self.conn().await?.fetch_new(folder, max).await?;
                self.store.upsert_messages(&new)?;
                changed |= !new.is_empty();
                let unread: Vec<MessageSummary> = new.into_iter().filter(|m| !m.seen).collect();
                if announce && !unread.is_empty() && self.store.folder_role(folder)? == Role::Inbox {
                    self.emit(Event::NewMail(unread));
                }
                // The synced window, plus search hits below it one by one.
                let mut ranges = vec![(from, max.max(from))];
                ranges.extend(cached.iter().filter(|uid| **uid < from).map(|uid| (*uid, *uid)));
                let flags = self.conn().await?.fetch_flags(folder, &ranges).await?;
                let on_server: std::collections::HashSet<u32> = flags.iter().map(|f| f.uid).collect();
                let gone: Vec<u32> = cached.into_iter().filter(|uid| !on_server.contains(uid)).collect();
                self.store.delete_messages(folder, &gone)?;
                changed |= !gone.is_empty();
                changed |= self.store.update_flags(folder, &flags)?;
            }
            None => {
                let latest = self
                    .conn()
                    .await?
                    .fetch_latest(folder, state.exists, INITIAL_WINDOW)
                    .await?;
                self.store.upsert_messages(&latest)?;
                changed |= !latest.is_empty();
                if let Some(oldest) = latest.iter().map(|m| m.uid).min() {
                    self.store.set_synced_from(folder, oldest)?;
                }
            }
        }
        self.store.set_sync_mark(folder, mark)?;
        // Gmail groups conversations itself; new mail and mail cached before the ids were kept get theirs.
        if self.account.is_gmail() {
            let missing = self.store.uids_without_thread(folder)?;
            if !missing.is_empty() {
                let threads = self.conn().await?.fetch_threads(folder, &missing).await?;
                self.store.set_threads(folder, &threads)?;
                changed |= !threads.is_empty();
            }
        }
        if changed {
            self.emit(Event::MessagesChanged(folder.to_string()));
            self.emit(Event::FoldersChanged);
        }

        let missing = self.store.uids_missing_body(folder, PREFETCH_BODIES, KEEP_BODIES)?;
        if !missing.is_empty() {
            self.download_bodies(folder, &missing).await?;
            self.emit(Event::MessagesChanged(folder.to_string()));
        }
        self.store.evict_bodies(folder, KEEP_BODIES)?;
        Ok(())
    }

    /// Searches the server and caches the headers of hits not cached yet, so the
    /// search finds mail older than the synced window and bodies not downloaded.
    async fn search(&mut self, query: String, folder: Option<String>) -> Result<()> {
        self.emit(Event::Status("Searching the server…".into()));
        // An all-mail folder (Gmail) holds everything; elsewhere, where most mail lives.
        let folders = match (folder, self.store.folder_by_role(Role::All)?) {
            (Some(folder), _) => vec![folder],
            (None, Some(all)) => vec![all],
            (None, None) => {
                let mut folders = Vec::new();
                for role in [Role::Inbox, Role::Sent, Role::Archive] {
                    folders.extend(self.store.folder_by_role(role)?);
                }
                folders.extend(self.active.clone().filter(|a| !folders.contains(a)));
                folders
            }
        };
        let mut hits = Vec::new();
        for folder in folders {
            let mut uids = self.conn().await?.search(&folder, &query).await?;
            uids.sort_unstable();
            let uids = &uids[uids.len().saturating_sub(SEARCH_HITS)..];
            let cached: std::collections::HashSet<u32> = self.store.uids(&folder)?.into_iter().collect();
            // Mail newer than the synced window is left to the next sync, which fetches from the newest cached UID up.
            let newest = match self.store.synced_from(&folder)? {
                Some(_) => cached.iter().copied().max().unwrap_or(0),
                None => u32::MAX,
            };
            let missing: Vec<u32> = uids.iter().copied().filter(|uid| !cached.contains(uid) && *uid <= newest).collect();
            let headers = self.conn().await?.fetch_headers(&folder, &missing).await?;
            self.store.upsert_messages(&headers)?;
            hits.extend(uids.iter().map(|uid| (folder.clone(), *uid)));
        }
        self.emit(Event::SearchHits { query, hits });
        self.emit(Event::Status(String::new()));
        Ok(())
    }

    async fn download_bodies(&mut self, folder: &str, uids: &[u32]) -> Result<()> {
        let mut replies = Vec::new();
        for batch in uids.chunks(BODY_BATCH) {
            let bodies = self.conn().await?.fetch_bodies(folder, batch).await?;
            for (uid, raw) in bodies {
                let body = mime::body_from_raw(&raw);
                replies.extend(body.calendar.clone().filter(|c| crate::ical::parse(c).method == "REPLY"));
                self.store.set_body(
                    folder,
                    uid,
                    &raw,
                    &body.snippet,
                    &body.text,
                    !body.attachments.is_empty(),
                )?;
            }
        }
        // Answers to the user's invitations, for calendars that do not read mail themselves.
        let mut answered = false;
        for ics in replies {
            match crate::calendar::apply_reply(&self.account, &self.creds, &mut self.store, &ics).await {
                Ok(changed) => answered |= changed,
                // A calendar problem must not stop the mail sync.
                Err(e) => log::warn!("recording an answer: {e:#}"),
            }
        }
        if answered {
            if let Err(e) = crate::calendar::sync(&self.account, &self.creds, &mut self.store).await {
                log::warn!("calendar: {e:#}");
            }
            self.emit(Event::CalendarChanged);
        }
        Ok(())
    }

    async fn send(
        &mut self,
        id: u64,
        draft: &Draft,
        replied_to: Option<(String, u32)>,
        sent: &std::cell::Cell<bool>,
    ) -> Result<()> {
        self.emit(Event::Status("Sending…".into()));
        let mut draft = draft.clone();
        draft.from_name = self.account.name.clone();
        draft.from_addr = self.account.email.clone();
        let built = mime::build_message(&draft)?;
        let secret = self.creds.secret().await?;
        B::send(&self.account, &secret, &built.raw, &built.recipients).await?;
        sent.set(true);
        self.emit(Event::Sent(id));
        self.emit(Event::Status("Message sent".into()));

        if let Some(drafts) = self.store.folder_by_role(Role::Drafts)? {
            self.remove_drafts(&drafts, &draft.message_id, 0).await?;
        }

        if let Some((folder, uid)) = replied_to {
            self.store
                .apply_local(&folder, uid, &PendingOp::SetAnswered)?;
            self.emit(Event::MessagesChanged(folder));
        }
        if !B::files_sent(&self.account)
            && let Some(sent) = self.store.folder_by_role(Role::Sent)?
        {
            self.conn().await?.save_sent(&sent, &built.raw).await?;
            if self.active.as_deref() == Some(sent.as_str()) {
                self.sync_folder(&sent).await?;
            }
        }
        self.flush().await?;
        Ok(())
    }
}

/// Keeps a second connection in IDLE on the inbox and requests a sync whenever
/// the server reports a change. Falls back to the poll timer while failing.
async fn idle_loop<B: Backend>(
    account: AccountConfig,
    creds: Arc<Credentials>,
    tx: mpsc::UnboundedSender<Command>,
) {
    let mut backoff = Duration::from_secs(10);
    loop {
        let result: Result<()> = async {
            let mut conn = B::connect(&account, &creds.secret().await?).await?;
            backoff = Duration::from_secs(10);
            loop {
                let (next, changed) = conn.wait_for_change("INBOX", IDLE_TIMEOUT).await?;
                conn = next;
                if changed {
                    // Give the server a moment to finish delivering a batch.
                    tokio::time::sleep(Duration::from_millis(500)).await;
                    if tx.send(Command::SyncAll).is_err() {
                        return Ok(());
                    }
                }
            }
        }
        .await;
        if tx.is_closed() {
            return;
        }
        if let Err(e) = result {
            log::info!("IDLE connection lost: {e:#}; retrying in {backoff:?}");
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(Duration::from_secs(300));
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::sync::Mutex;

    use super::*;
    use crate::backend::FolderState;
    use crate::store::Flags;

    /// A server refusal; any other error counts as a dropped connection.
    #[derive(Debug)]
    struct Refused;

    impl std::fmt::Display for Refused {
        fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("refused")
        }
    }

    impl std::error::Error for Refused {}

    #[derive(Default)]
    struct Server {
        folders: HashMap<String, Vec<MessageSummary>>,
        /// Returned by the next `apply` / `send` calls instead of doing anything.
        apply_errors: Vec<anyhow::Error>,
        send_errors: Vec<anyhow::Error>,
        connects: usize,
        sends: usize,
        /// Moves so far: moved copies get UIDs from 1001 up, never reusing one (as servers must).
        moves: u32,
    }

    thread_local! {
        // Per test: `#[tokio::test]` runs each test on its own thread.
        static SERVER: RefCell<Server> = RefCell::default();
    }

    fn server<T>(f: impl FnOnce(&mut Server) -> T) -> T {
        SERVER.with(|s| f(&mut s.borrow_mut()))
    }

    fn in_folder(folder: &str, keep: impl Fn(&MessageSummary) -> bool) -> Vec<MessageSummary> {
        server(|s| s.folders.get(folder).into_iter().flatten().filter(|m| keep(m)).cloned().collect())
    }

    struct Fake;

    impl Backend for Fake {
        async fn connect(_: &AccountConfig, _: &str) -> Result<Fake> {
            server(|s| s.connects += 1);
            Ok(Fake)
        }

        async fn logout(self) {}

        fn is_rejection(e: &anyhow::Error) -> bool {
            e.is::<Refused>()
        }

        async fn list_folders(&mut self) -> Result<Vec<(String, Option<String>, Role)>> {
            Ok(vec![("INBOX".into(), None, Role::Inbox), ("Trash".into(), None, Role::Trash)])
        }

        async fn select(&mut self, folder: &str) -> Result<FolderState> {
            let exists = in_folder(folder, |_| true).len() as u32;
            Ok(FolderState { exists, uid_validity: Some(1), highest_modseq: None })
        }

        async fn fetch_new(&mut self, folder: &str, after: u32) -> Result<Vec<MessageSummary>> {
            Ok(in_folder(folder, |m| m.uid > after))
        }

        async fn fetch_latest(&mut self, folder: &str, _: u32, count: u32) -> Result<Vec<MessageSummary>> {
            let all = in_folder(folder, |_| true);
            Ok(all[all.len().saturating_sub(count as usize)..].to_vec())
        }

        async fn fetch_older(&mut self, _: &str, _: u32, _: u32) -> Result<Vec<MessageSummary>> {
            Ok(Vec::new())
        }

        async fn fetch_headers(&mut self, folder: &str, uids: &[u32]) -> Result<Vec<MessageSummary>> {
            Ok(in_folder(folder, |m| uids.contains(&m.uid)))
        }

        async fn fetch_flags(&mut self, folder: &str, ranges: &[(u32, u32)]) -> Result<Vec<Flags>> {
            let found = in_folder(folder, |m| ranges.iter().any(|(a, b)| (*a..=*b).contains(&m.uid)));
            Ok(found
                .iter()
                .map(|m| Flags { uid: m.uid, seen: m.seen, flagged: m.flagged, answered: m.answered, labels: Some(m.labels.clone()) })
                .collect())
        }

        async fn fetch_threads(&mut self, _: &str, _: &[u32]) -> Result<Vec<(u32, u64)>> {
            Ok(Vec::new())
        }

        async fn search(&mut self, _: &str, _: &str) -> Result<Vec<u32>> {
            Ok(Vec::new())
        }

        async fn fetch_bodies(&mut self, _: &str, uids: &[u32]) -> Result<Vec<(u32, Vec<u8>)>> {
            Ok(uids.iter().map(|uid| (*uid, b"Subject: Hi\r\n\r\nHello\r\n".to_vec())).collect())
        }

        async fn apply(&mut self, folder: &str, uid: u32, op: &PendingOp) -> Result<()> {
            if let Some(e) = server(|s| s.apply_errors.pop()) {
                return Err(e);
            }
            server(|s| {
                let messages = s.folders.entry(folder.to_string()).or_default();
                match op {
                    PendingOp::SetSeen(v) => messages.iter_mut().filter(|m| m.uid == uid).for_each(|m| m.seen = *v),
                    PendingOp::SetFlagged(v) => messages.iter_mut().filter(|m| m.uid == uid).for_each(|m| m.flagged = *v),
                    PendingOp::SetAnswered => messages.iter_mut().filter(|m| m.uid == uid).for_each(|m| m.answered = true),
                    PendingOp::Expunge => messages.retain(|m| m.uid != uid),
                    PendingOp::Relabel { add, remove } => messages.iter_mut().filter(|m| m.uid == uid).for_each(|m| {
                        let mut labels: Vec<&str> = m.labels.lines().filter(|l| !remove.iter().any(|r| r == l)).collect();
                        labels.extend(add.iter().map(String::as_str).filter(|a| !m.labels.lines().any(|l| l == *a)));
                        m.labels = labels.join("\n");
                    }),
                    // Like a server: filed in the destination under a new UID.
                    PendingOp::Move(dest) => {
                        let Some(i) = messages.iter().position(|m| m.uid == uid) else { return };
                        let mut moved = messages.remove(i);
                        s.moves += 1;
                        moved.uid = 1000 + s.moves;
                        moved.folder = dest.clone();
                        s.folders.entry(dest.clone()).or_default().push(moved);
                    }
                }
            });
            Ok(())
        }

        async fn send(_: &AccountConfig, _: &str, _: &[u8], _: &[String]) -> Result<()> {
            server(|s| {
                s.sends += 1;
                s.send_errors.pop().map_or(Ok(()), Err)
            })
        }

        fn files_sent(_: &AccountConfig) -> bool {
            true
        }

        async fn save_sent(&mut self, _: &str, _: &[u8]) -> Result<()> {
            Ok(())
        }

        async fn save_draft(&mut self, _: &str, _: &[u8]) -> Result<()> {
            Ok(())
        }

        async fn find_message_id(&mut self, _: &str, _: &str) -> Result<Vec<u32>> {
            Ok(Vec::new())
        }

        async fn create_folder(&mut self, _: &str) -> Result<()> {
            Ok(())
        }

        async fn wait_for_change(self, _: &str, _: Duration) -> Result<(Fake, bool)> {
            Ok((self, false))
        }
    }

    fn message(uid: u32, seen: bool) -> MessageSummary {
        MessageSummary { folder: "INBOX".into(), uid, subject: format!("Message {uid}"), date: uid.into(), seen, ..Default::default() }
    }

    fn worker() -> (Worker<Fake>, Arc<Mutex<Vec<Event>>>) {
        let account = AccountConfig::guess("me@example.org");
        let events = Arc::new(Mutex::new(Vec::new()));
        let sink = events.clone();
        let worker = Worker {
            creds: Arc::new(Credentials::with_secret(&account, "secret".into())),
            account,
            store: Store::open_in_memory().unwrap(),
            conn: None,
            active: None,
            on_event: Arc::new(move |e| sink.lock().unwrap().push(e)),
        };
        (worker, events)
    }

    fn cached(worker: &Worker<Fake>) -> Vec<u32> {
        let mut uids = worker.store.uids("INBOX").unwrap();
        uids.sort_unstable();
        uids
    }

    fn errors(events: &Mutex<Vec<Event>>) -> Vec<Event> {
        events.lock().unwrap().iter().filter(|e| matches!(e, Event::Error(_))).cloned().collect()
    }

    #[tokio::test]
    async fn sync_reports_new_mail_and_drops_expunged() {
        server(|s| s.folders.insert("INBOX".into(), vec![message(1, true), message(2, true)]));
        let (mut worker, events) = worker();
        worker.execute(Command::SyncAll).await;
        assert_eq!(cached(&worker), [1, 2]);
        // The first sync fills the cache without notifying.
        assert!(!events.lock().unwrap().iter().any(|e| matches!(e, Event::NewMail(_))));

        server(|s| {
            let inbox = s.folders.get_mut("INBOX").unwrap();
            inbox.remove(0);
            inbox.push(message(3, false));
        });
        worker.execute(Command::SyncAll).await;
        assert_eq!(cached(&worker), [2, 3]);
        let new_mail: Vec<u32> = events.lock().unwrap().iter().filter_map(|e| match e {
            Event::NewMail(m) => Some(m.iter().map(|m| m.uid)),
            _ => None,
        }).flatten().collect();
        assert_eq!(new_mail, [3]);
        assert_eq!(errors(&events), []);
    }

    #[test]
    fn moving_out_of_gmail_all_mail_relabels() {
        let mut store = Store::open_in_memory().unwrap();
        store
            .sync_folders(&[
                ("INBOX".into(), None, Role::Inbox),
                ("[Gmail]/All Mail".into(), None, Role::All),
                ("[Gmail]/Trash".into(), None, Role::Trash),
                ("Work".into(), None, Role::Other),
                ("Projects".into(), None, Role::Other),
            ])
            .unwrap();
        let m = MessageSummary {
            folder: "[Gmail]/All Mail".into(),
            labels: "\\Inbox\n\\Important\nWork".into(),
            ..message(1, true)
        };
        store.upsert_messages(std::slice::from_ref(&m)).unwrap();
        let relabel = |add: &[&str], remove: &[&str]| PendingOp::Relabel {
            add: add.iter().map(|s| s.to_string()).collect(),
            remove: remove.iter().map(|s| s.to_string()).collect(),
        };
        // Into a label: off the inbox and Work, onto Projects; Important stays.
        assert_eq!(move_op(&store, true, &m, "Projects").unwrap(), relabel(&["Projects"], &["\\Inbox", "Work"]));
        assert_eq!(move_op(&store, true, &m, "INBOX").unwrap(), relabel(&["\\Inbox"], &["Work"]));
        // To All Mail itself: archived, no label left.
        assert_eq!(move_op(&store, true, &m, "[Gmail]/All Mail").unwrap(), relabel(&[], &["\\Inbox", "Work"]));
        // The trash is outside All Mail: a real move.
        assert_eq!(move_op(&store, true, &m, "[Gmail]/Trash").unwrap(), PendingOp::Move("[Gmail]/Trash".into()));
        // Not Gmail, or not from All Mail: a real move.
        assert_eq!(move_op(&store, false, &m, "Projects").unwrap(), PendingOp::Move("Projects".into()));
        let inbox = MessageSummary { folder: "INBOX".into(), ..m.clone() };
        assert_eq!(move_op(&store, true, &inbox, "Projects").unwrap(), PendingOp::Move("Projects".into()));
    }

    #[tokio::test]
    async fn moved_mail_shows_in_its_new_folder() {
        server(|s| {
            s.folders.insert("INBOX".into(), vec![message(1, false)]);
            s.folders.insert("Trash".into(), vec![MessageSummary { folder: "Trash".into(), ..message(7, false) }]);
        });
        let (mut worker, events) = worker();
        worker.execute(Command::SyncAll).await;
        worker.execute(Command::OpenFolder("Trash".into())).await;
        events.lock().unwrap().clear();

        worker.store.apply_local("INBOX", 1, &PendingOp::Move("Trash".into())).unwrap();
        worker.store.apply_local("Trash", 7, &PendingOp::Move("INBOX".into())).unwrap();
        worker.execute(Command::Flush).await;
        // Each in its new folder under its new UID, without waiting for the next sync.
        let uids = |folder: &str| worker.store.messages(folder, 10).unwrap().iter().map(|m| (m.subject.clone(), m.uid)).collect::<Vec<_>>();
        assert_eq!(uids("Trash"), [("Message 1".to_string(), 1001)]);
        assert_eq!(uids("INBOX"), [("Message 7".to_string(), 1002)]);
        // An unread message the user moved into the inbox is not new mail.
        assert!(!events.lock().unwrap().iter().any(|e| matches!(e, Event::NewMail(_))));
        assert_eq!(errors(&events), []);
    }

    #[tokio::test]
    async fn refused_move_is_undone_locally() {
        server(|s| s.folders.insert("INBOX".into(), vec![message(1, true)]));
        let (mut worker, events) = worker();
        worker.execute(Command::SyncAll).await;
        let op = delete_op(&worker.store, "INBOX").unwrap();
        assert_eq!(op, PendingOp::Move("Trash".into()));
        worker.store.apply_local("INBOX", 1, &op).unwrap();
        assert!(worker.store.messages("INBOX", 10).unwrap().is_empty());

        server(|s| s.apply_errors.push(Refused.into()));
        worker.execute(Command::Flush).await;
        assert_eq!(cached(&worker), [1]);
        assert!(worker.store.pending_ops().unwrap().is_empty());
        assert_eq!(errors(&events), []);
    }

    #[tokio::test]
    async fn dropped_connection_is_retried_once() {
        server(|s| s.folders.insert("INBOX".into(), vec![message(1, true)]));
        let (mut worker, events) = worker();
        worker.execute(Command::SyncAll).await;
        worker.store.apply_local("INBOX", 1, &PendingOp::SetFlagged(true)).unwrap();
        let connects = server(|s| s.connects);

        server(|s| s.apply_errors.push(anyhow!("connection reset")));
        worker.execute(Command::Flush).await;
        assert_eq!(server(|s| s.connects), connects + 1);
        assert!(in_folder("INBOX", |m| m.uid == 1)[0].flagged);
        assert!(worker.store.pending_ops().unwrap().is_empty());
        assert_eq!(errors(&events), []);
    }

    #[tokio::test]
    async fn failed_send_is_not_retried() {
        let (mut worker, events) = worker();
        server(|s| s.send_errors.push(anyhow!("connection reset")));
        let draft = Draft {
            to: "you@example.org".into(),
            subject: "Hi".into(),
            body: "Hello".into(),
            message_id: mime::new_message_id("me@example.org"),
            ..Default::default()
        };
        worker.execute(Command::Send { id: 7, draft, replied_to: None }).await;
        assert_eq!(server(|s| s.sends), 1);
        assert!(events.lock().unwrap().iter().any(|e| matches!(e, Event::SendFailed(7, _))));
    }
}
