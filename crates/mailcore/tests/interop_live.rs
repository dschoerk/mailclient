//! Invitations between mail vendors: Gmail, GMX and Outlook.com test accounts, through their
//! real servers. Checks that what we send is understood by the other side's mail and calendar,
//! and that we understand what Google Calendar and Outlook send. Needs the accounts (see
//! google_live.rs, gmx_live.rs, ms_live.rs) and a build with the Google and Microsoft clients:
//!   LIVE_GOOGLE_EMAIL=… LIVE_GOOGLE_REFRESH_TOKEN=… LIVE_GMX_EMAIL=… LIVE_GMX_PASSWORD=… \
//!   LIVE_MS_EMAIL=… LIVE_MS_REFRESH_TOKEN=… \
//!     cargo test -p mailcore --test interop_live -- --ignored --nocapture --test-threads=1
//! Delivery takes seconds to minutes; each test waits up to a few minutes per mail.

use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use mailcore::auth::{self, Credentials};
use mailcore::calendar;
use mailcore::ical::{self, Attendee, Event};
use mailcore::imap::Imap;
use mailcore::mime::{self, Body, Draft};
use mailcore::store::{Role, Store, StoredEvent};
use mailcore::{AccountConfig, Auth};

const MAIL_WAIT: Duration = Duration::from_secs(240);

struct Party {
    account: AccountConfig,
    /// Password, or (Google) refresh token.
    secret: String,
}

impl Party {
    fn oauth(auth: Auth, email_var: &str, token_var: &str) -> Party {
        // Keeps rotated refresh tokens (Microsoft) out of the real keyring.
        // SAFETY: the tests run one at a time (--test-threads=1), before any other thread.
        unsafe { std::env::set_var("MAILCLIENT_PASSWORD", "unused") };
        let mut account = AccountConfig::guess(&std::env::var(email_var).unwrap_or_else(|_| panic!("set {email_var}")));
        account.auth = auth;
        assert!(auth::available(auth), "built without the {auth:?} client ID");
        let secret = std::env::var(token_var).unwrap_or_else(|_| panic!("set {token_var} (printed by the provider's live test)"));
        Party { account, secret }
    }

    fn google() -> Party {
        Party::oauth(Auth::Google, "LIVE_GOOGLE_EMAIL", "LIVE_GOOGLE_REFRESH_TOKEN")
    }

    fn microsoft() -> Party {
        Party::oauth(Auth::Microsoft, "LIVE_MS_EMAIL", "LIVE_MS_REFRESH_TOKEN")
    }

    fn creds(&self) -> Credentials {
        Credentials::with_secret(&self.account, self.secret.clone())
    }

    fn gmx() -> Party {
        let account = AccountConfig::guess(&std::env::var("LIVE_GMX_EMAIL").expect("set LIVE_GMX_EMAIL"));
        Party { account, secret: std::env::var("LIVE_GMX_PASSWORD").expect("set LIVE_GMX_PASSWORD") }
    }

    fn email(&self) -> &str {
        &self.account.email
    }

    async fn login(&self) -> String {
        self.creds().secret().await.unwrap()
    }

    /// Sends like the engine does: our MIME layout, from this account.
    async fn send(&self, draft: Draft) {
        let draft = Draft {
            from_name: self.account.name.clone(),
            from_addr: self.account.email.clone(),
            message_id: mime::new_message_id(&self.account.email),
            ..draft
        };
        let built = mime::build_message(&draft).unwrap();
        mailcore::smtp::send(&self.account, &self.login().await, &built.raw, &built.recipients).await.unwrap();
    }

    /// The newest message whose subject contains `subject`, from the inbox or the spam folder.
    async fn wait_for(&self, subject: &str) -> Body {
        let deadline = Instant::now() + MAIL_WAIT;
        loop {
            let mut imap = Imap::connect(&self.account, &self.login().await).await.unwrap();
            let folders = imap.list_folders().await.unwrap();
            for folder in folders.iter().filter(|f| matches!(f.2, Role::Inbox | Role::Junk)).map(|f| f.0.clone()) {
                let exists = imap.select(&folder).await.unwrap().exists;
                let latest = imap.fetch_latest(&folder, exists, 40).await.unwrap();
                if let Some(m) = latest.iter().filter(|m| m.subject.contains(subject)).max_by_key(|m| m.uid) {
                    if folders.iter().any(|f| f.0 == folder && f.2 == Role::Junk) {
                        eprintln!("note: {subject:?} landed in {folder} at {}", self.email());
                    }
                    let raw = imap.fetch_bodies(&folder, &[m.uid]).await.unwrap().remove(0).1;
                    imap.logout().await;
                    return mime::body_from_raw(&raw);
                }
            }
            imap.logout().await;
            assert!(Instant::now() < deadline, "{subject:?} did not reach {} within {MAIL_WAIT:?}", self.email());
            tokio::time::sleep(Duration::from_secs(10)).await;
        }
    }

