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
/// Bodies downloaded ahead of time per sync, for offline reading and search.
const PREFETCH_BODIES: usize = 40;
/// Bodies per FETCH: each batch is held in memory whole, and attachments can be large.
const BODY_BATCH: usize = 5;
const POLL_INTERVAL: Duration = Duration::from_secs(5 * 60);
const IDLE_TIMEOUT: Duration = Duration::from_secs(25 * 60);

#[derive(Debug, Clone)]
pub enum Command {
    /// Refresh the folder list, the inbox, Sent and the active folder.
    SyncAll,
    /// Make `folder` the active folder and sync it.
    OpenFolder(String),
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
    Sent(u64),
    /// A send or a draft save failed.
    SendFailed(u64, String),
    DraftSaved(u64),
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
        loop {
            let command = tokio::select! {
                c = rx.recv() => match c {
                    Some(c) => c,
                    None => break,
                },
                _ = poll.tick() => Command::SyncAll,
            };
            if matches!(command, Command::Stop) {
                break;
            }
            // Collapse bursts of syncs (e.g. IDLE notifications plus a poll tick).
            if matches!(command, Command::SyncAll) {
                while let Ok(Command::SyncAll) = rx.try_recv() {}
            }
            let draft_id = match &command {
                Command::SaveDraft { id, .. } => Some(*id),
                _ => None,
            };
            // Sending is never retried: SMTP may already have delivered it.
            let retry = (!matches!(command, Command::Send { .. })).then(|| command.clone());
            let mut result = self.handle(command).await;
            if let (Err(e), Some(command)) = (&result, retry) {
                if !B::is_rejection(e) {
                    // Usually a connection the server dropped while idle: reconnect and try again once.
                    log::info!("engine: {e:#}; reconnecting");
                    self.conn = None;
                    result = self.handle(command).await;
                }
            }
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
            Command::Stop => {}
        }
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
                    self.store.remove_pending_op(id)?;
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

        match (self.store.min_uid(folder)?, self.store.max_uid(folder)?) {
            (Some(min), Some(max)) => {
                let new = self.conn().await?.fetch_new(folder, max).await?;
                self.store.upsert_messages(&new)?;
                changed |= !new.is_empty();
                let unread: Vec<MessageSummary> = new.into_iter().filter(|m| !m.seen).collect();
                if !unread.is_empty() && self.store.folder_role(folder)? == Role::Inbox {
                    self.emit(Event::NewMail(unread));
                }
                let flags = self.conn().await?.fetch_flags(folder, min, max).await?;
                let on_server: std::collections::HashSet<u32> = flags.iter().map(|f| f.0).collect();
                let gone: Vec<u32> = self
                    .store
                    .uids(folder)?
                    .into_iter()
                    .filter(|uid| (min..=max).contains(uid) && !on_server.contains(uid))
                    .collect();
                self.store.delete_messages(folder, &gone)?;
                changed |= !gone.is_empty();
                changed |= self.store.update_flags(folder, &flags)?;
            }
            _ => {
                let latest = self
                    .conn()
                    .await?
                    .fetch_latest(folder, state.exists, INITIAL_WINDOW)
                    .await?;
                self.store.upsert_messages(&latest)?;
                changed |= !latest.is_empty();
            }
        }
        if changed {
            self.emit(Event::MessagesChanged(folder.to_string()));
            self.emit(Event::FoldersChanged);
        }

        let missing = self.store.uids_missing_body(folder, PREFETCH_BODIES)?;
        if !missing.is_empty() {
            self.download_bodies(folder, &missing).await?;
            self.emit(Event::MessagesChanged(folder.to_string()));
        }
        Ok(())
    }

    async fn download_bodies(&mut self, folder: &str, uids: &[u32]) -> Result<()> {
        for batch in uids.chunks(BODY_BATCH) {
            let bodies = self.conn().await?.fetch_bodies(folder, batch).await?;
            for (uid, raw) in bodies {
                let body = mime::body_from_raw(&raw);
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
