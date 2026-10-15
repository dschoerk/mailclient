//! Calendar sync against a local Radicale server: `scripts/test-caldav.sh`, then
//! `cargo test -p mailcore --test caldav -- --ignored`.

use mailcore::auth::Credentials;
use mailcore::calendar;
use mailcore::http;
use mailcore::ical::{self, Attendee, Event};
use mailcore::store::Store;

const SERVER: &str = "http://localhost:5232";

/// An Outlook-style weekly meeting at 10:00 Vienna time, across the October DST change.
const WEEKLY: &str = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:test\r\nBEGIN:VTIMEZONE\r\nTZID:W. Europe Standard Time\r\n\
BEGIN:STANDARD\r\nDTSTART:16010101T030000\r\nTZOFFSETFROM:+0200\r\nTZOFFSETTO:+0100\r\nRRULE:FREQ=YEARLY;BYDAY=-1SU;BYMONTH=10\r\nEND:STANDARD\r\n\
BEGIN:DAYLIGHT\r\nDTSTART:16010101T020000\r\nTZOFFSETFROM:+0100\r\nTZOFFSETTO:+0200\r\nRRULE:FREQ=YEARLY;BYDAY=-1SU;BYMONTH=3\r\nEND:DAYLIGHT\r\n\
END:VTIMEZONE\r\nBEGIN:VEVENT\r\nUID:weekly-1\r\nDTSTAMP:20261001T000000Z\r\nSUMMARY:Team meeting\r\n\
DTSTART;TZID=W. Europe Standard Time:20261020T100000\r\nDTEND;TZID=W. Europe Standard Time:20261020T110000\r\n\
RRULE:FREQ=WEEKLY;COUNT=3\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";

fn utc(s: &str) -> i64 {
    chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M").unwrap().and_utc().timestamp()
}

