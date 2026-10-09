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

use crate::config::{AccountConfig, Security};
use crate::mime;
use crate::store::{MessageSummary, Role};

pub type Session = async_imap::Session<TlsStream<TcpStream>>;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);

/// PEM file of one extra trusted CA (self-hosted servers, local test server), from `MAILCLIENT_EXTRA_CA`.
pub fn extra_ca_pem() -> Result<Option<Vec<u8>>> {
    match std::env::var_os("MAILCLIENT_EXTRA_CA") {
        Some(path) => Ok(Some(std::fs::read(&path).with_context(|| format!("reading MAILCLIENT_EXTRA_CA {path:?}"))?)),
        None => Ok(None),
    }
}

pub fn tls_connector() -> Result<TlsConnector> {
    use rustls::pki_types::{CertificateDer, pem::PemObject};
    let mut roots = rustls::RootCertStore { roots: webpki_roots::TLS_SERVER_ROOTS.to_vec() };
    if let Some(pem) = extra_ca_pem()? {
        for cert in CertificateDer::pem_slice_iter(&pem) {
            roots.add(cert.context("parsing MAILCLIENT_EXTRA_CA")?)?;
        }
    }
    let config = rustls::ClientConfig::builder().with_root_certificates(roots).with_no_client_auth();
    Ok(TlsConnector::from(Arc::new(config)))
}

async fn tls_handshake(tcp: TcpStream, host: &str) -> Result<TlsStream<TcpStream>> {
    let name = rustls::pki_types::ServerName::try_from(host.to_string()).context("invalid server name")?;
    tls_connector()?.connect(name, tcp).await.with_context(|| format!("TLS handshake with {host}"))
}

pub struct Imap {
    session: Session,
    selected: Option<String>,
    has_move: bool,
}

pub struct FolderState {
    pub exists: u32,
    pub uid_validity: Option<u32>,
}

impl Imap {
    pub async fn connect(account: &AccountConfig, password: &str) -> Result<Imap> {
        let addr = (account.imap_host.as_str(), account.imap_port);
        let tcp = tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(addr))
            .await
            .map_err(|_| anyhow!("timed out connecting to {}", account.imap_host))?
            .with_context(|| format!("connecting to {}:{}", account.imap_host, account.imap_port))?;

        let tls = match account.imap_security {
            Security::Tls => {
                let tls = tls_handshake(tcp, &account.imap_host).await?;
                let mut client = async_imap::Client::new(tls);
                client.read_response().await?.ok_or_else(|| anyhow!("server closed connection before greeting"))?;
                client.into_inner()
            }
            Security::StartTls => {
                let mut client = async_imap::Client::new(tcp);
                client.read_response().await?.ok_or_else(|| anyhow!("server closed connection before greeting"))?;
                client.run_command_and_check_ok("STARTTLS", None).await.context("STARTTLS")?;
                tls_handshake(client.into_inner(), &account.imap_host).await?
            }
        };

