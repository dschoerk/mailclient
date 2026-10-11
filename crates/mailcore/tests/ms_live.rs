//! Live tests against a real Outlook.com (or Microsoft 365) test account. Need a build with the
//! Microsoft client ID (see docs/oauth-microsoft.md):
//!   LIVE_MS_EMAIL=you@outlook.com cargo test -p mailcore --test ms_live -- --ignored --nocapture --test-threads=1
//! Without LIVE_MS_REFRESH_TOKEN they open the browser sign-in once and print the refresh token;
//! export it to skip the browser on later runs.

use std::sync::OnceLock;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use mailcore::auth::{self, Credentials};
use mailcore::calendar;
use mailcore::ical::{self, Event};
use mailcore::imap::Imap;
use mailcore::store::{Role, Store};
use mailcore::{AccountConfig, Auth};

/// The test account and a refresh token for it; signs in through the browser once per run.
fn sign_in() -> (AccountConfig, String) {
    static REFRESH: OnceLock<String> = OnceLock::new();
    mailcore::init_tls();
    // Keeps rotated refresh tokens out of the real keyring.
    // SAFETY: the tests run one at a time (--test-threads=1), before any other thread.
    unsafe { std::env::set_var("MAILCLIENT_PASSWORD", "unused") };
    let email = std::env::var("LIVE_MS_EMAIL").expect("set LIVE_MS_EMAIL");
    let mut account = AccountConfig::guess(&email);
    // Microsoft 365 custom domains are not guessed.
    account.auth = Auth::Microsoft;
    assert!(auth::available(Auth::Microsoft), "built without MAILCLIENT_MS_CLIENT_ID");
    let refresh = REFRESH.get_or_init(|| {
        if let Ok(token) = std::env::var("LIVE_MS_REFRESH_TOKEN") {
            return token;
        }
        runtime().block_on(async {
            let sign_in = auth::SignIn::start(Auth::Microsoft, &email).await.unwrap();
            eprintln!("Sign in: {}", sign_in.url);
            let _ = std::process::Command::new("xdg-open").arg(&sign_in.url).spawn();
            let tokens = tokio::time::timeout(Duration::from_secs(300), sign_in.finish()).await.expect("sign-in timed out").unwrap();
            let refresh = tokens.refresh.expect("Microsoft returned no refresh token");
            eprintln!("LIVE_MS_REFRESH_TOKEN={refresh}");
            refresh
        })
    });
    (account, refresh.clone())
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap()
}

fn stamp() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs()
}

#[test]
#[ignore = "needs an Outlook.com test account and MAILCLIENT_MS_CLIENT_ID at build time"]
fn ms_sign_in_imap_smtp() {
    let (account, refresh) = sign_in();
    let email = account.email.clone();
    runtime().block_on(async {
        // The path the engine takes on every connection.
        let creds = Credentials::with_secret(&account, refresh);
        let access = creds.secret().await.unwrap();
        // Sending first: it tells apart a blocked mailbox from an IMAP-only problem.
        let raw = format!("From: {email}\r\nTo: {email}\r\nSubject: mailclient live test {}\r\n\r\nMicrosoft XOAUTH2 SMTP works.\r\n", stamp());
        mailcore::smtp::send(&account, &access, raw.as_bytes(), &[email.clone()]).await.unwrap();

        let mut imap = Imap::connect(&account, &access).await.unwrap();
        let folders = imap.list_folders().await.unwrap();
        for role in [Role::Inbox, Role::Sent, Role::Trash] {
            assert!(folders.iter().any(|f| f.2 == role), "no {role:?} folder in {folders:?}");
        }
        imap.logout().await;
    });
}

