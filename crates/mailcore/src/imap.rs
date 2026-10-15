//! Thin wrapper around an authenticated IMAP session.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use async_imap::extensions::idle::IdleResponse;
use async_imap::types::{Fetch, Flag, NameAttribute};
use futures::TryStreamExt;
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;
use tokio_rustls::client::TlsStream;

use crate::backend::{Backend, FolderState};
use crate::config::{AccountConfig, Auth, Security};
use crate::mime;
use crate::store::{Flags, MessageSummary, PendingOp, Role};

pub type Session = async_imap::Session<TlsStream<TcpStream>>;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);

/// PEM file of one extra trusted CA (self-hosted servers, local test server), from `MAILCLIENT_EXTRA_CA`.
pub fn extra_ca_pem() -> Result<Option<Vec<u8>>> {
    match std::env::var_os("MAILCLIENT_EXTRA_CA") {
        Some(path) => {
            Ok(Some(std::fs::read(&path).with_context(|| {
                format!("reading MAILCLIENT_EXTRA_CA {path:?}")
            })?))
        }
        None => Ok(None),
    }
}

pub fn tls_connector() -> Result<TlsConnector> {
    use rustls::pki_types::{CertificateDer, pem::PemObject};
    let mut roots = rustls::RootCertStore {
        roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
    };
    if let Some(pem) = extra_ca_pem()? {
        for cert in CertificateDer::pem_slice_iter(&pem) {
            roots.add(cert.context("parsing MAILCLIENT_EXTRA_CA")?)?;
        }
    }
    let config = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    Ok(TlsConnector::from(Arc::new(config)))
}

async fn tls_handshake(tcp: TcpStream, host: &str) -> Result<TlsStream<TcpStream>> {
    let name =
        rustls::pki_types::ServerName::try_from(host.to_string()).context("invalid server name")?;
    tls_connector()?
        .connect(name, tcp)
        .await
        .with_context(|| format!("TLS handshake with {host}"))
}

pub struct Imap {
    session: Session,
    selected: Option<String>,
    has_move: bool,
    has_condstore: bool,
    /// Gmail's extensions, with its own search syntax (X-GM-RAW).
    gmail: bool,
}

impl Imap {
    pub async fn connect(account: &AccountConfig, password: &str) -> Result<Imap> {
        let addr = (account.imap_host.as_str(), account.imap_port);
        let tcp = tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(addr))
            .await
            .map_err(|_| anyhow!("timed out connecting to {}", account.imap_host))?
            .with_context(|| {
                format!("connecting to {}:{}", account.imap_host, account.imap_port)
            })?;
        // Each command is small and waits for its answer; with Nagle's algorithm on, it would also
        // wait for the server to acknowledge the previous one, which Gmail delays: seconds per sync.
        tcp.set_nodelay(true)?;

        let tls = match account.imap_security {
            Security::Tls => {
                let tls = tls_handshake(tcp, &account.imap_host).await?;
                let mut client = async_imap::Client::new(tls);
                client
                    .read_response()
                    .await?
                    .ok_or_else(|| anyhow!("server closed connection before greeting"))?;
                client.into_inner()
            }
            Security::StartTls => {
                let mut client = async_imap::Client::new(tcp);
                client
                    .read_response()
                    .await?
                    .ok_or_else(|| anyhow!("server closed connection before greeting"))?;
                client
                    .run_command_and_check_ok("STARTTLS", None)
                    .await
                    .context("STARTTLS")?;
                tls_handshake(client.into_inner(), &account.imap_host).await?
            }
        };

