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
use tokio::sync::{Notify, mpsc};

use crate::config::AccountConfig;
use crate::imap::Imap;
use crate::mime::{self, Draft};
use crate::store::{PendingOp, Role, Store};

/// Messages fetched when a folder is synced for the first time.
const INITIAL_WINDOW: u32 = 500;
/// Bodies downloaded ahead of time per sync, for offline reading and search.
const PREFETCH_BODIES: usize = 40;
const POLL_INTERVAL: Duration = Duration::from_secs(5 * 60);
const IDLE_TIMEOUT: Duration = Duration::from_secs(25 * 60);

#[derive(Debug, Clone)]
pub enum Command {
    /// Refresh the folder list, the inbox and the active folder.
    SyncAll,
    /// Make `folder` the active folder and sync it.
    OpenFolder(String),
    /// Download a message body that is not cached yet.
    FetchBody { folder: String, uid: u32 },
    /// Push locally queued changes to the server.
    Flush,
    /// `id` is echoed back in [`Event::Sent`] / [`Event::SendFailed`].
    Send { id: u64, draft: Draft, replied_to: Option<(String, u32)> },
}

#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    Status(String),
    Error(String),
    Online(bool),
    FoldersChanged,
    MessagesChanged(String),
    BodyReady { folder: String, uid: u32 },
    Sent(u64),
    SendFailed(u64, String),
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
                let runtime = tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(2)
                    .enable_all()
                    .build()
                    .expect("tokio runtime");
                let on_event: Arc<dyn Fn(Event) + Send + Sync> = Arc::new(on_event);
                runtime.block_on(async move {
                    let password = match account.password() {
                        Ok(p) => p,
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
                    tokio::spawn(idle_loop(account.clone(), password.clone(), idle_tx));
                    Worker { account, password, store, imap: None, active: None, on_event }.run(rx).await;
                });
            })
            .expect("spawning engine thread");
        Engine { tx }
    }

    pub fn send(&self, command: Command) {
        let _ = self.tx.send(command);
    }
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
        None => store.folder_by_role(Role::All)?.ok_or_else(|| anyhow!("no Archive folder on this account"))?,
    };
    if dest == folder {
        return Err(anyhow!("message is already archived"));
    }
    Ok(PendingOp::Move(dest))
}

struct Worker {
    account: AccountConfig,
    password: String,
    store: Store,
    imap: Option<Imap>,
    active: Option<String>,
    on_event: Arc<dyn Fn(Event) + Send + Sync>,
}

fn is_server_rejection(e: &anyhow::Error) -> bool {
    e.chain().any(|c| {
        matches!(c.downcast_ref::<async_imap::error::Error>(), Some(async_imap::error::Error::No(_) | async_imap::error::Error::Bad(_)))
    })
}

