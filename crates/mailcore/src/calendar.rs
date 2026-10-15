//! Calendar sync: CalDAV (RFC 4791) for any WebDAV calendar server and for Google, and Microsoft
//! Graph for Outlook.com and Microsoft 365, which offer no CalDAV.
//!
//! Each sync fetches the instances in a window around today and replaces what was stored; a CalDAV
//! calendar whose change tag (sync-token, getctag) did not move since is kept as it is.
//! shortcut: a changed calendar is re-read whole; use sync-collection (RFC 6578) and Graph delta
//! queries if large shared calendars make syncing slow.

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine as _;
use chrono::{DateTime, NaiveDateTime, Utc};

use crate::auth::Credentials;
use crate::config::{AccountConfig, Auth};
use crate::http::{self, Response};
use crate::ical::{self, Event};
use crate::store::{CalendarInfo, LOCAL_CALENDAR, Store, StoredEvent};

/// Days before and after today that are synced.
const PAST_DAYS: i64 = 90;
const FUTURE_DAYS: i64 = 400;

const GRAPH: &str = "https://graph.microsoft.com/v1.0";

fn window() -> (i64, i64) {
    let now = Utc::now().timestamp();
    (now - PAST_DAYS * 86400, now + FUTURE_DAYS * 86400)
}

enum Source {
    CalDav { start: String, auth: String },
    Graph { auth: String },
}

/// Where the account's calendars live, or `None` when it has no calendar server.
async fn source(account: &AccountConfig, creds: &Credentials) -> Result<Option<Source>> {
    Ok(Some(match account.auth {
        Auth::Microsoft => Source::Graph { auth: format!("Bearer {}", creds.calendar_token().await?) },
        Auth::Google => Source::CalDav {
            start: format!("https://apidata.googleusercontent.com/caldav/v2/{}/user", account.email),
            auth: format!("Bearer {}", creds.calendar_token().await?),
        },
        Auth::Password if !account.caldav_url.trim().is_empty() => {
            let basic = base64::engine::general_purpose::STANDARD.encode(format!("{}:{}", account.username, creds.secret().await?));
            Source::CalDav { start: account.caldav_url.trim().to_string(), auth: format!("Basic {basic}") }
        }
        Auth::Password => return Ok(None),
    }))
}

/// Google's main calendar, used when discovery finds nothing and for new events.
fn google_primary(account: &AccountConfig) -> String {
    format!("https://apidata.googleusercontent.com/caldav/v2/{}/events/", account.email)
}

/// Fetches the account's calendars and their events into `store`. Accounts without a calendar
/// server keep only the events stored on this device.
pub async fn sync(account: &AccountConfig, creds: &Credentials, store: &mut Store) -> Result<()> {
    let Some(source) = source(account, creds).await? else { return Ok(()) };
    let (from, to) = window();
    let (calendars, events, kept) = match source {
        Source::CalDav { start, auth } => {
            let mut calendars = match (discover(&auth, &start).await, account.auth) {
                (Ok(c), Auth::Google) if c.is_empty() => vec![CalendarInfo { id: google_primary(account), ..Default::default() }],
                (Err(e), Auth::Google) => {
                    log::info!("calendar discovery: {e:#}; using the main calendar");
                    vec![CalendarInfo { id: google_primary(account), ..Default::default() }]
                }
                (result, _) => result?,
            };
            // Only calendars whose change tag moved are read again; all of them once a day, as the
            // window moves along. Servers without a tag: every time.
            let today = Utc::now().date_naive();
            let stored = store.calendars()?;
            let mut events = Vec::new();
            let mut kept = Vec::new();
            for c in &mut calendars {
                if !c.tag.is_empty() {
                    c.tag = format!("{today} {}", c.tag);
                    if stored.iter().any(|s| s.id == c.id && s.tag == c.tag) {
                        kept.push(c.id.clone());
                        continue;
                    }
                }
                events.extend(report(&auth, &c.id, from, to).await.with_context(|| format!("reading calendar {}", c.name))?);
            }
            (calendars, events, kept)
        }
        Source::Graph { auth } => {
            let (calendars, events) = graph_events(&auth, from, to).await?;
            (calendars, events, Vec::new())
        }
    };
    store.replace_synced_events(&calendars, &events, &kept)
}