        let client = async_imap::Client::new(tls);
        let mut session = match account.auth {
            Auth::Password => client.login(&account.username, password).await,
            Auth::Microsoft | Auth::Google => client.authenticate("XOAUTH2", XOAuth2(crate::auth::xoauth2(&account.username, password))).await,
        }
        .map_err(|(e, _)| {
            // GMX rejects even correct passwords until IMAP is switched on in its web mail.
            let hint = if account.imap_host.ends_with("gmx.net") {
                "\nGMX: enable access under Settings → POP3 & IMAP in GMX web mail; with two-factor login, use an app password."
            } else {
                ""
            };
            anyhow!("IMAP login failed: {e}{hint}")
        })?;
        let caps = session.capabilities().await?;
        Ok(Imap {
            session,
            selected: None,
            has_move: caps.has_str("MOVE"),
            has_condstore: caps.has_str("CONDSTORE"),
            gmail: caps.has_str("X-GM-EXT-1"),
        })
    }

    pub async fn logout(mut self) {
        let _ = self.session.logout().await;
    }

    pub async fn list_folders(&mut self) -> Result<Vec<(String, Option<String>, Role)>> {
        let names: Vec<_> = self
            .session
            .list(Some(""), Some("*"))
            .await?
            .try_collect()
            .await?;
        self.confirm().await?;
        let mut folders = Vec::new();
        for name in &names {
            if name
                .attributes()
                .iter()
                .any(|a| matches!(a, NameAttribute::NoSelect))
            {
                continue;
            }
            let role = if name.name().eq_ignore_ascii_case("INBOX") {
                Role::Inbox
            } else {
                name.attributes()
                    .iter()
                    .find_map(|a| match a {
                        NameAttribute::Sent => Some(Role::Sent),
                        NameAttribute::Drafts => Some(Role::Drafts),
                        NameAttribute::Trash => Some(Role::Trash),
                        NameAttribute::Junk => Some(Role::Junk),
                        NameAttribute::Archive => Some(Role::Archive),
                        NameAttribute::All => Some(Role::All),
                        NameAttribute::Flagged => Some(Role::Flagged),
                        _ => None,
                    })
                    .unwrap_or_else(|| role_from_name(name.name()))
            };
            folders.push((
                name.name().to_string(),
                name.delimiter().map(str::to_string),
                role,
            ));
        }
        // A role should map to one folder; prefer the one the server marked.
        let mut seen = std::collections::HashSet::new();
        for f in folders.iter_mut() {
            if f.2 != Role::Other && !seen.insert(f.2) {
                f.2 = Role::Other;
            }
        }
        Ok(folders)
    }

    pub async fn select(&mut self, folder: &str) -> Result<FolderState> {
        let mailbox = if self.has_condstore {
            self.session.select_condstore(folder).await
        } else {
            self.session.select(folder).await
        }
        .with_context(|| format!("selecting {folder}"))?;
        self.selected = Some(folder.to_string());
        // Every SELECT response carries UIDVALIDITY (RFC 3501); none means the reply was cut off.
        if mailbox.uid_validity.is_none() {
            self.selected = None;
            return Err(anyhow!(
                "connection to server lost (no SELECT response for {folder})"
            ));
        }
        Ok(FolderState {
            exists: mailbox.exists,
            uid_validity: mailbox.uid_validity,
            highest_modseq: mailbox.highest_modseq,
        })
    }

    async fn ensure_selected(&mut self, folder: &str) -> Result<()> {
        if self.selected.as_deref() != Some(folder) {
            self.select(folder).await?;
        }
        Ok(())
    }

    /// Gmail also reports each message's labels.
    fn summary_query(&self) -> &'static str {
        if self.gmail { "(UID FLAGS INTERNALDATE X-GM-LABELS BODY.PEEK[HEADER])" } else { "(UID FLAGS INTERNALDATE BODY.PEEK[HEADER])" }
    }

    fn summary(folder: &str, fetch: &Fetch) -> Option<MessageSummary> {
        let uid = fetch.uid?;
        let date = fetch.internal_date().map(|d| d.timestamp());
        let mut s =
            mime::summary_from_headers(folder, uid, fetch.header().unwrap_or_default(), date);
        let (seen, flagged, answered) = flags_of(fetch);
        s.seen = seen;
        s.flagged = flagged;
        s.answered = answered;
        s.labels = labels_of(fetch).unwrap_or_default();
        Some(s)
    }

    /// Headers of messages with UID greater than `after`.
    pub async fn fetch_new(&mut self, folder: &str, after: u32) -> Result<Vec<MessageSummary>> {
        self.ensure_selected(folder).await?;
        let fetches: Vec<Fetch> = self
            .session
            .uid_fetch(format!("{}:*", after + 1), self.summary_query())
            .await?
            .try_collect()
            .await?;
        self.confirm().await?;
        // "n:*" always returns the last message, even when its UID is below n.
        Ok(fetches
            .iter()
            .filter_map(|f| Self::summary(folder, f))
            .filter(|s| s.uid > after)
            .collect())
    }

    /// Headers of the newest `count` messages, by sequence number.
    pub async fn fetch_latest(
        &mut self,
        folder: &str,
        exists: u32,
        count: u32,
    ) -> Result<Vec<MessageSummary>> {
        self.ensure_selected(folder).await?;
        if exists == 0 {
            return Ok(Vec::new());
        }
        let start = exists.saturating_sub(count) + 1;
        let fetches: Vec<Fetch> = self
            .session
            .fetch(format!("{start}:*"), self.summary_query())
            .await?
            .try_collect()
            .await?;
        self.confirm().await?;
        Ok(fetches
            .iter()
            .filter_map(|f| Self::summary(folder, f))
            .collect())
    }

    /// Headers of the `count` messages just before UID `before`, by sequence number.
    pub async fn fetch_older(&mut self, folder: &str, before: u32, count: u32) -> Result<Vec<MessageSummary>> {
        self.ensure_selected(folder).await?;
        // Sequence number of the first message from `before` up, even when `before` itself is gone.
        let first = self.session.search(format!("UID {before}:*")).await?.into_iter().min();
        self.confirm().await?;
        // Up to `first` itself: "n:*" matches the last message even when its UID is below n
        // (RFC 3501). Servers that match nothing instead mean every message is older.
        let end = match first {
            Some(first) => first,
            None => self.select(folder).await?.exists,
        };
        if end == 0 {
            return Ok(Vec::new());
        }
        let start = end.saturating_sub(count).max(1);
        let fetches: Vec<Fetch> = self
            .session
            .fetch(format!("{start}:{end}"), self.summary_query())
            .await?
            .try_collect()
            .await?;
        self.confirm().await?;
        Ok(fetches
            .iter()
            .filter_map(|f| Self::summary(folder, f))
            .filter(|s| s.uid < before)
            .collect())
    }

    pub async fn fetch_headers(&mut self, folder: &str, uids: &[u32]) -> Result<Vec<MessageSummary>> {
        if uids.is_empty() {
            return Ok(Vec::new());
        }
        self.ensure_selected(folder).await?;
        let fetches: Vec<Fetch> = self
            .session
            .uid_fetch(uid_set(uids), self.summary_query())
            .await?
            .try_collect()
            .await?;
        self.confirm().await?;
        Ok(fetches
            .iter()
            .filter_map(|f| Self::summary(folder, f))
            .filter(|s| uids.contains(&s.uid))
            .collect())
    }

    /// Current flags (and Gmail labels) of all messages with UID in one of the inclusive `ranges`;
    /// with `changed_since`, only of those whose mod-sequence is above it (needs CONDSTORE).
    pub async fn fetch_flags(&mut self, folder: &str, ranges: &[(u32, u32)], changed_since: Option<u64>) -> Result<Vec<Flags>> {
        self.ensure_selected(folder).await?;
        let mut flags = Vec::new();
        let items = if self.gmail { "(UID FLAGS X-GM-LABELS)" } else { "(UID FLAGS)" };
        let query = match changed_since {
            Some(modseq) => format!("{items} (CHANGEDSINCE {modseq})"),
            None => items.to_string(),
        };
        for chunk in ranges.chunks(500) {
            let fetches: Vec<Fetch> = self
                .session
                .uid_fetch(range_set(chunk), &query)
                .await?
                .try_collect()
                .await?;
            self.confirm().await?;
            flags.extend(fetches.iter().filter_map(|f| {
                let (seen, flagged, answered) = flags_of(f);
                f.uid
                    .filter(|uid| chunk.iter().any(|(a, b)| (a..=b).contains(&uid)))
                    .map(|uid| Flags { uid, seen, flagged, answered, labels: labels_of(f) })
            }));
        }
        Ok(flags)
    }

    /// UIDs of the messages still in `folder` with UID in one of the inclusive `ranges`: a UID SEARCH
    /// answers with numbers only, much less than the flags (RFC 4549 6.1).
    pub async fn existing_uids(&mut self, folder: &str, ranges: &[(u32, u32)]) -> Result<Vec<u32>> {
        self.ensure_selected(folder).await?;
        let mut uids = Vec::new();
        for chunk in ranges.chunks(500) {
            let found = self.session.uid_search(format!("UID {}", range_set(chunk))).await?;
            self.confirm().await?;
            uids.extend(found.into_iter().filter(|uid| chunk.iter().any(|(a, b)| (a..=b).contains(&uid))));
        }
        Ok(uids)
    }

    /// Gmail's conversation id (X-GM-THRID) of each of these messages; none from other servers.
    /// async-imap parses the attribute but has no accessor for it, so this FETCH reads the responses itself.
    pub async fn fetch_threads(&mut self, folder: &str, uids: &[u32]) -> Result<Vec<(u32, u64)>> {
        use async_imap::imap_proto::{AttributeValue, Response, Status};
        if !self.gmail || uids.is_empty() {
            return Ok(Vec::new());
        }
        self.ensure_selected(folder).await?;
        let mut threads = Vec::new();
        for chunk in uids.chunks(500) {
            let id = self.session.run_command(format!("UID FETCH {} (UID X-GM-THRID)", uid_set(chunk))).await?;
            loop {
                let response = self.session.read_response().await?.ok_or_else(|| anyhow!("connection to server lost"))?;
                match response.parsed() {
                    Response::Fetch(_, attrs) => {
                        let uid = attrs.iter().find_map(|a| match a {
                            AttributeValue::Uid(uid) => Some(*uid),
                            _ => None,
                        });
                        let thread = attrs.iter().find_map(|a| match a {
                            AttributeValue::GmailThrId(t) => Some(*t),
                            _ => None,
                        });
                        threads.extend(uid.zip(thread));
                    }
                    Response::Done { tag, status, outcome } if *tag == id => match status {
                        Status::Ok => break,
                        // A refusal, like async-imap reports one (see `is_rejection`).
                        _ => return Err(async_imap::error::Error::No(outcome.information.as_deref().unwrap_or_default().to_string()).into()),
                    },
                    // Unsolicited EXISTS and the like: the next sync sees them.
                    _ => {}
                }
            }
        }
        Ok(threads)
    }

    /// UIDs of the messages in `folder` matching the search box's text.
    pub async fn search(&mut self, folder: &str, query: &str) -> Result<Vec<u32>> {
        self.ensure_selected(folder).await?;
        let uids = self.session.uid_search(search_criteria(query, self.gmail)).await?;
        self.confirm().await?;
        Ok(uids.into_iter().collect())
    }

    /// Full RFC 822 source of messages, without setting \Seen.
    pub async fn fetch_bodies(
        &mut self,
        folder: &str,
        uids: &[u32],
    ) -> Result<Vec<(u32, Vec<u8>)>> {
        if uids.is_empty() {
            return Ok(Vec::new());
        }
        self.ensure_selected(folder).await?;
        let fetches: Vec<Fetch> = self
            .session
            .uid_fetch(uid_set(uids), "(UID BODY.PEEK[])")
            .await?
            .try_collect()
            .await?;
        self.confirm().await?;
        Ok(fetches
            .iter()
            .filter_map(|f| Some((f.uid?, f.body()?.to_vec())))
            .collect())
    }

    pub async fn store_flag(&mut self, folder: &str, uid: u32, flag: &str, on: bool) -> Result<()> {
        self.ensure_selected(folder).await?;
        let op = if on { "+FLAGS.SILENT" } else { "-FLAGS.SILENT" };
        let _: Vec<_> = self
            .session
            .uid_store(uid.to_string(), format!("{op} ({flag})"))
            .await?
            .try_collect()
            .await?;
        self.confirm().await
    }

    /// Gmail: adds and removes labels (`X-GM-LABELS`); system ones (`\Inbox`) are atoms, the rest quoted.
    pub async fn relabel(&mut self, folder: &str, uid: u32, add: &[String], remove: &[String]) -> Result<()> {
        self.ensure_selected(folder).await?;
        for (sign, labels) in [("+", add), ("-", remove)] {
            if labels.is_empty() {
                continue;
            }
            let list: Vec<String> = labels.iter().map(|l| if l.starts_with('\\') { l.clone() } else { quote(l) }).collect();
            let _: Vec<_> = self
                .session
                .uid_store(uid.to_string(), format!("{sign}X-GM-LABELS ({})", list.join(" ")))
                .await?
                .try_collect()
                .await?;
            self.confirm().await?;
        }
        Ok(())
    }

    pub async fn move_message(&mut self, folder: &str, uid: u32, dest: &str) -> Result<()> {
        self.ensure_selected(folder).await?;
        if self.has_move {
            self.session.uid_mv(uid.to_string(), dest).await?;
        } else {
            self.session.uid_copy(uid.to_string(), dest).await?;
            self.expunge(folder, uid).await?;
        }
        Ok(())
    }

    /// Permanently removes a message (UID EXPUNGE only touches this UID).
    pub async fn expunge(&mut self, folder: &str, uid: u32) -> Result<()> {
        self.store_flag(folder, uid, "\\Deleted", true).await?;
        let _: Vec<_> = self
            .session
            .uid_expunge(uid.to_string())
            .await?
            .try_collect()
            .await?;
        self.confirm().await
    }

    pub async fn append(&mut self, folder: &str, flags: &str, raw: &[u8]) -> Result<()> {
        self.session.append(folder, Some(flags), None, raw).await?;
        Ok(())
    }

    pub async fn create(&mut self, folder: &str) -> Result<()> {
        self.session.create(folder).await?;
        Ok(())
    }

    pub async fn delete(&mut self, folder: &str) -> Result<()> {
        self.session.delete(folder).await?;
        Ok(())
    }

    /// async-imap 0.12 ends a response stream silently when the server has closed the
    /// connection, so a FETCH/STORE on a dead connection returns `Ok` with partial or no
    /// data. NOOP does check for the tagged reply; running it afterwards turns a lost
    /// connection into an error instead of "the server has no messages".
    async fn confirm(&mut self) -> Result<()> {
        self.session
            .noop()
            .await
            .context("connection to server lost")
    }

    pub async fn noop(&mut self) -> Result<()> {
        self.session.noop().await?;
        Ok(())
    }

    /// Waits in IDLE on `folder` until the server reports a change, `timeout`
    /// passes, or `cancel` fires. Returns the session and whether something changed.
    pub async fn idle(
        mut self,
        folder: &str,
        timeout: Duration,
        cancel: &tokio::sync::Notify,
    ) -> Result<(Imap, bool)> {
        self.ensure_selected(folder).await?;
        let Imap {
            session,
            selected,
            has_move,
            has_condstore,
            gmail,
        } = self;
        let mut handle = session.idle();
        handle.init().await.context("starting IDLE")?;
        let (wait, stop) = handle.wait_with_timeout(timeout);
        let result = tokio::select! {
            r = wait => r,
            _ = cancel.notified() => {
                drop(stop);
                Ok(IdleResponse::ManualInterrupt)
            }
        };
        let changed = matches!(result?, IdleResponse::NewData(_));
        let session = handle.done().await.context("ending IDLE")?;
        Ok((
            Imap {
                session,
                selected,
                has_move,
                has_condstore,
                gmail,
            },
            changed,
        ))
    }
}

