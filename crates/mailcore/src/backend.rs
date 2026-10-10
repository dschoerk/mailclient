//! The contract between the sync engine and a mail protocol. IMAP+SMTP is the
//! built-in implementation; another provider (JMAP, Graph, a plugin) implements
//! [`Backend`] and gets picked in `Engine::start`.
//!
//! shortcut: messages are addressed by (folder, UID) and synced by UID ranges, because the
//! cache is keyed that way; a backend with string ids or change tokens (JMAP, Graph) needs
//! an id mapping in the store and a backend-owned sync step first.

use std::future::Future;
use std::time::Duration;

use anyhow::Result;

use crate::config::AccountConfig;
use crate::store::{MessageSummary, PendingOp, Role};

pub struct FolderState {
    pub exists: u32,
    pub uid_validity: Option<u32>,
}

/// One authenticated session with the server. The engine keeps one for commands
/// and a second one parked in [`Backend::wait_for_change`].
pub trait Backend: Sized + Send + 'static {
    fn connect(account: &AccountConfig, secret: &str) -> impl Future<Output = Result<Self>> + Send;

    fn logout(self) -> impl Future<Output = ()> + Send;

    /// True when the server refused a request (retrying won't help), as opposed to a broken connection.
    fn is_rejection(e: &anyhow::Error) -> bool;

    /// (name, hierarchy delimiter, role) of every selectable folder.
    fn list_folders(
        &mut self,
    ) -> impl Future<Output = Result<Vec<(String, Option<String>, Role)>>> + Send;

    fn select(&mut self, folder: &str) -> impl Future<Output = Result<FolderState>> + Send;

    /// Headers of messages with UID greater than `after`.
    fn fetch_new(
        &mut self,
        folder: &str,
        after: u32,
    ) -> impl Future<Output = Result<Vec<MessageSummary>>> + Send;

    /// Headers of the newest `count` of the `exists` messages in `folder`.
    fn fetch_latest(
        &mut self,
        folder: &str,
        exists: u32,
        count: u32,
    ) -> impl Future<Output = Result<Vec<MessageSummary>>> + Send;

    /// (uid, seen, flagged, answered) of all messages with UID in `from..=to`.
    fn fetch_flags(
        &mut self,
        folder: &str,
        from: u32,
        to: u32,
    ) -> impl Future<Output = Result<Vec<(u32, bool, bool, bool)>>> + Send;

    /// Full RFC 822 source of messages, without marking them read.
    fn fetch_bodies(
        &mut self,
        folder: &str,
        uids: &[u32],
    ) -> impl Future<Output = Result<Vec<(u32, Vec<u8>)>>> + Send;

    /// Pushes one locally queued change to the server.
    fn apply(
        &mut self,
        folder: &str,
        uid: u32,
        op: &PendingOp,
    ) -> impl Future<Output = Result<()>> + Send;

    /// Delivers a built message. Needs no open session, so sending works while sync is down.
    fn send(
        account: &AccountConfig,
        secret: &str,
        raw: &[u8],
        recipients: &[String],
    ) -> impl Future<Output = Result<()>> + Send;

    /// Whether the server files sent messages itself; otherwise the engine calls [`Backend::save_sent`].
    fn files_sent(account: &AccountConfig) -> bool;

    fn save_sent(&mut self, folder: &str, raw: &[u8]) -> impl Future<Output = Result<()>> + Send;

    fn save_draft(&mut self, folder: &str, raw: &[u8]) -> impl Future<Output = Result<()>> + Send;

    /// UIDs of the messages in `folder` with this Message-ID header (`<id@host>`).
    fn find_message_id(&mut self, folder: &str, id: &str) -> impl Future<Output = Result<Vec<u32>>> + Send;

    fn create_folder(&mut self, name: &str) -> impl Future<Output = Result<()>> + Send;

    /// Waits until `folder` changes or `timeout` passes, keeping the session alive.
    /// Returns the session and whether something changed.
    fn wait_for_change(
        self,
        folder: &str,
        timeout: Duration,
    ) -> impl Future<Output = Result<(Self, bool)>> + Send;
}