/// Puts an event into the account's calendar. `ics` is a full VCALENDAR; `invitation` says it
/// came in by mail, which Google and Microsoft file in the calendar themselves.
pub async fn save(account: &AccountConfig, creds: &Credentials, store: &mut Store, ics: &str, invitation: bool) -> Result<()> {
    if invitation && account.auth != Auth::Password {
        return Ok(());
    }
    let (from, to) = window();
    match source(account, creds).await? {
        None => store.add_local_events(ics, &ical::expand(&ical::parse(ics).events, from, to)),
        Some(Source::CalDav { start, auth }) => {
            let calendar = match account.auth {
                Auth::Google => google_primary(account),
                _ => match store.calendars()?.into_iter().next() {
                    Some(c) => c.id,
                    None => discover(&auth, &start).await?.into_iter().next().ok_or_else(|| anyhow!("no calendar found on the server"))?.id,
                },
            };
            let uid = ical::parse(ics).events.first().map(|e| e.uid.clone()).unwrap_or_default();
            let name: String = uid.chars().map(|c| if c.is_ascii_alphanumeric() || "-_.".contains(c) { c } else { '_' }).collect();
            let url = format!("{}/{name}.ics", calendar.trim_end_matches('/'));
            let r = http::request(
                "PUT",
                &url,
                &[("Authorization", &auth), ("Content-Type", "text/calendar; charset=utf-8"), ("If-None-Match", "*")],
                ical::without_method(ics).as_bytes(),
            )
            .await?;
            // 412: the server has it already, e.g. delivered to its own scheduling inbox.
            if !r.ok() && r.status != 412 {
                bail!("saving the event failed: {}", failure(&r));
            }
            Ok(())
        }
        Some(Source::Graph { auth }) => {
            let e = ical::parse(ics).events.into_iter().next().ok_or_else(|| anyhow!("no event to save"))?;
            // Exchange mails the invitations to the attendees itself.
            let r = http::request("POST", &format!("{GRAPH}/me/events"), &[("Authorization", &auth), ("Content-Type", "application/json")], graph_event(&e).to_string().as_bytes()).await?;
            if !r.ok() {
                bail!("saving the event failed: {}", failure(&r));
            }
            Ok(())
        }
    }
}

/// A mail the attendees need after the organizer changed or deleted an event.
#[derive(Debug, Clone)]
pub struct Notice {
    /// REQUEST (changed) or CANCEL.
    pub method: &'static str,
    /// The VCALENDAR to mail, with its METHOD.
    pub ics: String,
    pub event: Event,
}

/// The notice for `text` (the event as stored) when the account organizes it and invited people.
/// Microsoft's servers send their own.
fn notice(account: &AccountConfig, text: &str, method: &'static str) -> Option<Notice> {
    let event = ical::parse(text).events.into_iter().next()?;
    let organizer = event.organizer.as_ref()?;
    (account.auth != Auth::Microsoft && organizer.email.eq_ignore_ascii_case(&account.email) && !event.attendees.is_empty())
        .then(|| Notice { method, ics: ical::with_method(text, method), event })
}

/// The stored text and ETag of a CalDAV resource.
async fn fetch(auth: &str, href: &str) -> Result<(String, Option<String>)> {
    let r = http::request("GET", href, &[("Authorization", auth)], b"").await?;
    if !r.ok() {
        bail!("reading the event failed: {}", failure(&r));
    }
    Ok((r.text(), r.header("ETag").map(str::to_string)))
}

/// The mails an organizer's change needs: the new version for everyone still invited, and a
/// cancellation for those taken off (RFC 5546 3.2.5), so they lose the meeting too.
fn change_notices(account: &AccountConfig, before: &str, after: &str, full: bool) -> Vec<Notice> {
    let mut notices: Vec<Notice> = notice(account, after, "REQUEST").into_iter().collect();
    let Some(old) = ical::parse(before).events.into_iter().next().filter(|_| full) else { return notices };
    let still = ical::parse(after).events.into_iter().next().map(|e| e.attendees).unwrap_or_default();
    let removed: Vec<ical::Attendee> =
        old.attendees.iter().filter(|a| !still.iter().any(|s| s.email.eq_ignore_ascii_case(&a.email))).cloned().collect();
    if !removed.is_empty() {
        let cancel = Event { attendees: removed, status: "CANCELLED".into(), ..old };
        notices.extend(notice(account, &ical::patch(before, &cancel, true), "CANCEL"));
    }
    notices
}