impl Backend for Imap {
    async fn connect(account: &AccountConfig, secret: &str) -> Result<Imap> {
        Imap::connect(account, secret).await
    }

    async fn logout(self) {
        Imap::logout(self).await
    }

    fn is_rejection(e: &anyhow::Error) -> bool {
        use async_imap::error::Error;
        e.chain().any(|c| {
            matches!(
                c.downcast_ref::<Error>(),
                Some(Error::No(_) | Error::Bad(_))
            )
        })
    }

    async fn list_folders(&mut self) -> Result<Vec<(String, Option<String>, Role)>> {
        Imap::list_folders(self).await
    }

    async fn select(&mut self, folder: &str) -> Result<FolderState> {
        Imap::select(self, folder).await
    }

    async fn fetch_new(&mut self, folder: &str, after: u32) -> Result<Vec<MessageSummary>> {
        Imap::fetch_new(self, folder, after).await
    }

    async fn fetch_latest(
        &mut self,
        folder: &str,
        exists: u32,
        count: u32,
    ) -> Result<Vec<MessageSummary>> {
        Imap::fetch_latest(self, folder, exists, count).await
    }

    async fn fetch_older(&mut self, folder: &str, before: u32, count: u32) -> Result<Vec<MessageSummary>> {
        Imap::fetch_older(self, folder, before, count).await
    }