        let client = async_imap::Client::new(tls);
        let mut session = client
            .login(&account.username, password)
            .await
            .map_err(|(e, _)| anyhow!("IMAP login failed: {e}"))?;
        let caps = session.capabilities().await?;
        let has_move = caps.has_str("MOVE");
        Ok(Imap { session, selected: None, has_move })
    }

    pub async fn logout(mut self) {
        let _ = self.session.logout().await;
    }

    pub async fn list_folders(&mut self) -> Result<Vec<(String, Option<String>, Role)>> {
        let names: Vec<_> = self.session.list(Some(""), Some("*")).await?.try_collect().await?;
        self.confirm().await?;
        let mut folders = Vec::new();
        for name in &names {
            if name.attributes().iter().any(|a| matches!(a, NameAttribute::NoSelect)) {
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
            folders.push((name.name().to_string(), name.delimiter().map(str::to_string), role));
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
        let mailbox = self.session.select(folder).await.with_context(|| format!("selecting {folder}"))?;
        self.selected = Some(folder.to_string());
        // Every SELECT response carries UIDVALIDITY (RFC 3501); none means the reply was cut off.
        if mailbox.uid_validity.is_none() {
            self.selected = None;
            return Err(anyhow!("connection to server lost (no SELECT response for {folder})"));
        }
        Ok(FolderState { exists: mailbox.exists, uid_validity: mailbox.uid_validity })
    }

    async fn ensure_selected(&mut self, folder: &str) -> Result<()> {
        if self.selected.as_deref() != Some(folder) {
            self.select(folder).await?;
        }
        Ok(())
    }

    const SUMMARY_QUERY: &str = "(UID FLAGS INTERNALDATE BODY.PEEK[HEADER])";

    fn summary(folder: &str, fetch: &Fetch) -> Option<MessageSummary> {
        let uid = fetch.uid?;
        let date = fetch.internal_date().map(|d| d.timestamp());
        let mut s = mime::summary_from_headers(folder, uid, fetch.header().unwrap_or_default(), date);
        let (seen, flagged, answered) = flags_of(fetch);
        s.seen = seen;
        s.flagged = flagged;
        s.answered = answered;
        Some(s)
    }

    /// Headers of messages with UID greater than `after`.
    pub async fn fetch_new(&mut self, folder: &str, after: u32) -> Result<Vec<MessageSummary>> {
        self.ensure_selected(folder).await?;
        let fetches: Vec<Fetch> =
            self.session.uid_fetch(format!("{}:*", after + 1), Self::SUMMARY_QUERY).await?.try_collect().await?;
        self.confirm().await?;
        // "n:*" always returns the last message, even when its UID is below n.
        Ok(fetches.iter().filter_map(|f| Self::summary(folder, f)).filter(|s| s.uid > after).collect())
    }

    /// Headers of the newest `count` messages, by sequence number.
    pub async fn fetch_latest(&mut self, folder: &str, exists: u32, count: u32) -> Result<Vec<MessageSummary>> {
        self.ensure_selected(folder).await?;
        if exists == 0 {
            return Ok(Vec::new());
        }
        let start = exists.saturating_sub(count) + 1;
        let fetches: Vec<Fetch> =
            self.session.fetch(format!("{start}:*"), Self::SUMMARY_QUERY).await?.try_collect().await?;
        self.confirm().await?;
        Ok(fetches.iter().filter_map(|f| Self::summary(folder, f)).collect())
    }

    /// Current flags of all messages with UID in `from..=to`, as (uid, seen, flagged, answered).
    pub async fn fetch_flags(&mut self, folder: &str, from: u32, to: u32) -> Result<Vec<(u32, bool, bool, bool)>> {
        self.ensure_selected(folder).await?;
        let fetches: Vec<Fetch> =
            self.session.uid_fetch(format!("{from}:{to}"), "(UID FLAGS)").await?.try_collect().await?;
        self.confirm().await?;
        Ok(fetches
            .iter()
            .filter_map(|f| {
                let (seen, flagged, answered) = flags_of(f);
                f.uid.filter(|uid| (from..=to).contains(uid)).map(|uid| (uid, seen, flagged, answered))
            })
            .collect())
    }

    /// Full RFC 822 source of messages, without setting \Seen.
    pub async fn fetch_bodies(&mut self, folder: &str, uids: &[u32]) -> Result<Vec<(u32, Vec<u8>)>> {
        if uids.is_empty() {
            return Ok(Vec::new());
        }
        self.ensure_selected(folder).await?;
        let fetches: Vec<Fetch> =
            self.session.uid_fetch(uid_set(uids), "(UID BODY.PEEK[])").await?.try_collect().await?;
        self.confirm().await?;
        Ok(fetches.iter().filter_map(|f| Some((f.uid?, f.body()?.to_vec()))).collect())
    }

    pub async fn store_flag(&mut self, folder: &str, uid: u32, flag: &str, on: bool) -> Result<()> {
        self.ensure_selected(folder).await?;
        let op = if on { "+FLAGS.SILENT" } else { "-FLAGS.SILENT" };
        let _: Vec<_> = self.session.uid_store(uid.to_string(), format!("{op} ({flag})")).await?.try_collect().await?;
        self.confirm().await
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
        let _: Vec<_> = self.session.uid_expunge(uid.to_string()).await?.try_collect().await?;
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

    /// async-imap 0.12 ends a response stream silently when the server has closed the
    /// connection, so a FETCH/STORE on a dead connection returns `Ok` with partial or no
    /// data. NOOP does check for the tagged reply; running it afterwards turns a lost
    /// connection into an error instead of "the server has no messages".
    async fn confirm(&mut self) -> Result<()> {
        self.session.noop().await.context("connection to server lost")
    }

    pub async fn noop(&mut self) -> Result<()> {
        self.session.noop().await?;
        Ok(())
    }

    /// Waits in IDLE on `folder` until the server reports a change, `timeout`
    /// passes, or `cancel` fires. Returns the session and whether something changed.
    pub async fn idle(mut self, folder: &str, timeout: Duration, cancel: &tokio::sync::Notify) -> Result<(Imap, bool)> {
        self.ensure_selected(folder).await?;
        let Imap { session, selected, has_move } = self;
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
        Ok((Imap { session, selected, has_move }, changed))
    }
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
    let last = name.rsplit(['/', '.']).next().unwrap_or(name).to_lowercase();
    match last.as_str() {
        "sent" | "sent items" | "sent messages" | "sent mail" | "gesendet" | "gesendete objekte" => Role::Sent,
        "drafts" | "entwürfe" => Role::Drafts,
        "trash" | "deleted" | "deleted items" | "deleted messages" | "papierkorb" | "gelöschte elemente" => {
            Role::Trash
        }
        "junk" | "spam" | "junk e-mail" => Role::Junk,
        "archive" | "archiv" => Role::Archive,
        _ => Role::Other,
    }
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
        parts.push(if start == end { start.to_string() } else { format!("{start}:{end}") });
        i += 1;
    }
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
    fn roles_from_names() {
        assert_eq!(role_from_name("INBOX.Sent"), Role::Sent);
        assert_eq!(role_from_name("[Gmail]/Spam"), Role::Junk);
        assert_eq!(role_from_name("Projects"), Role::Other);
    }
}