/// Changes an event to `new`'s values where it lives. A repeating event changes as a whole
/// series, and only its texts (`target.recurring`). Returns the mails its attendees need.
pub async fn update(account: &AccountConfig, creds: &Credentials, store: &mut Store, target: &StoredEvent, new: &Event) -> Result<Vec<Notice>> {
    let full = !target.recurring;
    if target.calendar == LOCAL_CALENDAR {
        let text = store.local_ics(&target.event.uid)?.ok_or_else(|| anyhow!("the event is no longer on this device"))?;
        let patched = ical::patch(&text, new, full);
        let (from, to) = window();
        store.add_local_events(&patched, &ical::expand(&ical::parse(&patched).events, from, to))?;
        return Ok(change_notices(account, &text, &patched, full));
    }
    match source(account, creds).await? {
        None => bail!("this account has no calendar server any more"),
        Some(Source::Graph { auth }) => {
            let mut body = graph_event(new);
            if !full {
                body = serde_json::json!({ "subject": body["subject"], "body": body["body"], "location": body["location"] });
            }
            let url = format!("{GRAPH}/me/events/{}", target.href);
            let r = http::request("PATCH", &url, &[("Authorization", &auth), ("Content-Type", "application/json")], body.to_string().as_bytes()).await?;
            if !r.ok() {
                bail!("saving the event failed: {}", failure(&r));
            }
            // Exchange mails attendees added, removed and kept.
            Ok(Vec::new())
        }
        Some(Source::CalDav { auth, .. }) => {
            let (text, etag) = fetch(&auth, &target.href).await?;
            let patched = ical::patch(&text, new, full);
            let mut headers = vec![("Authorization", auth.as_str()), ("Content-Type", "text/calendar; charset=utf-8")];
            // Refuses to overwrite a change made elsewhere since the read.
            headers.extend(etag.as_deref().map(|e| ("If-Match", e)));
            let r = http::request("PUT", &target.href, &headers, patched.as_bytes()).await?;
            match r.status {
                412 => bail!("the event was changed elsewhere; try again after the next sync"),
                _ if !r.ok() => bail!("saving the event failed: {}", failure(&r)),
                _ => Ok(change_notices(account, &text, &patched, full)),
            }
        }
    }
}

/// Deletes an event (a whole series for a repeating one). Returns the cancellation its attendees need.
pub async fn delete(account: &AccountConfig, creds: &Credentials, store: &mut Store, target: &StoredEvent) -> Result<Vec<Notice>> {
    let cancelled = |text: &str| {
        let mut e = ical::parse(text).events.into_iter().next().unwrap_or_default();
        e.status = "CANCELLED".into();
        notice(account, &ical::patch(text, &e, false), "CANCEL")
    };
    if target.calendar == LOCAL_CALENDAR {
        let text = store.local_ics(&target.event.uid)?;
        store.remove_local_events(&target.event.uid)?;
        return Ok(text.as_deref().and_then(cancelled).into_iter().collect());
    }
    match source(account, creds).await? {
        None => bail!("this account has no calendar server any more"),
        Some(Source::Graph { auth }) => {
            // Exchange mails the cancellation itself.
            let r = http::request("DELETE", &format!("{GRAPH}/me/events/{}", target.href), &[("Authorization", &auth)], b"").await?;
            if !r.ok() && r.status != 404 {
                bail!("deleting the event failed: {}", failure(&r));
            }
            Ok(Vec::new())
        }
        Some(Source::CalDav { auth, .. }) => {
            let (text, _) = fetch(&auth, &target.href).await?;
            let r = http::request("DELETE", &target.href, &[("Authorization", &auth)], b"").await?;
            if !r.ok() && r.status != 404 {
                bail!("deleting the event failed: {}", failure(&r));
            }
            Ok(cancelled(&text).into_iter().collect())
        }
    }
}

/// Records the answers in a REPLY mail (`ics`) on the organizer's copy of the event: on this
/// device, or on a CalDAV server (which leaves mailed answers to the client, see SCHEDULE-AGENT).
/// Google and Exchange record answers from mail themselves. Returns whether anything changed.
pub async fn apply_reply(account: &AccountConfig, creds: &Credentials, store: &mut Store, ics: &str) -> Result<bool> {
    let reply = ical::parse(ics);
    if reply.method != "REPLY" || account.auth != Auth::Password {
        return Ok(false);
    }
    let mut changed = false;
    for e in &reply.events {
        let Some(target) = store.event_by_uid(&e.uid)? else { continue };
        let answer = |text: &str| e.attendees.iter().fold(text.to_string(), |t, a| ical::set_partstat(&t, &e.uid, &a.email, &a.partstat));
        if target.calendar == LOCAL_CALENDAR {
            let Some(text) = store.local_ics(&e.uid)? else { continue };
            let answered = answer(&text);
            let (from, to) = window();
            store.add_local_events(&answered, &ical::expand(&ical::parse(&answered).events, from, to))?;
            changed = true;
        } else if let Some(Source::CalDav { auth, .. }) = source(account, creds).await? {
            let (text, etag) = fetch(&auth, &target.href).await?;
            let mut headers = vec![("Authorization", auth.as_str()), ("Content-Type", "text/calendar; charset=utf-8")];
            headers.extend(etag.as_deref().map(|e| ("If-Match", e)));
            let r = http::request("PUT", &target.href, &headers, answer(&text).as_bytes()).await?;
            if !r.ok() {
                bail!("recording the answer failed: {}", failure(&r));
            }
            changed = true;
        }
    }
    Ok(changed)
}