#[test]
#[ignore = "needs scripts/test-caldav.sh"]
fn discovers_saves_and_syncs() {
    // Radicale accepts any password; the override keeps the keyring out of it.
    unsafe { std::env::set_var("MAILCLIENT_PASSWORD", "x") };
    let user = format!("u{}", std::process::id());
    let account = mailcore::AccountConfig {
        username: user.clone(),
        caldav_url: format!("{SERVER}/"),
        ..mailcore::AccountConfig::guess(&format!("{user}@localhost"))
    };
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    rt.block_on(async {
        use base64::Engine as _;
        let basic = format!("Basic {}", base64::engine::general_purpose::STANDARD.encode(format!("{user}:x")));
        let auth = [("Authorization", basic.as_str())];
        let r = http::request("MKCALENDAR", &format!("{SERVER}/{user}/work/"), &auth, b"").await.unwrap();
        assert_eq!(r.status, 201, "{}", r.text());
        let r = http::request("PUT", &format!("{SERVER}/{user}/work/weekly.ics"), &[auth[0], ("Content-Type", "text/calendar")], WEEKLY.as_bytes()).await.unwrap();
        assert!(r.ok(), "{} {}", r.status, r.text());

        let creds = Credentials::load(&account).unwrap();
        let mut store = Store::open_in_memory().unwrap();
        // Something the user created, with an attendee: lands on the server too.
        let lunch = Event {
            uid: "lunch-1@example.org".into(),
            summary: "Lunch, then walk".into(),
            start: utc("2026-10-22 11:00"),
            end: utc("2026-10-22 12:00"),
            organizer: Some(Attendee { email: account.email.clone(), ..Default::default() }),
            attendees: vec![Attendee { email: "max@example.org".into(), partstat: "NEEDS-ACTION".into(), ..Default::default() }],
            ..Default::default()
        };
        calendar::save(&account, &creds, &mut store, &ical::write("REQUEST", &[lunch]), false).await.unwrap();
        calendar::sync(&account, &creds, &mut store).await.unwrap();

        let calendars = store.calendars().unwrap();
        assert_eq!(calendars.len(), 1, "{calendars:?}");
        assert_eq!(calendars[0].id, format!("{SERVER}/{user}/work/"));
        // Unchanged since: the next sync keeps the events without reading them again.
        assert!(!calendars[0].tag.is_empty(), "Radicale sends a sync-token");
        calendar::sync(&account, &creds, &mut store).await.unwrap();
        assert_eq!(store.calendars().unwrap(), calendars);
        let events: Vec<(String, i64)> = store.events(utc("2026-10-01 00:00"), utc("2026-12-01 00:00")).unwrap().into_iter().map(|s| (s.event.summary, s.event.start)).collect();
        assert_eq!(
            events,
            [
                ("Team meeting".to_string(), utc("2026-10-20 08:00")),
                ("Lunch, then walk".to_string(), utc("2026-10-22 11:00")),
                // 10:00 local is 09:00 UTC after the switch to winter time.
                ("Team meeting".to_string(), utc("2026-10-27 09:00")),
                ("Team meeting".to_string(), utc("2026-11-03 09:00")),
            ]
        );
        // Saving an event that is already there (an invitation answered twice) is fine.
        let again = ical::write("REQUEST", &[Event { uid: "lunch-1@example.org".into(), start: 0, end: 0, ..Default::default() }]);
        calendar::save(&account, &creds, &mut store, &again, true).await.unwrap();

        let find = |store: &Store, summary: &str| {
            store.events(utc("2026-10-01 00:00"), utc("2026-12-01 00:00")).unwrap().into_iter().find(|s| s.event.summary == summary)
        };
        // The attendee's answer arrives by mail: it is recorded on the server's copy.
        let invited = find(&store, "Lunch, then walk").unwrap();
        let reply = ical::with_method(&ical::reply(&invited.event, "max@example.org", "ACCEPTED"), "REPLY");
        assert!(calendar::apply_reply(&account, &creds, &mut store, &reply).await.unwrap());
        calendar::sync(&account, &creds, &mut store).await.unwrap();
        let answered = find(&store, "Lunch, then walk").unwrap();
        assert_eq!(answered.event.attendees[0].partstat, "ACCEPTED", "{:?}", answered.event.attendees);

        // Moving the organized lunch an hour and renaming it: the attendee gets an updated invitation.
        let target = find(&store, "Lunch, then walk").unwrap();
        assert!(!target.recurring && target.href.ends_with("/lunch-1_example.org.ics"), "{target:?}");
        let mut moved = target.event.clone();
        moved.summary = "Late lunch".into();
        (moved.start, moved.end) = (utc("2026-10-22 12:00"), utc("2026-10-22 13:00"));
        moved.attendees = vec![Attendee { email: "max@example.org".into(), partstat: "NEEDS-ACTION".into(), ..Default::default() }];
        moved.organizer = Some(Attendee { email: account.email.clone(), ..Default::default() });
        let notices = calendar::update(&account, &creds, &mut store, &target, &moved).await.unwrap();
        let [notice] = notices.as_slice() else { panic!("one notice expected: {notices:?}") };
        assert_eq!(notice.method, "REQUEST");
        assert_eq!(ical::parse(&notice.ics).events[0].sequence, 1);
        // Renaming the series renames every instance and keeps its times.
        let series = find(&store, "Team meeting").unwrap();
        assert!(series.recurring);
        let renamed = Event { summary: "Team sync".into(), ..series.event.clone() };
        assert!(calendar::update(&account, &creds, &mut store, &series, &renamed).await.unwrap().is_empty(), "nobody invited");
        calendar::sync(&account, &creds, &mut store).await.unwrap();
        let events: Vec<(String, i64)> = store.events(utc("2026-10-01 00:00"), utc("2026-12-01 00:00")).unwrap().into_iter().map(|s| (s.event.summary, s.event.start)).collect();
        assert_eq!(
            events,
            [
                ("Team sync".to_string(), utc("2026-10-20 08:00")),
                ("Late lunch".to_string(), utc("2026-10-22 12:00")),
                ("Team sync".to_string(), utc("2026-10-27 09:00")),
                ("Team sync".to_string(), utc("2026-11-03 09:00")),
            ]
        );
        // Editing the attendees: Ann is added and gets the invitation, Max is taken off and gets a
        // cancellation; the server keeps Ann only.
        let lunch = find(&store, "Late lunch").unwrap();
        let ann = Attendee { email: "ann@example.org".into(), name: "Ann".into(), partstat: "NEEDS-ACTION".into() };
        let reinvited = Event { attendees: vec![ann.clone()], organizer: Some(Attendee { email: account.email.clone(), ..Default::default() }), ..lunch.event.clone() };
        let notices = calendar::update(&account, &creds, &mut store, &lunch, &reinvited).await.unwrap();
        let sent: Vec<(&str, Vec<&str>)> = notices.iter().map(|n| (n.method, n.event.attendees.iter().map(|a| a.email.as_str()).collect())).collect();
        assert_eq!(sent, [("REQUEST", vec!["ann@example.org"]), ("CANCEL", vec!["max@example.org"])]);
        let cancel = ical::parse(&notices[1].ics);
        assert_eq!((cancel.method.as_str(), cancel.events[0].status.as_str()), ("CANCEL", "CANCELLED"));
        calendar::sync(&account, &creds, &mut store).await.unwrap();
        let attendees: Vec<String> = find(&store, "Late lunch").unwrap().event.attendees.into_iter().map(|a| a.email).collect();
        assert_eq!(attendees, ["ann@example.org"]);

        // Deleting: the series goes, the lunch's attendee gets a cancellation.
        let lunch = find(&store, "Late lunch").unwrap();
        let notices = calendar::delete(&account, &creds, &mut store, &lunch).await.unwrap();
        let [cancel] = notices.as_slice() else { panic!("one cancellation expected: {notices:?}") };
        assert_eq!((cancel.method, cancel.event.status.as_str()), ("CANCEL", "CANCELLED"));
        assert_eq!(cancel.event.attendees[0].email, "ann@example.org");
        let series = find(&store, "Team sync").unwrap();
        calendar::delete(&account, &creds, &mut store, &series).await.unwrap();
        calendar::sync(&account, &creds, &mut store).await.unwrap();
        assert!(store.events(0, i64::MAX).unwrap().is_empty());
    });
}