    async fn fetch_headers(&mut self, folder: &str, uids: &[u32]) -> Result<Vec<MessageSummary>> {
        Imap::fetch_headers(self, folder, uids).await
    }

    async fn fetch_flags(&mut self, folder: &str, ranges: &[(u32, u32)], changed_since: Option<u64>) -> Result<Vec<Flags>> {
        Imap::fetch_flags(self, folder, ranges, changed_since).await
    }

    async fn existing_uids(&mut self, folder: &str, ranges: &[(u32, u32)]) -> Result<Vec<u32>> {
        Imap::existing_uids(self, folder, ranges).await
    }

    async fn fetch_threads(&mut self, folder: &str, uids: &[u32]) -> Result<Vec<(u32, u64)>> {
        Imap::fetch_threads(self, folder, uids).await
    }

    async fn search(&mut self, folder: &str, query: &str) -> Result<Vec<u32>> {
        Imap::search(self, folder, query).await
    }

    async fn fetch_bodies(&mut self, folder: &str, uids: &[u32]) -> Result<Vec<(u32, Vec<u8>)>> {
        Imap::fetch_bodies(self, folder, uids).await
    }

    async fn apply(&mut self, folder: &str, uid: u32, op: &PendingOp) -> Result<()> {
        match op {
            PendingOp::SetSeen(v) => self.store_flag(folder, uid, "\\Seen", *v).await,
            PendingOp::SetFlagged(v) => self.store_flag(folder, uid, "\\Flagged", *v).await,
            PendingOp::SetAnswered => self.store_flag(folder, uid, "\\Answered", true).await,
            PendingOp::Move(dest) => self.move_message(folder, uid, dest).await,
            PendingOp::Expunge => self.expunge(folder, uid).await,
            PendingOp::Relabel { add, remove } => self.relabel(folder, uid, add, remove).await,
        }
    }