fn failure(r: &Response) -> String {
    // Google and Graph explain themselves in JSON ("CalDAV API has not been used in project …").
    let json: Option<serde_json::Value> = serde_json::from_slice(&r.body).ok();
    let text = r.text();
    // Google's CalDAV answers in GData XML instead.
    let gdata = text.split_once("<internalReason>").and_then(|(_, rest)| rest.split_once("</internalReason>")).map(|(reason, _)| reason.to_string());
    let message = json.as_ref().and_then(|j| j["error"]["message"].as_str().or(j["error"].as_str())).map(str::to_string).or(gdata);
    let detail = message.unwrap_or_else(|| text.chars().take(200).collect::<String>()).trim().to_string();
    match r.status {
        401 | 403 => format!("the calendar server refused access ({}): {detail}", r.status),
        status => format!("HTTP {status} {detail}"),
    }
}

// ---- CalDAV ----

#[derive(Debug, Default, Clone)]
struct DavProp {
    text: String,
    hrefs: Vec<String>,
    /// Names of child elements (resourcetype: collection, calendar).
    children: Vec<String>,
    /// `name` attributes of `comp` children (supported-calendar-component-set).
    comps: Vec<String>,
}

#[derive(Debug, Default)]
struct DavResponse {
    href: String,
    props: Vec<(String, DavProp)>,
}

impl DavResponse {
    fn prop(&self, name: &str) -> Option<&DavProp> {
        self.props.iter().find(|(n, _)| n == name).map(|(_, p)| p)
    }

    fn is_calendar(&self) -> bool {
        self.prop("resourcetype").is_some_and(|p| p.children.iter().any(|c| c == "calendar"))
    }
}

/// The `response` elements of a 207 Multi-Status body, with the properties found (status 200).
/// Elements are matched by local name: servers pick their own namespace prefixes.
fn multistatus(xml: &str) -> Result<Vec<DavResponse>> {
    let doc = roxmltree::Document::parse(xml).context("parsing the server's XML")?;
    let named = |n: &roxmltree::Node, name: &str| n.is_element() && n.tag_name().name() == name;
    let text = |n: roxmltree::Node| n.descendants().filter(|d| d.is_text()).filter_map(|d| d.text()).collect::<String>();
    Ok(doc
        .descendants()
        .filter(|n| named(n, "response"))
        .map(|resp| {
            let href = resp.children().find(|c| named(c, "href")).map(text).unwrap_or_default().trim().to_string();
            let props = resp
                .children()
                .filter(|c| named(c, "propstat"))
                .filter(|ps| ps.children().find(|c| named(c, "status")).is_none_or(|s| text(s).contains(" 200")))
                .flat_map(|ps| ps.children().filter(|c| named(c, "prop")).flat_map(|p| p.children().filter(|c| c.is_element())))
                .map(|p| {
                    let prop = DavProp {
                        text: text(p),
                        hrefs: p.descendants().filter(|d| named(d, "href")).map(|h| text(h).trim().to_string()).collect(),
                        children: p.children().filter(|c| c.is_element()).map(|c| c.tag_name().name().to_string()).collect(),
                        comps: p.children().filter(|c| named(c, "comp")).filter_map(|c| c.attribute("name")).map(str::to_string).collect(),
                    };
                    (p.tag_name().name().to_string(), prop)
                })
                .collect();
            DavResponse { href, props }
        })
        .collect())
}

/// A WebDAV request, following redirects (`.well-known`, moved collections). Returns the final URL.
async fn dav(auth: &str, method: &str, url: &str, depth: &str, body: &str) -> Result<(String, Response)> {
    let mut url = url.to_string();
    for _ in 0..5 {
        let headers = [("Authorization", auth), ("Depth", depth), ("Content-Type", "application/xml; charset=utf-8")];
        let r = http::request(method, &url, &headers, body.as_bytes()).await?;
        match (r.status, r.header("Location")) {
            (301 | 302 | 303 | 307 | 308, Some(to)) => url = url::Url::parse(&url)?.join(to)?.to_string(),
            _ if r.status == 207 || r.ok() => return Ok((url, r)),
            _ => bail!("{method} {url}: {}", failure(&r)),
        }
    }
    bail!("too many redirects from {url}")
}

