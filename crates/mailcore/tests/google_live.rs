//! Live tests against a real Gmail test account. Need a build with the Google client
//! (see docs/oauth-google.md):
//!   LIVE_GOOGLE_EMAIL=you@gmail.com cargo test -p mailcore --test google_live -- --ignored --nocapture --test-threads=1
//! Without LIVE_GOOGLE_REFRESH_TOKEN they open the browser sign-in once and print the refresh
//! token; export it to skip the browser on later runs (it expires after 7 days while the
//! Google app is in "Testing").

use std::sync::OnceLock;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use mailcore::calendar;
use mailcore::ical::{self, Event};
use mailcore::imap::Imap;
use mailcore::store::{Role, Store};
use mailcore::{AccountConfig, Auth, auth};

/// The test account and a refresh token for it; signs in through the browser once per run.
fn sign_in() -> (AccountConfig, String) {
    static REFRESH: OnceLock<String> = OnceLock::new();
    mailcore::init_tls();
    let email = std::env::var("LIVE_GOOGLE_EMAIL").expect("set LIVE_GOOGLE_EMAIL");
    let account = AccountConfig::guess(&email);
    assert_eq!(account.auth, Auth::Google);
    assert!(auth::available(Auth::Google), "built without MAILCLIENT_GOOGLE_CLIENT_ID");
    let refresh = REFRESH.get_or_init(|| {
        if let Ok(token) = std::env::var("LIVE_GOOGLE_REFRESH_TOKEN") {
            return token;
        }
        runtime().block_on(async {
            let sign_in = auth::SignIn::start(Auth::Google, &email).await.unwrap();
            eprintln!("Sign in: {}", sign_in.url);
            let _ = std::process::Command::new("xdg-open").arg(&sign_in.url).spawn();
            let tokens = tokio::time::timeout(Duration::from_secs(300), sign_in.finish()).await.expect("sign-in timed out").unwrap();
            let refresh = tokens.refresh.expect("Google returned no refresh token");
            eprintln!("LIVE_GOOGLE_REFRESH_TOKEN={refresh}");
            refresh
        })
    });
    // Credentials::load reads the override instead of the keyring.
    // SAFETY: the tests run one at a time (--test-threads=1), before any engine thread.
    unsafe { std::env::set_var("MAILCLIENT_PASSWORD", refresh) };
    (account, refresh.clone())
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap()
}

fn stamp() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs()
}

#[test]
#[ignore = "needs a Gmail test account and MAILCLIENT_GOOGLE_CLIENT_ID/_SECRET at build time"]
fn google_sign_in_imap_smtp() {
    let (account, refresh) = sign_in();
    let email = account.email.clone();
    runtime().block_on(async {
        // The path the engine takes on every connection.
        let tokens = auth::refresh(Auth::Google, &refresh).await.unwrap();
        assert!(tokens.refresh.is_none(), "Google does not rotate refresh tokens");

        let mut imap = Imap::connect(&account, &tokens.access).await.unwrap();
        let folders = imap.list_folders().await.unwrap();
        for role in [Role::Inbox, Role::Sent, Role::Trash] {
            assert!(folders.iter().any(|f| f.2 == role), "no {role:?} folder in {folders:?}");
        }
        imap.logout().await;

        let raw = format!("From: {email}\r\nTo: {email}\r\nSubject: mailclient live test {}\r\n\r\nGoogle XOAUTH2 SMTP works.\r\n", stamp());
        mailcore::smtp::send(&account, &tokens.access, raw.as_bytes(), &[email.clone()]).await.unwrap();
    });
}

/// Google Calendar over CalDAV: discovery, then an event created, synced back, moved, renamed
/// and deleted again.
#[test]
#[ignore = "needs a Gmail test account and MAILCLIENT_GOOGLE_CLIENT_ID/_SECRET at build time"]
fn google_calendar_create_edit_delete() {
    let (account, _) = sign_in();
    runtime().block_on(async {
        let creds = auth::Credentials::load(&account).unwrap();
        let mut store = Store::open_in_memory().unwrap();
        calendar::sync(&account, &creds, &mut store).await.unwrap();
        let calendars = store.calendars().unwrap();
        eprintln!("calendars: {calendars:?}");
        assert!(!calendars.is_empty(), "no calendars found");

        // Two days ahead, on the hour: inside the synced window, clear of today's real events.
        let stamp = stamp();
        let start = (stamp as i64 / 3600 + 48) * 3600;
        let title = format!("mailclient live {stamp}");
        let event = Event {
            uid: format!("mailclient-live-{stamp}@example.org"),
            summary: title.clone(),
            location: "Nowhere".into(),
            description: "Created by the mailclient live test; deleted at its end.".into(),
            start,
            end: start + 3600,
            ..Default::default()
        };
        let find = |store: &Store, summary: &str| {
            store.events(start - 86400, start + 3 * 86400).unwrap().into_iter().find(|s| s.event.summary == summary)
        };
        calendar::save(&account, &creds, &mut store, &ical::write("", &[event]), false).await.unwrap();
        calendar::sync(&account, &creds, &mut store).await.unwrap();
        let stored = find(&store, &title).expect("the new event comes back from Google");
        assert_eq!((stored.event.start, stored.event.end, stored.event.location.as_str()), (start, start + 3600, "Nowhere"));
        assert!(!stored.recurring && !stored.href.is_empty(), "{stored:?}");

        let moved_title = format!("{title} (moved)");
        let moved = Event { summary: moved_title.clone(), start: start + 3600, end: start + 7200, ..stored.event.clone() };
        let notice = calendar::update(&account, &creds, &mut store, &stored, &moved).await.unwrap();
        assert!(notice.is_none(), "nobody was invited");
        calendar::sync(&account, &creds, &mut store).await.unwrap();
        assert!(find(&store, &title).is_none(), "old title still there");
        let stored = find(&store, &moved_title).expect("the change comes back from Google");
        assert_eq!((stored.event.start, stored.event.end), (start + 3600, start + 7200));

        calendar::delete(&account, &creds, &mut store, &stored).await.unwrap();
        calendar::sync(&account, &creds, &mut store).await.unwrap();
        assert!(find(&store, &moved_title).is_none(), "deleted event still there");
    });
}