impl Worker {
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
            // Collapse bursts of syncs (e.g. IDLE notifications plus a poll tick).
            if matches!(command, Command::SyncAll) {
                while let Ok(Command::SyncAll) = rx.try_recv() {}
            }
            // Sending is never retried: SMTP may already have delivered it.
            let retry = (!matches!(command, Command::Send { .. })).then(|| command.clone());
            let mut result = self.handle(command).await;
            if let (Err(e), Some(command)) = (&result, retry) {
                if !is_server_rejection(e) {
                    // Usually a connection the server dropped while idle: reconnect and try again once.
                    log::info!("engine: {e:#}; reconnecting");
                    self.imap = None;
                    result = self.handle(command).await;
                }
            }
            if let Err(e) = result {
                log::warn!("engine: {e:#}");
                if !is_server_rejection(&e) {
                    // Assume the connection is broken; reconnect on next use.
                    self.imap = None;
                    self.emit(Event::Online(false));
                }
                self.emit(Event::Error(format!("{e:#}")));
            }
        }
        if let Some(imap) = self.imap.take() {
            imap.logout().await;
        }
    }

    async fn imap(&mut self) -> Result<&mut Imap> {
        if self.imap.is_none() {
            self.emit(Event::Status(format!("Connecting to {}…", self.account.imap_host)));
            self.imap = Some(Imap::connect(&self.account, &self.password).await?);
            self.emit(Event::Online(true));
        }
        Ok(self.imap.as_mut().unwrap())
    }

    async fn handle(&mut self, command: Command) -> Result<()> {
        match command {
            Command::SyncAll => {
                self.flush().await?;
                self.emit(Event::Status("Syncing folders…".into()));
                let folders = self.imap().await?.list_folders().await?;
                self.store.sync_folders(&folders)?;
                self.emit(Event::FoldersChanged);
                let inbox = self.store.folder_by_role(Role::Inbox)?;
                let mut targets: Vec<String> = inbox.into_iter().collect();
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
            Command::Send { id, draft, replied_to } => {
                let sent = std::cell::Cell::new(false);
                if let Err(e) = self.send(id, &draft, replied_to, &sent).await {
                    if sent.get() {
                        // Delivered, but filing it in Sent failed: not a send failure.
                        return Err(e.context("message sent, but saving it to the Sent folder failed"));
                    }
                    self.emit(Event::SendFailed(id, format!("{e:#}")));
                    self.emit(Event::Status(String::new()));
                }
            }
        }
        Ok(())
    }

    async fn flush(&mut self) -> Result<()> {
        let ops = self.store.pending_ops()?;
        for (id, folder, uid, op) in ops {
            let imap = self.imap().await?;
            let result = match &op {
                PendingOp::SetSeen(v) => imap.store_flag(&folder, uid, "\\Seen", *v).await,
                PendingOp::SetFlagged(v) => imap.store_flag(&folder, uid, "\\Flagged", *v).await,
                PendingOp::SetAnswered => imap.store_flag(&folder, uid, "\\Answered", true).await,
                PendingOp::Move(dest) => imap.move_message(&folder, uid, dest).await,
                PendingOp::Expunge => imap.expunge(&folder, uid).await,
            };
            match result {
                Ok(()) => self.store.remove_pending_op(id)?,
                // The server refused (message gone, folder missing): retrying won't help.
                Err(e) if is_server_rejection(&e) => {
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
        let state = self.imap().await?.select(folder).await?;
        if let Some(v) = state.uid_validity
            && self.store.check_uidvalidity(folder, v)? {
                log::info!("UIDVALIDITY of {folder} changed, cache reset");
            }

        match (self.store.min_uid(folder)?, self.store.max_uid(folder)?) {
            (Some(min), Some(max)) => {
                let new = self.imap().await?.fetch_new(folder, max).await?;
                self.store.upsert_messages(&new)?;
                let flags = self.imap().await?.fetch_flags(folder, min, max).await?;
                let on_server: std::collections::HashSet<u32> = flags.iter().map(|f| f.0).collect();
                let gone: Vec<u32> = self
                    .store
                    .uids(folder)?
                    .into_iter()
                    .filter(|uid| (min..=max).contains(uid) && !on_server.contains(uid))
                    .collect();
                self.store.delete_messages(folder, &gone)?;
                self.store.update_flags(folder, &flags)?;
            }
            _ => {
                let latest = self.imap().await?.fetch_latest(folder, state.exists, INITIAL_WINDOW).await?;
                self.store.upsert_messages(&latest)?;
            }
        }
        self.emit(Event::MessagesChanged(folder.to_string()));
        self.emit(Event::FoldersChanged);

        let missing = self.store.uids_missing_body(folder, PREFETCH_BODIES)?;
        if !missing.is_empty() {
            self.download_bodies(folder, &missing).await?;
            self.emit(Event::MessagesChanged(folder.to_string()));
        }
        Ok(())
    }

    async fn download_bodies(&mut self, folder: &str, uids: &[u32]) -> Result<()> {
        let bodies = self.imap().await?.fetch_bodies(folder, uids).await?;
        for (uid, raw) in bodies {
            let body = mime::body_from_raw(&raw);
            self.store.set_body(folder, uid, &raw, &body.snippet, &body.text, !body.attachments.is_empty())?;
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
        crate::smtp::send(&self.account, &self.password, &built.raw, &built.recipients).await?;
        sent.set(true);
        self.emit(Event::Sent(id));
        self.emit(Event::Status("Message sent".into()));

        if let Some((folder, uid)) = replied_to {
            self.store.apply_local(&folder, uid, &PendingOp::SetAnswered)?;
            self.emit(Event::MessagesChanged(folder));
        }
        // Gmail files messages sent over SMTP itself; appending would duplicate them.
        let server_saves_sent = self.account.smtp_host.ends_with("gmail.com");
        if !server_saves_sent
            && let Some(sent) = self.store.folder_by_role(Role::Sent)? {
                self.imap().await?.append(&sent, "(\\Seen)", &built.raw).await?;
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
async fn idle_loop(
    account: AccountConfig,
    password: String,
    tx: mpsc::UnboundedSender<Command>,
) {
    let never = Notify::new();
    let mut backoff = Duration::from_secs(10);
    loop {
        let result: Result<()> = async {
            let mut imap = Imap::connect(&account, &password).await?;
            backoff = Duration::from_secs(10);
            loop {
                let (next, changed) = imap.idle("INBOX", IDLE_TIMEOUT, &never).await?;
                imap = next;
                if changed {
                    // Give the server a moment to finish delivering a batch.
                    tokio::time::sleep(Duration::from_millis(500)).await;
                    if tx.send(Command::SyncAll).is_err() {
                        return Ok(());
                    }
                } else {
                    imap.noop().await?;
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