async fn propfind(auth: &str, url: &str, depth: &str, props: &str) -> Result<(String, Vec<DavResponse>)> {
    let body = format!(
        r#"<?xml version="1.0" encoding="utf-8"?><d:propfind xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav" xmlns:a="http://apple.com/ns/ical/" xmlns:cs="http://calendarserver.org/ns/"><d:prop>{props}</d:prop></d:propfind>"#
    );
    let (url, r) = dav(auth, "PROPFIND", url, depth, &body).await?;
    Ok((url, multistatus(&r.text())?))
}

fn join(base: &str, href: &str) -> Result<String> {
    Ok(url::Url::parse(base)?.join(href)?.to_string())
}

/// The calendars (holding events) reachable from `start`: a server root, a principal, a
/// calendar home, or a single calendar.
async fn discover(auth: &str, start: &str) -> Result<Vec<CalendarInfo>> {
    let mut start = url::Url::parse(start).with_context(|| format!("invalid calendar URL {start}"))?;
    if start.path() == "/" {
        start.set_path("/.well-known/caldav");
    }
    let (url, found) = propfind(auth, start.as_str(), "0", "<d:current-user-principal/><d:resourcetype/><d:displayname/><a:calendar-color/><d:sync-token/><cs:getctag/>").await?;
    if let Some(r) = found.first().filter(|r| r.is_calendar()) {
        return Ok(vec![calendar_info(&url, r)?]);
    }
    let principal = match found.iter().find_map(|r| r.prop("current-user-principal")?.hrefs.first().cloned()) {
        Some(href) => join(&url, &href)?,
        None => url,
    };
    let (url, found) = propfind(auth, &principal, "0", "<c:calendar-home-set/>").await?;
    let home = found
        .iter()
        .find_map(|r| r.prop("calendar-home-set")?.hrefs.first().cloned())
        .ok_or_else(|| anyhow!("the server has no calendars for this account"))?;
    let home = join(&url, &home)?;
    let (url, found) =
        propfind(auth, &home, "1", "<d:resourcetype/><d:displayname/><a:calendar-color/><c:supported-calendar-component-set/><d:sync-token/><cs:getctag/>").await?;
    found
        .iter()
        .filter(|r| r.is_calendar())
        .filter(|r| r.prop("supported-calendar-component-set").is_none_or(|p| p.comps.is_empty() || p.comps.iter().any(|c| c == "VEVENT")))
        .map(|r| calendar_info(&url, r))
        .collect()
}

fn calendar_info(base: &str, r: &DavResponse) -> Result<CalendarInfo> {
    let id = if r.href.is_empty() { base.to_string() } else { join(base, &r.href)? };
    let name = r.prop("displayname").map(|p| p.text.trim().to_string()).filter(|n| !n.is_empty());
    let fallback = id.trim_end_matches('/').rsplit('/').next().unwrap_or_default().to_string();
    let color = r.prop("calendar-color").map(|p| p.text.trim().to_string()).unwrap_or_default();
    // Either changes with every change to the calendar: sync-token (RFC 6578), or CalendarServer's getctag.
    let tag = ["sync-token", "getctag"].iter().filter_map(|n| r.prop(n)).map(|p| p.text.trim()).filter(|t| !t.is_empty()).collect::<Vec<_>>().join(" ");
    Ok(CalendarInfo { id, name: name.unwrap_or(fallback), color, tag })
}

fn dav_time(ts: i64) -> String {
    DateTime::<Utc>::from_timestamp(ts, 0).unwrap_or_default().format("%Y%m%dT%H%M%SZ").to_string()
}

/// Events of one calendar in `from..to`. Asks the server to expand repeats; the ones it leaves
/// unexpanded are expanded here.
async fn report(auth: &str, calendar: &str, from: i64, to: i64) -> Result<Vec<StoredEvent>> {
    let (start, end) = (dav_time(from), dav_time(to));
    let body = format!(
        r#"<?xml version="1.0" encoding="utf-8"?><c:calendar-query xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav"><d:prop><c:calendar-data><c:expand start="{start}" end="{end}"/></c:calendar-data></d:prop><c:filter><c:comp-filter name="VCALENDAR"><c:comp-filter name="VEVENT"><c:time-range start="{start}" end="{end}"/></c:comp-filter></c:comp-filter></c:filter></c:calendar-query>"#
    );
    let (url, r) = dav(auth, "REPORT", calendar, "1", &body).await?;
    events_from_report(&r.text(), &url, calendar, from, to)
}