    /// Answers an invitation the way the reader's Accept / Maybe / Decline does.
    async fn answer(&self, invite: &Event, partstat: &str, verb: &str) {
        let organizer = invite.organizer.clone().expect("invitation names its organizer");
        self.send(Draft {
            to: organizer.email.clone(),
            subject: format!("{verb}: {}", invite.summary),
            body: format!("{} has {} the invitation.", self.email(), verb.to_lowercase()),
            calendar: Some(("REPLY".into(), ical::reply(invite, self.email(), partstat))),
            ..Default::default()
        })
        .await;
    }

    /// The account's calendar events around `start`, freshly synced.
    async fn calendar_events(&self, start: i64) -> Vec<StoredEvent> {
        let mut store = Store::open_in_memory().unwrap();
        calendar::sync(&self.account, &self.creds(), &mut store).await.unwrap();
        store.events(start - 86400, start + 2 * 86400).unwrap()
    }

    /// Waits until the calendar holds an event `check` accepts; returns it.
    async fn wait_for_event(&self, start: i64, what: &str, check: impl Fn(&Event) -> bool) -> StoredEvent {
        let deadline = Instant::now() + MAIL_WAIT;
        loop {
            if let Some(s) = self.calendar_events(start).await.into_iter().find(|s| check(&s.event)) {
                return s;
            }
            assert!(Instant::now() < deadline, "{what}: not seen in {}'s calendar within {MAIL_WAIT:?}", self.email());
            tokio::time::sleep(Duration::from_secs(10)).await;
        }
    }

    /// Deletes the events of this title (cleanup; a run's titles are unique). Failing to clean up
    /// does not fail a test whose checks passed.
    async fn delete_events(&self, summary: &str, start: i64) {
        for s in self.calendar_events(start).await.into_iter().filter(|s| s.event.summary == summary) {
            if let Err(e) = calendar::delete(&self.account, &self.creds(), &mut Store::open_in_memory().unwrap(), &s).await {
                eprintln!("warning: {summary:?} left in {}'s calendar: {e:#}", self.email());
            }
        }
    }

    async fn save_event(&self, ics: &str) {
        calendar::save(&self.account, &self.creds(), &mut Store::open_in_memory().unwrap(), ics, false).await.unwrap();
    }
}

fn runtime() -> tokio::runtime::Runtime {
    mailcore::init_tls();
    tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap()
}

/// A meeting a few days ahead on the hour, unique per run.
fn meeting(name: &str, organizer: &Party, attendee: &Party) -> Event {
    let stamp = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
    let start = (stamp as i64 / 3600 + 72) * 3600;
    Event {
        uid: format!("interop-{name}-{stamp}@mailclient.test"),
        summary: format!("Interop {name} {stamp}"),
        location: "Room 1, \"Main\"; floor 2".into(),
        description: "Line one\nLine two, with a comma".into(),
        start,
        end: start + 5400,
        organizer: Some(Attendee { email: organizer.email().into(), name: "Toni Tester".into(), partstat: String::new() }),
        attendees: vec![Attendee { email: attendee.email().into(), name: "Doe, Emilie".into(), partstat: "NEEDS-ACTION".into() }],
        status: "CONFIRMED".into(),
        ..Default::default()
    }
}

fn invitation(event: &Event, to: &Party) -> Draft {
    Draft {
        to: to.email().into(),
        subject: format!("Invitation: {}", event.summary),
        body: format!("{}\nWhen: in three days\n", event.summary),
        calendar: Some(("REQUEST".into(), ical::write("REQUEST", std::slice::from_ref(event)))),
        ..Default::default()
    }
}

/// The one event of a received calendar part, checked against what was sent.
fn received(body: &Body, method: &str, sent: &Event) -> Event {
    let cal = ical::parse(body.calendar.as_deref().expect("the mail has no calendar part"));
    assert_eq!(cal.method, method);
    let e = cal.events.into_iter().find(|e| e.uid == sent.uid).expect("the calendar part lost the UID");
    assert_eq!((e.start, e.end), (sent.start, sent.end), "times changed in transit");
    e
}

