//! Sync against a server with CONDSTORE (RFC 7162), a local Dovecot: only changed flags are
//! fetched, and expunges are found by the count or a UID SEARCH. Start it first (it uses the test
//! CA of scripts/test-mailserver.sh):
//!   scripts/test-dovecot.sh && cargo test -p mailcore --test dovecot -- --ignored

use std::path::PathBuf;
use std::sync::mpsc;
use std::time::Duration;

use mailcore::engine::{self, Engine, Event};
use mailcore::imap::Imap;
use mailcore::store::Store;
use mailcore::{AccountConfig, Security};

#[test]
#[ignore = "needs scripts/test-dovecot.sh"]
fn condstore_sync() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/test-mailserver");
    // SAFETY: set before any other thread starts.
    unsafe {
        std::env::set_var("MAILCLIENT_EXTRA_CA", root.join("ca.pem"));
        std::env::set_var("MAILCLIENT_PASSWORD", "pass");
    }
    mailcore::init_tls();
    // Fresh mailbox per run; the container takes any user name with USER_PASSWORD.
    let user = format!("t{}", std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis());
    let account = AccountConfig {
        name: "Test".into(),
        email: format!("{user}@localhost"),
        username: user.clone(),
        imap_host: "localhost".into(),
        imap_port: 31993,
        imap_security: Security::Tls,
        smtp_host: "localhost".into(),
        smtp_port: 1,
        smtp_security: Security::Tls,
        auth: mailcore::Auth::Password,
        caldav_url: String::new(),
        color: String::new(),
    };
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    let mail = |n: u32| format!("From: a@b.c\r\nTo: {user}@localhost\r\nSubject: m{n}\r\nMessage-ID: <m{n}-{user}@x>\r\n\r\nbody {n}\r\n").into_bytes();
    let mut imap = rt.block_on(async {
        let mut imap = Imap::connect(&account, "pass").await.unwrap();
        for n in 1..=3 {
            imap.append("INBOX", "(\\Seen)", &mail(n)).await.unwrap();
        }
        let state = imap.select("INBOX").await.unwrap();
        eprintln!("modseq {:?} exists {}", state.highest_modseq, state.exists);
        assert!(state.highest_modseq.is_some());
        imap
    });

    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("cache.sqlite");
    let store = Store::open(&db).unwrap();
    let (tx, rx) = mpsc::channel();
    let engine = Engine::start(account.clone(), db.clone(), move |e| {
        let _ = tx.send(e);
    });
    loop {
        match rx.recv_timeout(Duration::from_secs(20)).unwrap() {
            Event::Error(e) => panic!("{e}"),
            Event::MessagesChanged(f) if f == "INBOX" => break,
            _ => {}
        }
    }
    drop(engine);
    std::thread::sleep(Duration::from_millis(500));
    let mut uids = store.uids("INBOX").unwrap();
    uids.sort();
    assert_eq!(uids, [1, 2, 3]);

    rt.block_on(async {
        let before = imap.select("INBOX").await.unwrap().highest_modseq.unwrap();
        imap.store_flag("INBOX", 2, "\\Seen", false).await.unwrap();
        let changed = imap.fetch_flags("INBOX", &[(1, 3)], Some(before)).await.unwrap();
        eprintln!("changed since {before}: {:?}", changed.iter().map(|f| (f.uid, f.seen)).collect::<Vec<_>>());
        assert_eq!(changed.len(), 1);
        assert_eq!((changed[0].uid, changed[0].seen), (2, false));
        imap.append("INBOX", "", &mail(4)).await.unwrap();
        // Counts add up: no UID SEARCH.
        let arrived = engine::check_inbox(&account, &db).await.unwrap();
        assert_eq!(arrived.iter().map(|m| m.uid).collect::<Vec<_>>(), [4]);
        let unread: Vec<u32> = store.messages("INBOX", 10).unwrap().iter().filter(|m| !m.seen).map(|m| m.uid).collect();
        assert_eq!(unread.len(), 2, "{unread:?}");

        // An expunge: the UID SEARCH finds it.
        imap.expunge("INBOX", 1).await.unwrap();
        let mut left = imap.existing_uids("INBOX", &[(1, 4)]).await.unwrap();
        left.sort();
        assert_eq!(left, [2, 3, 4]);
        engine::check_inbox(&account, &db).await.unwrap();
        let mut uids = store.uids("INBOX").unwrap();
        uids.sort();
        assert_eq!(uids, [2, 3, 4]);
    });
}