fn events_from_report(xml: &str, base: &str, calendar: &str, from: i64, to: i64) -> Result<Vec<StoredEvent>> {
    let mut events = Vec::new();
    for r in multistatus(xml)? {
        if let Some(data) = r.prop("calendar-data") {
            let parsed = ical::parse(&data.text).events;
            // Expanded by the server: instances carry RECURRENCE-ID; otherwise the RRULE is still there.
            let recurring = parsed.iter().any(|e| e.rrule.is_some() || e.recurrence_id.is_some());
            let href = join(base, &r.href)?;
            // Overrides only replace instances of their own resource.
            events.extend(ical::expand(&parsed, from, to).into_iter().map(|event| StoredEvent {
                calendar: calendar.to_string(),
                href: href.clone(),
                recurring,
                event,
                ..Default::default()
            }));
        }
    }
    Ok(events)
}

// ---- Microsoft Graph ----

async fn graph_get(auth: &str, url: &str) -> Result<serde_json::Value> {
    let r = http::request("GET", url, &[("Authorization", auth), ("Prefer", "outlook.timezone=\"UTC\""), ("Accept", "application/json")], b"").await?;
    if !r.ok() {
        bail!("reading the calendar failed: {}", failure(&r));
    }
    serde_json::from_slice(&r.body).context("parsing the calendar response")
}

/// Every page of a Graph collection.
async fn graph_list(auth: &str, url: &str) -> Result<Vec<serde_json::Value>> {
    let mut items = Vec::new();
    let mut next = Some(url.to_string());
    while let Some(url) = next {
        let page = graph_get(auth, &url).await?;
        items.extend(page["value"].as_array().cloned().unwrap_or_default());
        next = page["@odata.nextLink"].as_str().map(str::to_string);
    }
    Ok(items)
}

async fn graph_events(auth: &str, from: i64, to: i64) -> Result<(Vec<CalendarInfo>, Vec<StoredEvent>)> {
    let iso = |ts: i64| DateTime::<Utc>::from_timestamp(ts, 0).unwrap_or_default().format("%Y-%m-%dT%H:%M:%SZ").to_string();
    let calendars: Vec<CalendarInfo> = graph_list(auth, &format!("{GRAPH}/me/calendars?$select=id,name,hexColor"))
        .await?
        .iter()
        .map(|c| CalendarInfo {
            id: c["id"].as_str().unwrap_or_default().to_string(),
            name: c["name"].as_str().unwrap_or_default().to_string(),
            color: c["hexColor"].as_str().unwrap_or_default().to_string(),
            ..Default::default()
        })
        .collect();
    let mut events = Vec::new();
    for c in &calendars {
        let url = format!(
            "{GRAPH}/me/calendars/{}/calendarView?startDateTime={}&endDateTime={}&$top=200&$select=id,seriesMasterId,type,iCalUId,subject,start,end,isAllDay,isCancelled,location,bodyPreview,organizer,attendees",
            c.id,
            iso(from),
            iso(to)
        );
        events.extend(graph_list(auth, &url).await?.iter().filter_map(event_from_graph).map(|e| StoredEvent { calendar: c.id.clone(), ..e }));
    }
    Ok((calendars, events))
}

