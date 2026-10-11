//! Moving mail between folders (Gmail labels) through the engine, as the app does, on a real
//! Gmail test account. After each move the cache must list what the server lists, in every
//! folder involved, without waiting for a later sync. Needs a build with the Google client
//! (see docs/oauth-google.md) and the account's refresh token (from `google_live`):
//!   LIVE_GOOGLE_EMAIL=… LIVE_GOOGLE_REFRESH_TOKEN=… \
//!     cargo test -p mailcore --test gmail_move_live -- --ignored --nocapture --test-threads=1
//! Each test appends its own messages, marked with a unique word, and cleans up after itself.

use std::sync::mpsc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use mailcore::engine::{self, Command, Engine, Event};
use mailcore::imap::Imap;
use mailcore::store::{Role, Store};
use mailcore::{AccountConfig, Auth, auth};

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap()
}

/// A Gmail account with test messages in its inbox, an engine syncing it, and an empty label.
struct Live {
    account: AccountConfig,
    refresh: String,
    /// The word in every test message's subject.
    mark: String,
    label: String,
    all_mail: String,
    trash: String,
    store: Store,
    engine: Engine,
    events: mpsc::Receiver<Event>,
    _dir: tempfile::TempDir,
}

impl Live {
    fn start(messages: usize) -> Live {
        mailcore::init_tls();
        let email = std::env::var("LIVE_GOOGLE_EMAIL").expect("set LIVE_GOOGLE_EMAIL");
        let refresh = std::env::var("LIVE_GOOGLE_REFRESH_TOKEN").expect("set LIVE_GOOGLE_REFRESH_TOKEN (see google_live)");
        let account = AccountConfig::guess(&email);
        assert_eq!(account.auth, Auth::Google);
        assert!(auth::available(Auth::Google), "built without MAILCLIENT_GOOGLE_CLIENT_ID");
        // Credentials::load reads the override instead of the keyring.
        // SAFETY: the tests run one at a time (--test-threads=1), before the engine starts.
        unsafe { std::env::set_var("MAILCLIENT_PASSWORD", &refresh) };

        let stamp = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis();
        let mark = format!("mvtest{stamp}");
        let label = format!("mailclient-move-{stamp}");
        let (all_mail, trash) = runtime().block_on(async {
            let mut imap = imap(&account, &refresh).await;
            let folders = imap.list_folders().await.unwrap();
            let role = |r: Role| folders.iter().find(|f| f.2 == r).map(|f| f.0.clone()).unwrap_or_else(|| panic!("no {r:?} folder"));
            imap.create(&label).await.unwrap();
            for i in 1..=messages {
                let raw = format!(
                    "From: Move Test <{email}>\r\nTo: {email}\r\nSubject: {mark} message {i}\r\n\
                     Date: {}\r\nMessage-ID: <{mark}-{i}@mailclient.test>\r\n\r\nMoved around by gmail_move_live.\r\n",
                    chrono::Utc::now().to_rfc2822()
                );
                imap.append("INBOX", "()", raw.as_bytes()).await.unwrap();
            }
            imap.logout().await;
            (role(Role::All), role(Role::Trash))
        });

        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("cache.sqlite");
        let store = Store::open(&db).unwrap();
        let (tx, events) = mpsc::channel();
        let engine = Engine::start(account.clone(), db, move |e| {
            let _ = tx.send(e);
        });
        let live = Live { account, refresh, mark, label, all_mail, trash, store, engine, events, _dir: dir };
        live.wait_event("folder list", |e| matches!(e, Event::FoldersChanged));
        // The app syncs a folder when it is opened; these are the ones the tests look at.
        for folder in [live.all_mail.clone(), live.label.clone()] {
            live.engine.send(Command::OpenFolder(folder));
        }
        live.wait_until("test messages cached", || live.local("INBOX").len() == messages && live.local(&live.all_mail).len() == messages);
        live
    }