/// Gmail → GMX, our client on both ends: the invitation and the answer arrive intact. Then:
/// does Google Calendar record the answer on the organizer's copy?
#[test]
#[ignore = "needs the Gmail and GMX test accounts"]
fn our_invitation_gmail_to_gmx_and_back() {
    let (toni, emilie) = (Party::google(), Party::gmx());
    runtime().block_on(async {
        let event = meeting("g2x", &toni, &emilie);
        // As the app does: into the organizer's calendar (the client mails), then the invitation.
        toni.save_event(&ical::write("", std::slice::from_ref(&event))).await;
        toni.send(invitation(&event, &emilie)).await;

        let invite = received(&emilie.wait_for(&event.summary).await, "REQUEST", &event);
        assert_eq!((invite.summary.as_str(), invite.location.as_str(), invite.description.as_str()), (event.summary.as_str(), event.location.as_str(), event.description.as_str()));
        assert_eq!(invite.organizer.as_ref().map(|o| o.email.as_str()), Some(toni.email()));
        emilie.answer(&invite, "ACCEPTED", "Accepted").await;

        let reply = received(&toni.wait_for(&format!("Accepted: {}", event.summary)).await, "REPLY", &event);
        let who: Vec<(&str, &str)> = reply.attendees.iter().map(|a| (a.email.as_str(), a.partstat.as_str())).collect();
        assert_eq!(who, [(emilie.email(), "ACCEPTED")]);

        // Google applies replies to the organizer's event when it gets them by mail.
        let stored = toni.wait_for_event(event.start, "Emilie's acceptance", |e| e.uid == event.uid && answered(e, emilie.email(), "ACCEPTED")).await;
        eprintln!("Google recorded: {:?}", stored.event.attendees);
        toni.delete_events(&event.summary, event.start).await;
    });
}

/// GMX → Gmail, our client on both ends: Gmail files our invitation in Google Calendar, and the
/// answer reaches the GMX organizer.
#[test]
#[ignore = "needs the Gmail and GMX test accounts"]
fn our_invitation_gmx_to_gmail_and_back() {
    let (toni, emilie) = (Party::google(), Party::gmx());
    runtime().block_on(async {
        let mut event = meeting("x2g", &toni, &emilie);
        event.organizer = Some(Attendee { email: emilie.email().into(), name: "Doe, Emilie".into(), partstat: String::new() });
        event.attendees = vec![Attendee { email: toni.email().into(), name: "Toni Tester".into(), partstat: "NEEDS-ACTION".into() }];
        emilie.send(invitation(&event, &toni)).await;

        let invite = received(&toni.wait_for(&event.summary).await, "REQUEST", &event);
        // Gmail adds invitations to the calendar by itself.
        let filed = toni.wait_for_event(event.start, "the invitation in Google Calendar", |e| e.uid == event.uid).await;
        assert_eq!((filed.event.start, filed.event.end, filed.event.summary.as_str()), (event.start, event.end, event.summary.as_str()));
        assert_eq!(filed.event.organizer.as_ref().map(|o| o.email.to_lowercase()), Some(emilie.email().to_lowercase()));

        toni.answer(&invite, "TENTATIVE", "Tentative").await;
        let reply = received(&emilie.wait_for(&format!("Tentative: {}", event.summary)).await, "REPLY", &event);
        let who: Vec<(&str, &str)> = reply.attendees.iter().map(|a| (a.email.as_str(), a.partstat.as_str())).collect();
        assert_eq!(who, [(toni.email(), "TENTATIVE")]);
        toni.delete_events(&event.summary, event.start).await;
    });
}

