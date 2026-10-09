//! End-to-end test against a local GreenMail server. Start it first:
//!   scripts/test-mailserver.sh && cargo test -p mailcore --test greenmail -- --ignored

use std::path::PathBuf;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use mailcore::engine::{self, Command, Engine, Event};
use mailcore::imap::Imap;
use mailcore::mime::{self, Draft};
use mailcore::store::{PendingOp, Store};
use mailcore::{AccountConfig, Security};

fn wait_until(what: &str, mut cond: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while !cond() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(200));
    }
}

#[test]
#[ignore = "needs scripts/test-mailserver.sh"]
fn send_sync_flag_delete_idle() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/test-mailserver");
    // SAFETY: set before any other thread starts.
    unsafe {
        std::env::set_var("MAILCLIENT_EXTRA_CA", root.join("ca.pem"));
        std::env::set_var("MAILCLIENT_PASSWORD", "any");
    }
    mailcore::init_tls();

    // Fresh mailbox per run; GreenMail creates users on first login.
    let user = format!(
        "t{}@localhost",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis()
    );
    let account = AccountConfig {
        name: "Test".into(),
        email: user.clone(),
        username: user.clone(),
        imap_host: "localhost".into(),
        imap_port: 3993,
        imap_security: Security::Tls,
        smtp_host: "localhost".into(),
        smtp_port: 3465,
        smtp_security: Security::Tls,
        auth: mailcore::Auth::Password,
    };
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("cache.sqlite");
    // Create the cache before the engine, as the app does: two connections racing to
    // switch a new file to WAL can fail with "database is locked".
    let mut store = Store::open(&db).unwrap();
    let (tx, rx) = mpsc::channel();
    let engine = Engine::start(account.clone(), db.clone(), move |e| {
        let _ = tx.send(e);
    });
    let wait_event = |pred: &dyn Fn(&Event) -> bool| loop {
        match rx
            .recv_timeout(Duration::from_secs(20))
            .expect("engine event")
        {
            Event::Error(e) => panic!("engine error: {e}"),
            e if pred(&e) => return e,
            _ => {}
        }
    };
    wait_event(&|e| matches!(e, Event::FoldersChanged));

    // Send to ourselves, with an attachment.
    let attachment = dir.path().join("notes.txt");
    std::fs::write(&attachment, "attached text").unwrap();
    let draft = Draft {
        to: user.clone(),
        subject: "Hello GreenMail".into(),
        body: "first body".into(),
        attachments: vec![attachment],
        ..Default::default()
    };
    engine.send(Command::Send {
        id: 1,
        draft,
        replied_to: None,
    });
    if let Event::SendFailed(_, e) =
        wait_event(&|e| matches!(e, Event::Sent(1) | Event::SendFailed(..)))
    {
        panic!("send failed: {e}");
    }

    // Sync until it arrives, with its body prefetched.
    wait_until("message in cache", || {
        engine.send(Command::SyncAll);
        std::thread::sleep(Duration::from_millis(300));
        store
            .messages("INBOX", 10)
            .unwrap()
            .iter()
            .any(|m| m.subject == "Hello GreenMail")
    });
    let msg = store.messages("INBOX", 10).unwrap().remove(0);
    assert_eq!(msg.from_addr, user);
    wait_until("body prefetched", || {
        store.raw("INBOX", msg.uid).unwrap().is_some()
    });
    let body = mime::body_from_raw(&store.raw("INBOX", msg.uid).unwrap().unwrap());
    assert!(body.text.contains("first body"), "{}", body.text);
    assert_eq!(
        body.attachments
            .iter()
            .map(|a| a.name.as_str())
            .collect::<Vec<_>>(),
        ["notes.txt"]
    );

    // Direct server view, to check what the engine pushed.
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let server_flags = |uid: u32| {
        rt.block_on(async {
            let mut imap = Imap::connect(&account, "any").await.unwrap();
            let state = imap.select("INBOX").await.unwrap();
            let flags = imap.fetch_flags("INBOX", uid, uid).await.unwrap();
            imap.logout().await;
            (state.exists, flags)
        })
    };

    // Flag locally, flush, and check the server.
    store
        .apply_local("INBOX", msg.uid, &PendingOp::SetFlagged(true))
        .unwrap();
    engine.send(Command::Flush);
    wait_until("flag pushed", || store.pending_ops().unwrap().is_empty());
    assert!(
        server_flags(msg.uid)
            .1
            .iter()
            .any(|f| f.0 == msg.uid && f.2),
        "flag not on server"
    );

    // Delete (no Trash folder on GreenMail, so this expunges).
    let op = engine::delete_op(&store, "INBOX").unwrap();
    assert_eq!(op, PendingOp::Expunge);
    store.apply_local("INBOX", msg.uid, &op).unwrap();
    engine.send(Command::Flush);
    wait_until("delete pushed", || store.pending_ops().unwrap().is_empty());
    assert_eq!(server_flags(msg.uid).0, 0, "message still on server");

    // New mail must show up through IDLE alone, without asking for a sync.
    let draft = Draft {
        to: user.clone(),
        subject: "Via IDLE".into(),
        body: "second".into(),
        ..Default::default()
    };
    engine.send(Command::Send {
        id: 2,
        draft,
        replied_to: None,
    });
    wait_event(&|e| matches!(e, Event::Sent(2)));
    wait_until("IDLE-triggered sync", || {
        store
            .messages("INBOX", 10)
            .unwrap()
            .iter()
            .any(|m| m.subject == "Via IDLE")
    });
}