    fn wait_event(&self, what: &str, pred: impl Fn(&Event) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.events.recv_timeout(left).unwrap_or_else(|_| panic!("timed out waiting for {what}")) {
                Event::Error(e) => panic!("engine error while waiting for {what}: {e}"),
                e if pred(&e) => return,
                _ => {}
            }
        }
    }

    fn wait_until(&self, what: &str, mut cond: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(60);
        while !cond() {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            // Engine errors would otherwise only show as a timeout.
            while let Ok(e) = self.events.try_recv() {
                if let Event::Error(e) = e {
                    panic!("engine error while waiting for {what}: {e}");
                }
            }
            std::thread::sleep(Duration::from_millis(300));
        }
    }

    /// Subjects of this test's messages the app lists in `folder`, sorted.
    fn local(&self, folder: &str) -> Vec<String> {
        let mut subjects: Vec<String> =
            self.store.messages(folder, 1000).unwrap().into_iter().map(|m| m.subject).filter(|s| s.contains(&self.mark)).collect();
        subjects.sort();
        subjects
    }

    /// Subjects of this test's messages the server has in `folder`, sorted.
    fn server(&self, folder: &str) -> Vec<String> {
        runtime().block_on(async {
            let mut imap = imap(&self.account, &self.refresh).await;
            let uids = imap.search(folder, &self.mark).await.unwrap();
            let mut subjects: Vec<String> =
                imap.fetch_headers(folder, &uids).await.unwrap().into_iter().map(|m| m.subject).collect();
            imap.logout().await;
            subjects.sort();
            subjects
        })
    }

    /// Moves the messages with these numbers from `from` to `to` the way the app does: locally
    /// at once (out of All Mail as a change of labels), then sent with a Flush.
    fn move_messages(&self, numbers: &[usize], from: &str, to: &str) {
        let wanted: Vec<String> = numbers.iter().map(|i| format!("{} message {i}", self.mark)).collect();
        let found: Vec<_> =
            self.store.messages(from, 1000).unwrap().into_iter().filter(|m| wanted.contains(&m.subject)).collect();
        assert_eq!(found.len(), numbers.len(), "not all of {numbers:?} cached in {from}");
        let mut store = Store::open(&self.store_path()).unwrap();
        for m in found {
            let op = engine::move_op(&store, true, &m, to).unwrap();
            store.apply_local(from, m.uid, &op).unwrap();
        }
        self.engine.send(Command::Flush);
        self.wait_until("moves sent", || self.store.pending_ops().unwrap().is_empty());
    }

    fn store_path(&self) -> std::path::PathBuf {
        self._dir.path().join("cache.sqlite")
    }

    /// Labels of this test's messages in All Mail, as the app shows them: "subject: label, label".
    fn local_labels(&self) -> Vec<String> {
        let mut found: Vec<String> = self
            .store
            .messages(&self.all_mail, 1000)
            .unwrap()
            .into_iter()
            .filter(|m| m.subject.contains(&self.mark))
            .map(|m| labelled(&m.subject, self.store.labels(&m.folder, m.uid).unwrap()))
            .collect();
        found.sort();
        found
    }

    fn server_labels(&self) -> Vec<String> {
        runtime().block_on(async {
            let mut imap = imap(&self.account, &self.refresh).await;
            let uids = imap.search(&self.all_mail, &self.mark).await.unwrap();
            let headers = imap.fetch_headers(&self.all_mail, &uids).await.unwrap();
            imap.logout().await;
            let mut found: Vec<String> =
                headers.into_iter().map(|m| labelled(&m.subject, m.labels.lines().map(str::to_string).collect())).collect();
            found.sort();
            found
        })
    }

    /// The cache agrees with the server in each of these folders, and on the labels, without a sync in between.
    fn assert_in_step(&self, folders: &[&str]) {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let mut differ: Vec<(String, Vec<String>, Vec<String>)> = folders
                .iter()
                .map(|f| (f.to_string(), self.local(f), self.server(f)))
                .filter(|(_, local, server)| local != server)
                .collect();
            let (local, server) = (self.local_labels(), self.server_labels());
            if local != server {
                differ.push(("labels in All Mail".into(), local, server));
            }
            if differ.is_empty() {
                return;
            }
            if Instant::now() > deadline {
                let report: Vec<String> =
                    differ.iter().map(|(f, l, s)| format!("{f}:\n  app    {l:?}\n  server {s:?}")).collect();
                panic!("the app's cache and Gmail differ:\n{}", report.join("\n"));
            }
            std::thread::sleep(Duration::from_secs(1));
        }
    }
}