/// Google Calendar's own invitation (its format, its mail) read by us, and our answer recorded by
/// Google.
#[test]
#[ignore = "needs the Gmail and GMX test accounts"]
fn google_calendar_invitation_to_gmx() {
    let (toni, emilie) = (Party::google(), Party::gmx());
    runtime().block_on(async {
        let event = meeting("gcal", &toni, &emilie);
        // Without SCHEDULE-AGENT=CLIENT, Google does the scheduling: it mails the invitation itself.
        toni.save_event(&ical::write("REQUEST", std::slice::from_ref(&event))).await;

        let body = emilie.wait_for(&event.summary).await;
        let cal = ical::parse(body.calendar.as_deref().expect("Google's invitation has no calendar part"));
        eprintln!("Google's invitation: method {:?}, {} event(s)", cal.method, cal.events.len());
        assert_eq!(cal.method, "REQUEST");
        let invite = cal.events.into_iter().find(|e| e.uid == event.uid).expect("Google kept our UID");
        assert_eq!((invite.start, invite.end, invite.summary.as_str()), (event.start, event.end, event.summary.as_str()));
        assert_eq!(invite.organizer.as_ref().map(|o| o.email.to_lowercase()), Some(toni.email().to_lowercase()));
        assert!(invite.attendees.iter().any(|a| a.email.eq_ignore_ascii_case(emilie.email())), "{:?}", invite.attendees);

        emilie.answer(&invite, "DECLINED", "Declined").await;
        toni.wait_for_event(event.start, "Emilie's answer", |e| e.uid == event.uid && answered(e, emilie.email(), "DECLINED")).await;
        toni.delete_events(&event.summary, event.start).await;
    });
}

fn answered(e: &Event, who: &str, partstat: &str) -> bool {
    e.attendees.iter().any(|a| a.email.eq_ignore_ascii_case(who) && a.partstat == partstat)
}

/// Gmail → Outlook.com, our client on both ends: Exchange files our invitation in the calendar,
/// the answer goes back through Outlook's SMTP, and Google records it.
#[test]
#[ignore = "needs the Gmail and Outlook.com test accounts"]
fn our_invitation_gmail_to_outlook_and_back() {
    let (toni, vinny) = (Party::google(), Party::microsoft());
    runtime().block_on(async {
        let mut event = meeting("g2o", &toni, &vinny);
        event.attendees[0].name = "Vinny Validator".into();
        toni.save_event(&ical::write("", std::slice::from_ref(&event))).await;
        toni.send(invitation(&event, &vinny)).await;

        let invite = received(&vinny.wait_for(&event.summary).await, "REQUEST", &event);
        // Exchange adds invitations to the calendar by itself (keeping our UID as iCalUId).
        let filed = vinny.wait_for_event(event.start, "the invitation in Outlook", |e| e.summary == event.summary).await;
        eprintln!("Outlook filed: uid {:?}, organizer {:?}", filed.event.uid, filed.event.organizer);
        assert_eq!((filed.event.start, filed.event.end), (event.start, event.end));

        vinny.answer(&invite, "ACCEPTED", "Accepted").await;
        received(&toni.wait_for(&format!("Accepted: {}", event.summary)).await, "REPLY", &event);
        toni.wait_for_event(event.start, "Vinny's acceptance", |e| e.uid == event.uid && answered(e, vinny.email(), "ACCEPTED")).await;
        toni.delete_events(&event.summary, event.start).await;
        vinny.delete_events(&event.summary, event.start).await;
    });
}

/// Outlook's own invitation (Exchange's format with Windows time zone names, its mail) read by
/// us, and our answer recorded by Exchange.
#[test]
#[ignore = "needs the Gmail and Outlook.com test accounts"]
fn outlook_invitation_to_gmail() {
    let (toni, vinny) = (Party::google(), Party::microsoft());
    runtime().block_on(async {
        let mut event = meeting("ocal", &vinny, &toni);
        event.organizer = None;
        event.attendees = vec![Attendee { email: toni.email().into(), name: "Toni Tester".into(), partstat: "NEEDS-ACTION".into() }];
        // Created through Graph with an attendee: Exchange mails the invitation.
        vinny.save_event(&ical::write("", std::slice::from_ref(&event))).await;

        let body = toni.wait_for(&event.summary).await;
        let cal = ical::parse(body.calendar.as_deref().expect("Outlook's invitation has no calendar part"));
        eprintln!("Outlook's invitation: method {:?}, {} event(s)", cal.method, cal.events.len());
        assert_eq!(cal.method, "REQUEST");
        let invite = cal.events.into_iter().find(|e| e.summary == event.summary).expect("the event is in the invitation");
        assert_eq!((invite.start, invite.end), (event.start, event.end), "times differ after Outlook's time zones");
        assert_eq!(invite.organizer.as_ref().map(|o| o.email.to_lowercase()), Some(vinny.email().to_lowercase()));

        toni.answer(&invite, "ACCEPTED", "Accepted").await;
        vinny.wait_for_event(event.start, "Toni's answer", |e| e.summary == event.summary && answered(e, toni.email(), "ACCEPTED")).await;
        vinny.delete_events(&event.summary, event.start).await;
        toni.delete_events(&event.summary, event.start).await;
    });
}