fn event_from_graph(v: &serde_json::Value) -> Option<StoredEvent> {
    if v["isCancelled"].as_bool() == Some(true) {
        return None;
    }
    // Instances of a series are changed through the series.
    let recurring = matches!(v["type"].as_str(), Some("occurrence" | "exception"));
    let href = if recurring { &v["seriesMasterId"] } else { &v["id"] };
    // "2026-10-20T08:00:00.0000000", in UTC as asked for with the Prefer header.
    let time = |t: &serde_json::Value| NaiveDateTime::parse_from_str(t["dateTime"].as_str()?.get(..19)?, "%Y-%m-%dT%H:%M:%S").ok().map(|d| d.and_utc().timestamp());
    let organizer = &v["organizer"]["emailAddress"];
    let event = Event {
        uid: v["iCalUId"].as_str().unwrap_or_default().to_string(),
        summary: v["subject"].as_str().unwrap_or_default().to_string(),
        location: v["location"]["displayName"].as_str().unwrap_or_default().to_string(),
        description: v["bodyPreview"].as_str().unwrap_or_default().to_string(),
        start: time(&v["start"])?,
        end: time(&v["end"])?,
        all_day: v["isAllDay"].as_bool().unwrap_or(false),
        organizer: organizer["address"].as_str().map(|a| ical::Attendee {
            email: a.to_string(),
            name: organizer["name"].as_str().unwrap_or_default().to_string(),
            partstat: String::new(),
        }),
        attendees: v["attendees"]
            .as_array()
            .map(|all| {
                all.iter()
                    .filter_map(|a| {
                        Some(ical::Attendee {
                            email: a["emailAddress"]["address"].as_str()?.to_string(),
                            name: a["emailAddress"]["name"].as_str().unwrap_or_default().to_string(),
                            partstat: match a["status"]["response"].as_str() {
                                Some("accepted" | "organizer") => "ACCEPTED",
                                Some("declined") => "DECLINED",
                                Some("tentativelyAccepted") => "TENTATIVE",
                                _ => "NEEDS-ACTION",
                            }
                            .to_string(),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default(),
        ..Default::default()
    };
    Some(StoredEvent { href: href.as_str().unwrap_or_default().to_string(), recurring, event, ..Default::default() })
}

fn graph_event(e: &Event) -> serde_json::Value {
    let time = |ts: i64| {
        serde_json::json!({
            "dateTime": DateTime::<Utc>::from_timestamp(ts, 0).unwrap_or_default().format("%Y-%m-%dT%H:%M:%S").to_string(),
            "timeZone": "UTC",
        })
    };
    serde_json::json!({
        "subject": e.summary,
        "body": { "contentType": "text", "content": e.description },
        "start": time(e.start),
        "end": time(e.end),
        "isAllDay": e.all_day,
        "location": { "displayName": e.location },
        "attendees": e.attendees.iter().map(|a| serde_json::json!({
            "emailAddress": { "address": a.email, "name": a.name },
            "type": "required",
        })).collect::<Vec<_>>(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_calendar_listing() {
        // Nextcloud-style: own prefixes, a 404 propstat, the home itself and an address book.
        let xml = r##"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:" xmlns:cal="urn:ietf:params:xml:ns:caldav" xmlns:x1="http://apple.com/ns/ical/">
 <d:response><d:href>/remote.php/dav/calendars/max/</d:href>
  <d:propstat><d:prop><d:resourcetype><d:collection/></d:resourcetype></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response>
 <d:response><d:href>/remote.php/dav/calendars/max/personal/</d:href>
  <d:propstat><d:prop><d:resourcetype><d:collection/><cal:calendar/></d:resourcetype><d:displayname>Personal</d:displayname>
   <x1:calendar-color>#0082c9</x1:calendar-color><d:sync-token>http://sabre.io/ns/sync/42</d:sync-token>
   <cs:getctag xmlns:cs="http://calendarserver.org/ns/">42</cs:getctag>
   <cal:supported-calendar-component-set><cal:comp name="VEVENT"/><cal:comp name="VTODO"/></cal:supported-calendar-component-set></d:prop>
   <d:status>HTTP/1.1 200 OK</d:status></d:propstat>
  <d:propstat><d:prop><d:getetag/></d:prop><d:status>HTTP/1.1 404 Not Found</d:status></d:propstat></d:response>
 <d:response><d:href>/remote.php/dav/calendars/max/tasks/</d:href>
  <d:propstat><d:prop><d:resourcetype><d:collection/><cal:calendar/></d:resourcetype><d:displayname>Tasks</d:displayname>
   <cal:supported-calendar-component-set><cal:comp name="VTODO"/></cal:supported-calendar-component-set></d:prop>
   <d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response>
</d:multistatus>"##;
        let found = multistatus(xml).unwrap();
        assert_eq!(found.len(), 3);
        assert!(found[0].prop("getetag").is_none());
        let calendars: Vec<CalendarInfo> = found
            .iter()
            .filter(|r| r.is_calendar())
            .filter(|r| r.prop("supported-calendar-component-set").is_none_or(|p| p.comps.iter().any(|c| c == "VEVENT")))
            .map(|r| calendar_info("https://cloud.example.org/remote.php/dav/calendars/max/", r).unwrap())
            .collect();
        assert_eq!(
            calendars,
            [CalendarInfo {
                id: "https://cloud.example.org/remote.php/dav/calendars/max/personal/".into(),
                name: "Personal".into(),
                color: "#0082c9".into(),
                tag: "http://sabre.io/ns/sync/42 42".into()
            }]
        );
    }

    #[test]
    fn reads_expanded_and_unexpanded_reports() {
        // One server expanded (instances with RECURRENCE-ID), the other left the RRULE.
        let xml = r#"<multistatus xmlns="DAV:"><response><href>/cal/a.ics</href><propstat><prop>
<calendar-data xmlns="urn:ietf:params:xml:ns:caldav"><![CDATA[BEGIN:VCALENDAR
BEGIN:VEVENT
UID:a
RECURRENCE-ID:20261005T080000Z
DTSTART:20261005T080000Z
DTEND:20261005T090000Z
SUMMARY:Standup
END:VEVENT
BEGIN:VEVENT
UID:a
RECURRENCE-ID:20261006T080000Z
DTSTART:20261006T080000Z
DTEND:20261006T090000Z
SUMMARY:Standup
END:VEVENT
END:VCALENDAR
]]></calendar-data></prop><status>HTTP/1.1 200 OK</status></propstat></response>
<response><href>/cal/b.ics</href><propstat><prop><C:calendar-data xmlns:C="urn:ietf:params:xml:ns:caldav">BEGIN:VCALENDAR&#13;
BEGIN:VEVENT&#13;
UID:b&#13;
DTSTART:20261001T120000Z&#13;
DTEND:20261001T130000Z&#13;
RRULE:FREQ=WEEKLY;COUNT=10&#13;
SUMMARY:Lunch &amp; learn&#13;
END:VEVENT&#13;
END:VCALENDAR&#13;
</C:calendar-data></prop><status>HTTP/1.1 200 OK</status></propstat></response></multistatus>"#;
        let ts = |s: &str| NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M").unwrap().and_utc().timestamp();
        let events = events_from_report(xml, "https://dav.example.org/cal/", "cal-id", ts("2026-10-01 00:00"), ts("2026-10-16 00:00")).unwrap();
        assert!(events.iter().all(|e| e.recurring && e.calendar == "cal-id"));
        assert_eq!(events[0].href, "https://dav.example.org/cal/a.ics");
        let got: Vec<(&str, i64)> = events.iter().map(|e| (e.event.summary.as_str(), e.event.start)).collect();
        assert_eq!(
            got,
            [
                ("Standup", ts("2026-10-05 08:00")),
                ("Standup", ts("2026-10-06 08:00")),
                ("Lunch & learn", ts("2026-10-01 12:00")),
                ("Lunch & learn", ts("2026-10-08 12:00")),
                ("Lunch & learn", ts("2026-10-15 12:00")),
            ]
        );
    }

    #[test]
    fn explains_refusals() {
        let google = Response {
            status: 403,
            headers: Vec::new(),
            body: b"<errors xmlns=\"http://schemas.google.com/g/2005\"><error><code>accessNotConfigured</code><internalReason>CalDAV API has not been used in project 5 before or it is disabled.</internalReason></error></errors>".to_vec(),
        };
        assert_eq!(failure(&google), "the calendar server refused access (403): CalDAV API has not been used in project 5 before or it is disabled.");
        let graph = Response { status: 401, headers: Vec::new(), body: br#"{"error":{"code":"InvalidAuthenticationToken","message":"Access token has expired."}}"#.to_vec() };
        assert_eq!(failure(&graph), "the calendar server refused access (401): Access token has expired.");
    }

    #[test]
    fn graph_events_round_trip() {
        let v = serde_json::json!({
            "iCalUId": "040000008200E001", "subject": "1:1", "isAllDay": false, "isCancelled": false,
            "start": { "dateTime": "2026-10-20T08:00:00.0000000", "timeZone": "UTC" },
            "end": { "dateTime": "2026-10-20T08:30:00.0000000", "timeZone": "UTC" },
            "location": { "displayName": "Teams" },
            "organizer": { "emailAddress": { "name": "Jane", "address": "jane@contoso.com" } },
            "attendees": [{ "type": "required", "status": { "response": "tentativelyAccepted" }, "emailAddress": { "name": "Max", "address": "max@contoso.com" } }],
        });
        let stored = event_from_graph(&v).unwrap();
        assert!(!stored.recurring);
        let e = stored.event;
        assert_eq!((e.uid.as_str(), e.end - e.start, e.location.as_str()), ("040000008200E001", 1800, "Teams"));
        assert_eq!(e.organizer.as_ref().unwrap().email, "jane@contoso.com");
        assert_eq!((e.attendees[0].email.as_str(), e.attendees[0].partstat.as_str()), ("max@contoso.com", "TENTATIVE"));
        assert_eq!(graph_event(&e)["start"]["dateTime"], "2026-10-20T08:00:00");
        assert!(event_from_graph(&serde_json::json!({ "isCancelled": true })).is_none());
        let mut occurrence = v.clone();
        occurrence["type"] = "occurrence".into();
        occurrence["id"] = "AAMk-occ".into();
        occurrence["seriesMasterId"] = "AAMk-series".into();
        let stored = event_from_graph(&occurrence).unwrap();
        assert_eq!((stored.recurring, stored.href.as_str()), (true, "AAMk-series"));
    }
}