impl Drop for Live {
    /// The test messages go to the trash, the label away.
    fn drop(&mut self) {
        runtime().block_on(async {
            let mut imap = imap(&self.account, &self.refresh).await;
            for folder in [self.all_mail.as_str(), "INBOX"] {
                for uid in imap.search(folder, &self.mark).await.unwrap_or_default() {
                    let _ = imap.move_message(folder, uid, &self.trash).await;
                }
            }
            let _ = imap.delete(&self.label).await;
            imap.logout().await;
        });
    }
}

/// "subject: label, label", labels sorted, Important and Starred left out (Gmail sets those itself).
fn labelled(subject: &str, labels: Vec<String>) -> String {
    let mut labels: Vec<String> = labels.into_iter().filter(|l| l != "\\Important" && l != "\\Starred").collect();
    labels.sort();
    format!("{subject}: {}", labels.join(", "))
}

async fn imap(account: &AccountConfig, refresh: &str) -> Imap {
    let tokens = auth::refresh(Auth::Google, refresh).await.unwrap();
    Imap::connect(account, &tokens.access).await.unwrap()
}

#[test]
#[ignore = "needs a Gmail test account (LIVE_GOOGLE_EMAIL, LIVE_GOOGLE_REFRESH_TOKEN)"]
fn move_one_message_to_a_label_and_back() {
    let live = Live::start(1);
    let (label, all) = (live.label.clone(), live.all_mail.clone());
    live.move_messages(&[1], "INBOX", &label);
    live.assert_in_step(&["INBOX", &label, &all]);
    live.move_messages(&[1], &label, "INBOX");
    live.assert_in_step(&["INBOX", &label, &all]);
}

#[test]
#[ignore = "needs a Gmail test account (LIVE_GOOGLE_EMAIL, LIVE_GOOGLE_REFRESH_TOKEN)"]
fn move_several_messages_to_a_label() {
    let live = Live::start(3);
    let (label, all) = (live.label.clone(), live.all_mail.clone());
    live.move_messages(&[1, 3], "INBOX", &label);
    live.assert_in_step(&["INBOX", &label, &all]);
}

/// What the app does in a search over all folders: the row found is the All Mail copy.
#[test]
#[ignore = "needs a Gmail test account (LIVE_GOOGLE_EMAIL, LIVE_GOOGLE_REFRESH_TOKEN)"]
fn move_from_all_mail_to_a_label() {
    let live = Live::start(2);
    let (label, all) = (live.label.clone(), live.all_mail.clone());
    live.move_messages(&[1, 2], &all, &label);
    live.assert_in_step(&["INBOX", &label, &all]);
}

#[test]
#[ignore = "needs a Gmail test account (LIVE_GOOGLE_EMAIL, LIVE_GOOGLE_REFRESH_TOKEN)"]
fn move_from_a_label_to_the_trash() {
    let live = Live::start(2);
    let (label, all, trash) = (live.label.clone(), live.all_mail.clone(), live.trash.clone());
    live.move_messages(&[1, 2], "INBOX", &label);
    live.assert_in_step(&["INBOX", &label, &all]);
    live.engine.send(Command::OpenFolder(trash.clone()));
    live.move_messages(&[2], &label, &trash);
    live.assert_in_step(&["INBOX", &label, &all, &trash]);
}

/// Archiving from All Mail: the inbox label comes off, the message stays in All Mail.
#[test]
#[ignore = "needs a Gmail test account (LIVE_GOOGLE_EMAIL, LIVE_GOOGLE_REFRESH_TOKEN)"]
fn archive_from_all_mail() {
    let live = Live::start(2);
    let (label, all) = (live.label.clone(), live.all_mail.clone());
    live.move_messages(&[1], &all, &all);
    live.assert_in_step(&["INBOX", &label, &all]);
    assert_eq!(live.local("INBOX").len(), 1, "the archived one is still in the inbox");
}

