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
    /// Search the server; answered with [`Event::SearchHits`].
    Search(String),
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

pub fn archive_op(store: &Store, folder: &str) -> Result<PendingOp> {
    let dest = match store.folder_by_role(Role::Archive)? {
        Some(a) => a,
        None => store
            .folder_by_role(Role::All)?
            .ok_or_else(|| anyhow!("no Archive folder on this account"))?,
    };
    if dest == folder {
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
                continue;
            }
            // Collapse bursts: syncs (IDLE notifications plus a poll tick), and searches typed
            // faster than the server answers, where the last one wins.
            if matches!(command, Command::SyncAll | Command::Search(_)) {
                while let Ok(next) = rx.try_recv() {
                    match (&command, next) {
                        (Command::SyncAll, Command::SyncAll) => {}
                        (Command::Search(_), next @ Command::Search(_)) => command = next,
                        (_, next) => {
                            queued = Some(next);
                            break;
                        }
                    }
                }
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
        if let Some(conn) = self.conn.take() {
            conn.logout().await;
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
                // Sent too: the user's replies belong in the inbox's conversations.
                for role in [Role::Inbox, Role::Sent] {
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
            Command::Search(query) => self.search(query).await?,
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
        let ops = self.store.pending_ops()?;
        for (id, folder, uid, op) in ops {
            let result = self.conn().await?.apply(&folder, uid, &op).await;
            match result {
                Ok(()) => self.store.remove_pending_op(id)?,
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
                if !unread.is_empty() && self.store.folder_role(folder)? == Role::Inbox {
                    self.emit(Event::NewMail(unread));
                }
                // The synced window, plus search hits below it one by one.
                let mut ranges = vec![(from, max.max(from))];
                ranges.extend(cached.iter().filter(|uid| **uid < from).map(|uid| (*uid, *uid)));
                let flags = self.conn().await?.fetch_flags(folder, &ranges).await?;
                let on_server: std::collections::HashSet<u32> = flags.iter().map(|f| f.0).collect();
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
    async fn search(&mut self, query: String) -> Result<()> {
        self.emit(Event::Status("Searching the server…".into()));
        // An all-mail folder (Gmail) holds everything; elsewhere, where most mail lives.
        let folders = match self.store.folder_by_role(Role::All)? {
            Some(all) => vec![all],
            None => {
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