    async fn send(
        account: &AccountConfig,
        secret: &str,
        raw: &[u8],
        recipients: &[String],
    ) -> Result<()> {
        crate::smtp::send(account, secret, raw, recipients).await
    }

    fn files_sent(account: &AccountConfig) -> bool {
        // Gmail files messages sent over SMTP itself; appending would duplicate them.
        account.smtp_host.ends_with("gmail.com")
    }

    async fn save_sent(&mut self, folder: &str, raw: &[u8]) -> Result<()> {
        self.append(folder, "(\\Seen)", raw).await
    }

    async fn save_draft(&mut self, folder: &str, raw: &[u8]) -> Result<()> {
        self.append(folder, "(\\Draft \\Seen)", raw).await
    }

    async fn find_message_id(&mut self, folder: &str, id: &str) -> Result<Vec<u32>> {
        self.ensure_selected(folder).await?;
        let mut uids: Vec<u32> = self.session.uid_search(format!("HEADER Message-ID {}", quote(id))).await?.into_iter().collect();
        self.confirm().await?;
        uids.sort_unstable();
        Ok(uids)
    }

    async fn create_folder(&mut self, name: &str) -> Result<()> {
        self.create(name).await
    }

    async fn wait_for_change(self, folder: &str, timeout: Duration) -> Result<(Imap, bool)> {
        let never = tokio::sync::Notify::new();
        let (mut imap, changed) = self.idle(folder, timeout, &never).await?;
        if !changed {
            imap.noop().await?;
        }
        Ok((imap, changed))
    }
}