/// The Outlook calendar over Microsoft Graph: an event created, synced back, moved, renamed
/// and deleted again.
#[test]
#[ignore = "needs an Outlook.com test account and MAILCLIENT_MS_CLIENT_ID at build time"]
fn ms_calendar_create_edit_delete() {
    let (account, refresh) = sign_in();
    runtime().block_on(async {
        let creds = Credentials::with_secret(&account, refresh);
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
        // Graph gives the event its own id: found again by title.
        let find = |store: &Store, summary: &str| {
            store.events(start - 86400, start + 3 * 86400).unwrap().into_iter().find(|s| s.event.summary == summary)
        };
        calendar::save(&account, &creds, &mut store, &ical::write("", &[event]), false).await.unwrap();
        calendar::sync(&account, &creds, &mut store).await.unwrap();
        let stored = find(&store, &title).expect("the new event comes back from Outlook");
        assert_eq!((stored.event.start, stored.event.end, stored.event.location.as_str()), (start, start + 3600, "Nowhere"));
        assert!(!stored.recurring && !stored.href.is_empty(), "{stored:?}");

        let moved_title = format!("{title} (moved)");
        let moved = Event { summary: moved_title.clone(), start: start + 3600, end: start + 7200, ..stored.event.clone() };
        assert!(calendar::update(&account, &creds, &mut store, &stored, &moved).await.unwrap().is_empty(), "Exchange mails its own updates");
        calendar::sync(&account, &creds, &mut store).await.unwrap();
        assert!(find(&store, &title).is_none(), "old title still there");
        let stored = find(&store, &moved_title).expect("the change comes back from Outlook");
        assert_eq!((stored.event.start, stored.event.end), (start + 3600, start + 7200));

        calendar::delete(&account, &creds, &mut store, &stored).await.unwrap();
        calendar::sync(&account, &creds, &mut store).await.unwrap();
        assert!(find(&store, &moved_title).is_none(), "deleted event still there");
    });
}

/// Editing a meeting's attendees as the edit form does: one added (Exchange keeps both and
/// invites the new one), then the first removed (Exchange keeps the second only).
#[test]
#[ignore = "needs an Outlook.com test account and MAILCLIENT_MS_CLIENT_ID at build time"]
fn ms_edit_attendees() {
    let (account, refresh) = sign_in();
    runtime().block_on(async {
        let creds = Credentials::with_secret(&account, refresh);
        let mut store = Store::open_in_memory().unwrap();
        let stamp = stamp();
        let start = (stamp as i64 / 3600 + 50) * 3600;
        let title = format!("mailclient attendees {stamp}");
        let first = ical::Attendee { email: "first@example.org".into(), name: "First".into(), partstat: "NEEDS-ACTION".into() };
        let event = Event { uid: format!("att-{stamp}@example.org"), summary: title.clone(), start, end: start + 3600, attendees: vec![first.clone()], ..Default::default() };
        calendar::save(&account, &creds, &mut store, &ical::write("", &[event]), false).await.unwrap();
        calendar::sync(&account, &creds, &mut store).await.unwrap();
        let find = |store: &Store| store.events(start - 86400, start + 2 * 86400).unwrap().into_iter().find(|s| s.event.summary == title);
        let stored = find(&store).expect("meeting synced back");
        eprintln!("before: {:?}", stored.event.attendees);

        let second = ical::Attendee { email: "second@example.org".into(), name: "Second".into(), partstat: String::new() };
        let mut new = stored.event.clone();
        new.attendees.push(second);
        calendar::update(&account, &creds, &mut store, &stored, &new).await.unwrap();
        calendar::sync(&account, &creds, &mut store).await.unwrap();
        let after = find(&store).expect("meeting still there");
        eprintln!("after: {:?}", after.event.attendees);
        let emails: Vec<String> = after.event.attendees.iter().map(|a| a.email.clone()).collect();
        assert_eq!(emails, ["first@example.org", "second@example.org"]);

        let mut fewer = after.event.clone();
        fewer.attendees.retain(|a| a.email != "first@example.org");
        calendar::update(&account, &creds, &mut store, &after, &fewer).await.unwrap();
        calendar::sync(&account, &creds, &mut store).await.unwrap();
        let last = find(&store).expect("meeting still there");
        let emails: Vec<String> = last.event.attendees.iter().map(|a| a.email.clone()).collect();
        calendar::delete(&account, &creds, &mut store, &last).await.unwrap();
        assert_eq!(emails, ["second@example.org"]);
    });
}