/// `password` is the access token here; XOAUTH2 sends everything in the first response.
struct XOAuth2(String);

impl async_imap::Authenticator for XOAuth2 {
    type Response = String;
    fn process(&mut self, _challenge: &[u8]) -> String {
        // A second challenge carries the server's error details; an empty reply ends the exchange.
        std::mem::take(&mut self.0)
    }
}

/// Gmail labels, one per line; None when the server sent none (not Gmail).
fn labels_of(fetch: &Fetch) -> Option<String> {
    fetch.gmail_labels().map(|labels| labels.iter().map(|l| unescape(l)).collect::<Vec<_>>().join("\n"))
}

/// A quoted IMAP string's content as the parser leaves it, escapes still in: Gmail sends
/// `"\\\\Inbox"` for the label `\\Inbox`.
fn unescape(quoted: &str) -> String {
    let mut out = String::with_capacity(quoted.len());
    let mut chars = quoted.chars().peekable();
    while let Some(c) = chars.next() {
        // Only the two escapes there are: an atom's lone `\\Inbox` stays as it is.
        match (c, chars.peek()) {
            ('\\', Some(&next @ ('\\' | '"'))) => {
                out.push(next);
                chars.next();
            }
            _ => out.push(c),
        }
    }
    out
}

fn flags_of(fetch: &Fetch) -> (bool, bool, bool) {
    let (mut seen, mut flagged, mut answered) = (false, false, false);
    for flag in fetch.flags() {
        match flag {
            Flag::Seen => seen = true,
            Flag::Flagged => flagged = true,
            Flag::Answered => answered = true,
            _ => {}
        }
    }
    (seen, flagged, answered)
}

/// Guesses special-use roles for servers without RFC 6154 support.
fn role_from_name(name: &str) -> Role {
    let last = name
        .rsplit(['/', '.'])
        .next()
        .unwrap_or(name)
        .to_lowercase();
    match last.as_str() {
        "sent" | "sent items" | "sent messages" | "sent mail" | "gesendet"
        | "gesendete objekte" => Role::Sent,
        "drafts" | "entwürfe" => Role::Drafts,
        "trash"
        | "deleted"
        | "deleted items"
        | "deleted messages"
        | "papierkorb"
        | "gelöschte elemente" => Role::Trash,
        "junk" | "spam" | "junk e-mail" => Role::Junk,
        "archive" | "archiv" => Role::Archive,
        _ => Role::Other,
    }
}

/// `s` as an IMAP quoted string. A line break would end the command early; quotes and backslashes are escaped.
fn quote(s: &str) -> String {
    format!("\"{}\"", s.replace(|c: char| c.is_control(), " ").replace('\\', "\\\\").replace('"', "\\\""))
}

/// SEARCH criteria for the search box's text: Gmail's own syntax there, elsewhere every word
/// in the headers or body, and `from:address` in From.
fn search_criteria(query: &str, gmail: bool) -> String {
    let criteria = if gmail {
        format!("X-GM-RAW {}", quote(query))
    } else {
        query
            .split_whitespace()
            .map(|w| match w.get(..5) {
                Some(p) if p.eq_ignore_ascii_case("from:") && w.len() > 5 => format!("FROM {}", quote(&w[5..])),
                _ => format!("TEXT {}", quote(w)),
            })
            .collect::<Vec<_>>()
            .join(" ")
    };
    if query.is_ascii() { criteria } else { format!("CHARSET UTF-8 {criteria}") }
}

/// Compresses sorted UIDs into an IMAP sequence set like "1:5,9,12:13".
fn uid_set(uids: &[u32]) -> String {
    let mut sorted = uids.to_vec();
    sorted.sort_unstable();
    sorted.dedup();
    let mut parts = Vec::new();
    let mut i = 0;
    while i < sorted.len() {
        let start = sorted[i];
        let mut end = start;
        while i + 1 < sorted.len() && sorted[i + 1] == end + 1 {
            i += 1;
            end = sorted[i];
        }
        parts.push(if start == end {
            start.to_string()
        } else {
            format!("{start}:{end}")
        });
        i += 1;
    }
    parts.join(",")
}

/// Inclusive UID ranges as an IMAP sequence set. Scattered search hits make long sets; callers send
/// at most 500 ranges at a time, as servers limit command lines to a few kB.
fn range_set(ranges: &[(u32, u32)]) -> String {
    let parts: Vec<String> = ranges.iter().map(|(a, b)| if a == b { a.to_string() } else { format!("{a}:{b}") }).collect();
    parts.join(",")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uid_sets() {
        assert_eq!(uid_set(&[5, 1, 2, 3, 9, 12, 13]), "1:3,5,9,12:13");
        assert_eq!(uid_set(&[7]), "7");
    }

    #[test]
    fn unescapes_quoted_labels() {
        assert_eq!(unescape(r"\\Inbox"), r"\Inbox");
        assert_eq!(unescape(r#"say \"hi\""#), r#"say "hi""#);
        assert_eq!(unescape("[Imap]/Archiv"), "[Imap]/Archiv");
        assert_eq!(unescape(r"\Important"), r"\Important");
    }

    #[test]
    fn search_criteria_quote_input() {
        assert_eq!(search_criteria("from:a@b.org lunch", false), r#"FROM "a@b.org" TEXT "lunch""#);
        assert_eq!(search_criteria("say \"hi\"\r\nX", true), r#"X-GM-RAW "say \"hi\"  X""#);
        assert_eq!(search_criteria("Grüße", false), r#"CHARSET UTF-8 TEXT "Grüße""#);
        assert_eq!(quote("<a\"b>\r\nX"), r#""<a\"b>  X""#);
    }

    #[test]
    fn roles_from_names() {
        assert_eq!(role_from_name("INBOX.Sent"), Role::Sent);
        assert_eq!(role_from_name("[Gmail]/Spam"), Role::Junk);
        assert_eq!(role_from_name("Projects"), Role::Other);
    }
}
