//! iCalendar (RFC 5545) events and iTIP methods (RFC 5546): reading what CalDAV servers and
//! invitation mails carry, expanding repeats, and writing invitations and replies.
//!
//! Times are kept as Unix seconds. All-day events start at UTC midnight of their date, so their
//! date reads the same in every zone. Zones come from the calendar's own VTIMEZONE blocks, which
//! RFC 5545 requires for every TZID and which Outlook and Google both send.

use std::sync::Arc;

use chrono::{DateTime, Datelike, Duration, Local, NaiveDate, NaiveDateTime, NaiveTime, TimeZone, Utc, Weekday};

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Attendee {
    pub email: String,
    pub name: String,
    /// NEEDS-ACTION, ACCEPTED, DECLINED, TENTATIVE.
    pub partstat: String,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Event {
    pub uid: String,
    pub summary: String,
    pub location: String,
    pub description: String,
    pub start: i64,
    pub end: i64,
    pub all_day: bool,
    pub organizer: Option<Attendee>,
    pub attendees: Vec<Attendee>,
    pub sequence: u32,
    /// CONFIRMED, TENTATIVE or CANCELLED.
    pub status: String,
    /// Set on one changed instance of a repeating event: the start it replaces.
    pub recurrence_id: Option<i64>,
    pub rrule: Option<String>,
    pub exdates: Vec<i64>,
    pub zone: Zone,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Calendar {
    /// iTIP method of a mailed calendar: REQUEST, REPLY, CANCEL; empty for stored calendars.
    pub method: String,
    pub events: Vec<Event>,
}

/// Where an event's wall-clock times live, for repeating it across DST changes.
#[derive(Debug, Clone, Default, PartialEq)]
pub enum Zone {
    #[default]
    Utc,
    /// Floating times, and TZIDs without a VTIMEZONE.
    Local,
    Rules(Arc<Vec<Observance>>),
}

/// A STANDARD or DAYLIGHT block of a VTIMEZONE.
#[derive(Debug, Clone, PartialEq)]
pub struct Observance {
    start: NaiveDateTime,
    offset_from: i32,
    offset_to: i32,
    /// (month, nth weekday) of a yearly rule like BYMONTH=3;BYDAY=-1SU.
    yearly: Option<(u32, i32, Weekday)>,
}

impl Zone {
    fn offset_at(&self, local: NaiveDateTime) -> i32 {
        match self {
            Zone::Utc => 0,
            Zone::Local => Local.from_local_datetime(&local).earliest().map_or(0, |d| d.offset().local_minus_utc()),
            Zone::Rules(obs) => {
                // The observance that began last before `local` is in force.
                let onset = |o: &Observance| -> Option<NaiveDateTime> {
                    match o.yearly {
                        None => (o.start <= local).then_some(o.start),
                        Some((month, n, wd)) => [local.year(), local.year() - 1].into_iter().find_map(|y| {
                            let t = nth_weekday(y, month, n, wd)?.and_time(o.start.time());
                            (t <= local && t >= o.start).then_some(t)
                        }),
                    }
                };
                match obs.iter().filter_map(|o| Some((onset(o)?, o.offset_to))).max_by_key(|(t, _)| *t) {
                    Some((_, offset)) => offset,
                    None => obs.iter().min_by_key(|o| o.start).map_or(0, |o| o.offset_from),
                }
            }
        }
    }

    fn to_utc(&self, local: NaiveDateTime) -> i64 {
        local.and_utc().timestamp() - self.offset_at(local) as i64
    }

    fn to_local(&self, ts: i64) -> NaiveDateTime {
        let utc = DateTime::from_timestamp(ts, 0).unwrap_or_default().naive_utc();
        // Offsets are looked up by local time; the first guess is off by at most the DST shift.
        let guess = utc + Duration::seconds(self.offset_at(utc) as i64);
        utc + Duration::seconds(self.offset_at(guess) as i64)
    }
}

/// The `n`th (negative: from the end) `wd` of a month.
fn nth_weekday(year: i32, month: u32, n: i32, wd: Weekday) -> Option<NaiveDate> {
    if n > 0 {
        let first = NaiveDate::from_ymd_opt(year, month, 1)?;
        let shift = (7 + wd.num_days_from_monday() as i64 - first.weekday().num_days_from_monday() as i64) % 7;
        let d = first + Duration::days(shift + 7 * (n as i64 - 1));
        (d.month() == month).then_some(d)
    } else {
        let next = if month == 12 { NaiveDate::from_ymd_opt(year + 1, 1, 1) } else { NaiveDate::from_ymd_opt(year, month + 1, 1) }?;
        let last = next.pred_opt()?;
        let shift = (7 + last.weekday().num_days_from_monday() as i64 - wd.num_days_from_monday() as i64) % 7;
        let d = last - Duration::days(shift + 7 * (-n as i64 - 1));
        (d.month() == month).then_some(d)
    }
}

// ---- reading ----

#[derive(Clone)]
struct Prop {
    name: String,
    params: Vec<(String, String)>,
    value: String,
}

impl Prop {
    fn param(&self, name: &str) -> Option<&str> {
        self.params.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }
}

#[derive(Default)]
struct Component {
    name: String,
    props: Vec<Prop>,
    children: Vec<Component>,
}

impl Component {
    fn prop(&self, name: &str) -> Option<&Prop> {
        self.props.iter().find(|p| p.name == name)
    }

    fn text(&self, name: &str) -> String {
        self.prop(name).map(|p| unescape(&p.value)).unwrap_or_default()
    }
}

/// Splits at the first `sep` outside double quotes.
fn split_unquoted(s: &str, sep: char) -> Vec<&str> {
    let mut parts = Vec::new();
    let (mut quoted, mut from) = (false, 0);
    for (i, c) in s.char_indices() {
        if c == '"' {
            quoted = !quoted;
        } else if c == sep && !quoted {
            parts.push(&s[from..i]);
            from = i + 1;
        }
    }
    parts.push(&s[from..]);
    parts
}

fn parse_line(line: &str) -> Option<Prop> {
    let colon = {
        let mut quoted = false;
        line.char_indices().find(|&(_, c)| {
            if c == '"' {
                quoted = !quoted;
            }
            c == ':' && !quoted
        })?.0
    };
    let mut head = split_unquoted(&line[..colon], ';').into_iter();
    let name = head.next()?.trim().to_ascii_uppercase();
    let params = head
        .filter_map(|p| p.split_once('='))
        .map(|(k, v)| (k.to_ascii_uppercase(), v.trim_matches('"').to_string()))
        .collect();
    Some(Prop { name, params, value: line[colon + 1..].to_string() })
}

fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('n' | 'N') => out.push('\n'),
                Some(other) => out.push(other),
                None => {}
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// Content lines, unfolded: a line starting with a space or tab continues the previous one.
fn unfold(text: &str) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    for line in text.split('\n').map(|l| l.strip_suffix('\r').unwrap_or(l)) {
        match (line.strip_prefix([' ', '\t']), lines.last_mut()) {
            (Some(rest), Some(last)) => last.push_str(rest),
            _ if line.is_empty() => {}
            _ => lines.push(line.to_string()),
        }
    }
    lines
}

fn components(text: &str) -> Vec<Component> {
    let lines = unfold(text);
    let mut stack = vec![Component::default()];
    for prop in lines.iter().filter_map(|l| parse_line(l)) {
        match prop.name.as_str() {
            "BEGIN" => stack.push(Component { name: prop.value.trim().to_ascii_uppercase(), ..Default::default() }),
            "END" if stack.len() > 1 => {
                let done = stack.pop().unwrap();
                stack.last_mut().unwrap().children.push(done);
            }
            _ => stack.last_mut().unwrap().props.push(prop),
        }
    }
    // Unclosed components (truncated input) still count.
    while stack.len() > 1 {
        let done = stack.pop().unwrap();
        stack.last_mut().unwrap().children.push(done);
    }
    stack.pop().unwrap().children
}

fn parse_offset(s: &str) -> Option<i32> {
    let s = s.trim();
    let sign = if s.starts_with('-') { -1 } else { 1 };
    let digits = s.trim_start_matches(['+', '-']);
    let h: i32 = digits.get(0..2)?.parse().ok()?;
    let m: i32 = digits.get(2..4)?.parse().ok()?;
    let sec: i32 = digits.get(4..6).and_then(|x| x.parse().ok()).unwrap_or(0);
    Some(sign * (h * 3600 + m * 60 + sec))
}

fn parse_naive(value: &str) -> Option<NaiveDateTime> {
    let v = value.trim().trim_end_matches('Z');
    match v.len() {
        8 => NaiveDate::parse_from_str(v, "%Y%m%d").ok().map(|d| d.and_time(NaiveTime::MIN)),
        _ => NaiveDateTime::parse_from_str(v, "%Y%m%dT%H%M%S").ok(),
    }
}

fn rule_parts(rule: &str) -> Vec<(String, String)> {
    rule.split(';').filter_map(|p| p.split_once('=')).map(|(k, v)| (k.to_ascii_uppercase(), v.to_string())).collect()
}

fn weekday(code: &str) -> Option<Weekday> {
    Some(match code {
        "MO" => Weekday::Mon,
        "TU" => Weekday::Tue,
        "WE" => Weekday::Wed,
        "TH" => Weekday::Thu,
        "FR" => Weekday::Fri,
        "SA" => Weekday::Sat,
        "SU" => Weekday::Sun,
        _ => return None,
    })
}

/// "-1SU" -> (-1, Sunday); "TU" -> (0, Tuesday).
fn parse_byday(s: &str) -> Option<(i32, Weekday)> {
    let s = s.trim();
    let split = s.len().checked_sub(2)?;
    let n = if split == 0 { 0 } else { s[..split].parse().ok()? };
    Some((n, weekday(&s[split..])?))
}

fn parse_zone(tz: &Component) -> Option<(String, Zone)> {
    let id = tz.prop("TZID")?.value.clone();
    let obs: Vec<Observance> = tz
        .children
        .iter()
        .filter(|c| c.name == "STANDARD" || c.name == "DAYLIGHT")
        .filter_map(|c| {
            let rule = c.prop("RRULE").map(|r| rule_parts(&r.value)).unwrap_or_default();
            let get = |k: &str| rule.iter().find(|(key, _)| key == k).map(|(_, v)| v.as_str());
            let yearly = match (get("BYMONTH"), get("BYDAY")) {
                (Some(m), Some(d)) => {
                    let (n, wd) = parse_byday(d)?;
                    Some((m.parse().ok()?, if n == 0 { 1 } else { n }, wd))
                }
                _ => None,
            };
            Some(Observance {
                start: parse_naive(&c.prop("DTSTART")?.value)?,
                offset_from: parse_offset(&c.prop("TZOFFSETFROM")?.value)?,
                offset_to: parse_offset(&c.prop("TZOFFSETTO")?.value)?,
                yearly,
            })
        })
        .collect();
    (!obs.is_empty()).then(|| (id, Zone::Rules(Arc::new(obs))))
}

/// (Unix time, all-day, zone) of a DTSTART-like property.
fn parse_time(prop: &Prop, zones: &[(String, Zone)]) -> Option<(i64, bool, Zone)> {
    let local = parse_naive(&prop.value)?;
    let date_only = prop.param("VALUE").is_some_and(|v| v.eq_ignore_ascii_case("DATE")) || prop.value.trim().len() == 8;
    let zone = if date_only || prop.value.trim().ends_with('Z') {
        Zone::Utc
    } else {
        match prop.param("TZID") {
            // shortcut: a TZID without VTIMEZONE is read as local time; add chrono-tz if senders turn up that skip it.
            Some(id) => zones.iter().find(|(z, _)| z == id).map_or(Zone::Local, |(_, z)| z.clone()),
            None => Zone::Local,
        }
    };
    Some((zone.to_utc(local), date_only, zone))
}

fn parse_person(prop: &Prop) -> Attendee {
    let value = prop.value.trim();
    let email = match value.get(..7) {
        Some(p) if p.eq_ignore_ascii_case("mailto:") => &value[7..],
        _ => value,
    };
    Attendee {
        email: email.to_string(),
        name: prop.param("CN").unwrap_or_default().to_string(),
        partstat: prop.param("PARTSTAT").unwrap_or("NEEDS-ACTION").to_ascii_uppercase(),
    }
}

/// Reads every VEVENT of every VCALENDAR in `text`. Unreadable events are skipped.
pub fn parse(text: &str) -> Calendar {
    let mut calendar = Calendar::default();
    for vcal in components(text).into_iter().filter(|c| c.name == "VCALENDAR") {
        if let Some(m) = vcal.prop("METHOD") {
            calendar.method = m.value.trim().to_ascii_uppercase();
        }
        let zones: Vec<(String, Zone)> = vcal.children.iter().filter(|c| c.name == "VTIMEZONE").filter_map(parse_zone).collect();
        for ev in vcal.children.iter().filter(|c| c.name == "VEVENT") {
            let Some((start, all_day, zone)) = ev.prop("DTSTART").and_then(|p| parse_time(p, &zones)) else { continue };
            let end = match (ev.prop("DTEND").and_then(|p| parse_time(p, &zones)), ev.prop("DURATION")) {
                (Some((end, _, _)), _) => end,
                (None, Some(d)) => start + parse_duration(&d.value).unwrap_or(0),
                (None, None) if all_day => start + 86400,
                (None, None) => start,
            };
            calendar.events.push(Event {
                uid: ev.text("UID"),
                summary: ev.text("SUMMARY"),
                location: ev.text("LOCATION"),
                description: ev.text("DESCRIPTION"),
                start,
                end: end.max(start),
                all_day,
                organizer: ev.prop("ORGANIZER").map(parse_person),
                attendees: ev.props.iter().filter(|p| p.name == "ATTENDEE").map(parse_person).collect(),
                sequence: ev.prop("SEQUENCE").and_then(|p| p.value.trim().parse().ok()).unwrap_or(0),
                status: ev.prop("STATUS").map(|p| p.value.trim().to_ascii_uppercase()).unwrap_or_default(),
                recurrence_id: ev.prop("RECURRENCE-ID").and_then(|p| parse_time(p, &zones)).map(|t| t.0),
                rrule: ev.prop("RRULE").map(|p| p.value.trim().to_string()),
                exdates: ev
                    .props
                    .iter()
                    .filter(|p| p.name == "EXDATE")
                    .flat_map(|p| {
                        p.value.split(',').filter_map(|v| parse_time(&Prop { value: v.to_string(), ..p.clone() }, &zones)).map(|t| t.0).collect::<Vec<_>>()
                    })
                    .collect(),
                zone,
            });
        }
    }
    calendar
}

/// Seconds of an RFC 5545 duration like "PT1H30M" or "P1D".
fn parse_duration(s: &str) -> Option<i64> {
    let s = s.trim();
    let (sign, s) = match s.strip_prefix('-') {
        Some(rest) => (-1, rest),
        None => (1, s.trim_start_matches('+')),
    };
    let mut total = 0;
    let mut num = String::new();
    for c in s.strip_prefix('P')?.chars() {
        match c {
            '0'..='9' => num.push(c),
            'T' => {}
            unit => {
                let n: i64 = num.parse().ok()?;
                num.clear();
                total += n * match unit {
                    'W' => 7 * 86400,
                    'D' => 86400,
                    'H' => 3600,
                    'M' => 60,
                    'S' => 1,
                    _ => return None,
                };
            }
        }
    }
    Some(sign * total)
}

// ---- repeating ----

/// Most instances one repeating event yields, against runaway rules.
const MAX_INSTANCES: usize = 5000;

/// The instances of `events` that overlap `from..to`: repeating events are expanded, and an
/// instance changed on its own (RECURRENCE-ID) replaces the one it was generated as.
pub fn expand(events: &[Event], from: i64, to: i64) -> Vec<Event> {
    let overlaps = |e: &Event| e.start < to && (e.end > from || (e.end == e.start && e.start >= from));
    let replaced = |uid: &str, start: i64| events.iter().any(|o| o.uid == uid && o.recurrence_id == Some(start));
    let mut out = Vec::new();
    for e in events {
        if e.status == "CANCELLED" {
            continue;
        }
        let Some(rule) = e.rrule.as_deref().filter(|_| e.recurrence_id.is_none()) else {
            if overlaps(e) {
                out.push(e.clone());
            }
            continue;
        };
        for start in occurrences(e, rule, to) {
            let instance = Event { start, end: start + (e.end - e.start), rrule: None, ..e.clone() };
            if overlaps(&instance) && !e.exdates.contains(&start) && !replaced(&e.uid, start) {
                out.push(instance);
            }
        }
    }
    out.sort_by_key(|e| e.start);
    out
}

/// Start times of a repeating event, up to `to`.
fn occurrences(e: &Event, rule: &str, to: i64) -> Vec<i64> {
    let parts = rule_parts(rule);
    let get = |k: &str| parts.iter().find(|(key, _)| key == k).map(|(_, v)| v.as_str());
    let interval: i64 = get("INTERVAL").and_then(|v| v.parse().ok()).filter(|&n| n > 0).unwrap_or(1);
    let count: Option<usize> = get("COUNT").and_then(|v| v.parse().ok());
    let until = get("UNTIL").and_then(|v| {
        let local = parse_naive(v)?;
        // A date-only or floating UNTIL is in the event's own zone.
        Some(if v.trim().ends_with('Z') { local.and_utc().timestamp() } else { e.zone.to_utc(local) })
    });
    let byday: Vec<(i32, Weekday)> = get("BYDAY").map(|v| v.split(',').filter_map(parse_byday).collect()).unwrap_or_default();
    let bymonthday: Vec<u32> = get("BYMONTHDAY").map(|v| v.split(',').filter_map(|d| d.trim().parse().ok()).collect()).unwrap_or_default();
    let first = e.zone.to_local(e.start);
    let time = first.time();
    let mut out = Vec::new();
    for k in 0..(MAX_INSTANCES as i64) {
        let mut dates: Vec<NaiveDate> = match get("FREQ").unwrap_or("") {
            "DAILY" => vec![first.date() + Duration::days(k * interval)],
            "WEEKLY" if !byday.is_empty() => {
                let week = first.date() - Duration::days(first.weekday().num_days_from_monday() as i64) + Duration::weeks(k * interval);
                byday.iter().map(|(_, wd)| week + Duration::days(wd.num_days_from_monday() as i64)).collect()
            }
            "WEEKLY" => vec![first.date() + Duration::weeks(k * interval)],
            "MONTHLY" => {
                let m0 = first.month0() as i64 + k * interval;
                let (y, m) = (first.year() + (m0 / 12) as i32, (m0 % 12) as u32 + 1);
                if !byday.is_empty() {
                    byday.iter().filter_map(|&(n, wd)| nth_weekday(y, m, if n == 0 { 1 } else { n }, wd)).collect()
                } else if !bymonthday.is_empty() {
                    bymonthday.iter().filter_map(|&d| NaiveDate::from_ymd_opt(y, m, d)).collect()
                } else {
                    NaiveDate::from_ymd_opt(y, m, first.day()).into_iter().collect()
                }
            }
            "YEARLY" => NaiveDate::from_ymd_opt(first.year() + (k * interval) as i32, first.month(), first.day()).into_iter().collect(),
            _ => return vec![e.start],
        };
        dates.sort();
        for date in dates {
            let ts = e.zone.to_utc(date.and_time(time));
            if ts < e.start {
                continue;
            }
            if until.is_some_and(|u| ts > u) || count.is_some_and(|c| out.len() >= c) || ts >= to || out.len() >= MAX_INSTANCES {
                return out;
            }
            out.push(ts);
        }
    }
    out
}

// ---- writing ----

fn escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace(';', "\\;").replace(',', "\\,").replace("\r\n", "\\n").replace('\n', "\\n")
}

/// Folds a content line at 75 octets (RFC 5545 3.1), not splitting characters.
fn fold(line: &str, out: &mut String) {
    let mut width = 0;
    for c in line.chars() {
        if width + c.len_utf8() > 75 {
            out.push_str("\r\n ");
            width = 1;
        }
        out.push(c);
        width += c.len_utf8();
    }
    out.push_str("\r\n");
}

fn time_value(ts: i64, all_day: bool) -> String {
    let t = DateTime::<Utc>::from_timestamp(ts, 0).unwrap_or_default();
    if all_day { format!(";VALUE=DATE:{}", t.format("%Y%m%d")) } else { format!(":{}", t.format("%Y%m%dT%H%M%SZ")) }
}

fn person_line(name: &str, p: &Attendee, extra: &str) -> String {
    let cn = if p.name.is_empty() { String::new() } else { format!(";CN=\"{}\"", p.name.replace('"', "")) };
    format!("{name}{cn}{extra}:mailto:{}", p.email)
}

/// A VCALENDAR with `method` (empty for an object stored on a CalDAV server). Times are written
/// in UTC, which every client reads; zones only matter for repeats, which this never writes.
pub fn write(method: &str, events: &[Event]) -> String {
    let mut out = String::new();
    let mut line = |l: String| fold(&l, &mut out);
    line("BEGIN:VCALENDAR".into());
    line("VERSION:2.0".into());
    line("PRODID:-//mailclient//EN".into());
    line("CALSCALE:GREGORIAN".into());
    if !method.is_empty() {
        line(format!("METHOD:{method}"));
    }
    let stamp = Utc::now().format("%Y%m%dT%H%M%SZ");
    for e in events {
        line("BEGIN:VEVENT".into());
        line(format!("UID:{}", e.uid));
        line(format!("DTSTAMP:{stamp}"));
        line(format!("DTSTART{}", time_value(e.start, e.all_day)));
        line(format!("DTEND{}", time_value(e.end, e.all_day)));
        if let Some(rid) = e.recurrence_id {
            line(format!("RECURRENCE-ID{}", time_value(rid, e.all_day)));
        }
        line(format!("SEQUENCE:{}", e.sequence));
        for (name, value) in [("SUMMARY", &e.summary), ("LOCATION", &e.location), ("DESCRIPTION", &e.description)] {
            if !value.is_empty() {
                line(format!("{name}:{}", escape(value)));
            }
        }
        if !e.status.is_empty() {
            line(format!("STATUS:{}", e.status));
        }
        if let Some(org) = &e.organizer {
            line(person_line("ORGANIZER", org, ""));
        }
        for a in &e.attendees {
            // A stored copy: the client mails the invitations, so the server must not (RFC 6638).
            let agent = if method.is_empty() { ";SCHEDULE-AGENT=CLIENT" } else { "" };
            let rsvp = if a.partstat == "NEEDS-ACTION" { ";RSVP=TRUE" } else { "" };
            line(person_line("ATTENDEE", a, &format!(";ROLE=REQ-PARTICIPANT;PARTSTAT={}{rsvp}{agent}", a.partstat)));
        }
        line("END:VEVENT".into());
    }
    line("END:VCALENDAR".into());
    out
}

/// The REPLY to an invitation, answering for attendee `me` with ACCEPTED, TENTATIVE or DECLINED.
pub fn reply(invite: &Event, me: &str, partstat: &str) -> String {
    let mine = invite.attendees.iter().find(|a| a.email.eq_ignore_ascii_case(me));
    let attendee = Attendee {
        email: mine.map_or(me, |a| a.email.as_str()).to_string(),
        name: mine.map(|a| a.name.clone()).unwrap_or_default(),
        partstat: partstat.to_string(),
    };
    let event = Event {
        attendees: vec![attendee],
        description: String::new(),
        status: String::new(),
        ..invite.clone()
    };
    write("REPLY", &[event])
}

/// The calendar text with its METHOD line dropped: what a CalDAV server accepts for storing.
pub fn without_method(text: &str) -> String {
    let mut out = String::new();
    let mut skipping = false;
    for line in text.split_inclusive('\n') {
        let continued = line.starts_with([' ', '\t']);
        if !continued {
            skipping = line.len() >= 7 && line[..7].eq_ignore_ascii_case("METHOD:");
        }
        if !skipping {
            out.push_str(line);
        }
    }
    out
}

/// `text` with the VEVENTs of `new.uid` changed to `new`'s title, location, notes and status, and
/// with `full` also its times, organizer and attendees (not for a repeating series, whose
/// instances keep their times). Everything else stays as the calendar wrote it: alarms, zones,
/// other apps' properties, attendees' answers. SEQUENCE goes up, so receivers see a newer version.
pub fn patch(text: &str, new: &Event, full: bool) -> String {
    let sequence = parse(text).events.iter().filter(|e| e.uid == new.uid).map(|e| e.sequence).max().unwrap_or(0) + 1;
    let mut replaced = vec!["SUMMARY", "LOCATION", "DESCRIPTION", "SEQUENCE", "DTSTAMP"];
    if !new.status.is_empty() {
        replaced.push("STATUS");
    }
    if full {
        replaced.extend(["DTSTART", "DTEND", "DURATION", "ORGANIZER", "ATTENDEE"]);
    }
    let mut fresh = vec![format!("DTSTAMP:{}", Utc::now().format("%Y%m%dT%H%M%SZ")), format!("SEQUENCE:{sequence}")];
    for (name, value) in [("SUMMARY", &new.summary), ("LOCATION", &new.location), ("DESCRIPTION", &new.description)] {
        if !value.is_empty() {
            fresh.push(format!("{name}:{}", escape(value)));
        }
    }
    if !new.status.is_empty() {
        fresh.push(format!("STATUS:{}", new.status));
    }
    if full {
        fresh.push(format!("DTSTART{}", time_value(new.start, new.all_day)));
        fresh.push(format!("DTEND{}", time_value(new.end, new.all_day)));
        fresh.extend(new.organizer.iter().map(|o| person_line("ORGANIZER", o, "")));
    }

    let mut out = String::new();
    // The VEVENT being read, as (nesting level, line); alarms inside it are level 1.
    let mut event: Option<Vec<(usize, String)>> = None;
    let mut level = 0;
    for line in unfold(text) {
        let prop = parse_line(&line);
        let (name, value) = prop.as_ref().map_or(("", ""), |p| (p.name.as_str(), p.value.trim()));
        match event.as_mut() {
            None if name == "BEGIN" && value.eq_ignore_ascii_case("VEVENT") => event = Some(vec![(0, line)]),
            None => fold(&line, &mut out),
            Some(lines) if name == "END" && level == 0 => {
                lines.push((0, line));
                let lines = event.take().unwrap();
                let uid = lines.iter().find_map(|(l, x)| (*l == 0).then(|| parse_line(x)).flatten().filter(|p| p.name == "UID")).map(|p| p.value.trim().to_string());
                let ours = uid.as_deref() == Some(new.uid.as_str());
                let mut attendees_kept = Vec::new();
                for (i, (l, x)) in lines.iter().enumerate() {
                    let p = parse_line(x);
                    let pname = p.as_ref().map_or("", |p| p.name.as_str());
                    if ours && *l == 0 && pname == "ATTENDEE" && full {
                        // Attendees still invited keep their line, with their answer.
                        let email = parse_person(p.as_ref().unwrap()).email;
                        if new.attendees.iter().any(|a| a.email.eq_ignore_ascii_case(&email)) {
                            attendees_kept.push(email);
                            fold(x, &mut out);
                        }
                        continue;
                    }
                    if ours && *l == 0 && i > 0 && replaced.contains(&pname) {
                        continue;
                    }
                    if ours && i == lines.len() - 1 {
                        for f in &fresh {
                            fold(f, &mut out);
                        }
                        if full {
                            for a in new.attendees.iter().filter(|a| !attendees_kept.iter().any(|k| k.eq_ignore_ascii_case(&a.email))) {
                                fold(&person_line("ATTENDEE", a, ";ROLE=REQ-PARTICIPANT;PARTSTAT=NEEDS-ACTION;RSVP=TRUE;SCHEDULE-AGENT=CLIENT"), &mut out);
                            }
                        }
                    }
                    fold(x, &mut out);
                }
            }
            Some(lines) => {
                if name == "END" {
                    level -= 1;
                }
                lines.push((level, line));
                if name == "BEGIN" {
                    level += 1;
                }
            }
        }
    }
    out
}

/// The calendar text with `email`'s answer in the VEVENTs of `uid` set to `partstat`: what an
/// organizer's calendar records when a REPLY arrives. Nothing else changes, SEQUENCE included.
pub fn set_partstat(text: &str, uid: &str, email: &str, partstat: &str) -> String {
    let mut out = String::new();
    // The VEVENT being read, as (nesting level, line); alarms inside it are level 1.
    let mut event: Option<Vec<(usize, String)>> = None;
    let mut level = 0;
    for line in unfold(text) {
        let prop = parse_line(&line);
        let (name, value) = prop.as_ref().map_or(("", ""), |p| (p.name.as_str(), p.value.trim()));
        match event.as_mut() {
            None if name == "BEGIN" && value.eq_ignore_ascii_case("VEVENT") => event = Some(vec![(0, line)]),
            None => fold(&line, &mut out),
            Some(lines) if name == "END" && level == 0 => {
                lines.push((0, line));
                let lines = event.take().unwrap();
                let ours = lines.iter().any(|(l, x)| *l == 0 && parse_line(x).is_some_and(|p| p.name == "UID" && p.value.trim() == uid));
                for (l, x) in lines {
                    match parse_line(&x) {
                        Some(mut p) if ours && l == 0 && p.name == "ATTENDEE" && parse_person(&p).email.eq_ignore_ascii_case(email) => {
                            p.params.retain(|(k, _)| k != "PARTSTAT" && k != "RSVP");
                            p.params.push(("PARTSTAT".into(), partstat.into()));
                            let params: String = p
                                .params
                                .iter()
                                .map(|(k, v)| if v.contains([',', ';', ':']) { format!(";{k}=\"{v}\"") } else { format!(";{k}={v}") })
                                .collect();
                            fold(&format!("{}{params}:{}", p.name, p.value), &mut out);
                        }
                        _ => fold(&x, &mut out),
                    }
                }
            }
            Some(lines) => {
                if name == "END" {
                    level -= 1;
                }
                lines.push((level, line));
                if name == "BEGIN" {
                    level += 1;
                }
            }
        }
    }
    out
}

/// The calendar text with `method` (REQUEST, CANCEL), for mailing it.
pub fn with_method(text: &str, method: &str) -> String {
    let text = without_method(text);
    match text.find('\n') {
        Some(i) => format!("{}METHOD:{method}\r\n{}", &text[..=i], &text[i + 1..]),
        None => text,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn utc(s: &str) -> i64 {
        NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M").unwrap().and_utc().timestamp()
    }

    /// An Outlook invitation: Windows zone name, VTIMEZONE with yearly DST rules, folded lines.
    const OUTLOOK: &str = "BEGIN:VCALENDAR\r\nMETHOD:REQUEST\r\nPRODID:Microsoft Exchange Server 2010\r\nVERSION:2.0\r\n\
BEGIN:VTIMEZONE\r\nTZID:W. Europe Standard Time\r\nBEGIN:STANDARD\r\nDTSTART:16010101T030000\r\nTZOFFSETFROM:+0200\r\nTZOFFSETTO:+0100\r\n\
RRULE:FREQ=YEARLY;INTERVAL=1;BYDAY=-1SU;BYMONTH=10\r\nEND:STANDARD\r\nBEGIN:DAYLIGHT\r\nDTSTART:16010101T020000\r\nTZOFFSETFROM:+0100\r\n\
TZOFFSETTO:+0200\r\nRRULE:FREQ=YEARLY;INTERVAL=1;BYDAY=-1SU;BYMONTH=3\r\nEND:DAYLIGHT\r\nEND:VTIMEZONE\r\n\
BEGIN:VEVENT\r\nORGANIZER;CN=\"Doe, Jane\":mailto:jane@example.com\r\nATTENDEE;ROLE=REQ-PARTICIPANT;PARTSTAT=NEEDS-ACTION;RSVP=TRUE;CN=Max:mailto:max@example.org\r\n\
DESCRIPTION;LANGUAGE=en-US:Agenda:\\n1. Budget\\, Q4\\n\r\nUID:040000008200E00074C5B7101A82E008\r\nSUMMARY;LANGUAGE=en-US:Quarterly re\r\n view\r\n\
DTSTART;TZID=W. Europe Standard Time:20261020T100000\r\nDTEND;TZID=W. Europe Standard Time:20261020T113000\r\nSEQUENCE:2\r\n\
LOCATION;LANGUAGE=en-US:Room 4\r\nSTATUS:CONFIRMED\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";

    #[test]
    fn reads_outlook_invitation() {
        let cal = parse(OUTLOOK);
        assert_eq!(cal.method, "REQUEST");
        let [e] = cal.events.as_slice() else { panic!("{cal:?}") };
        assert_eq!(e.summary, "Quarterly review");
        assert_eq!(e.description, "Agenda:\n1. Budget, Q4\n");
        // 20 October is still summer time (+02:00).
        assert_eq!((e.start, e.end), (utc("2026-10-20 08:00"), utc("2026-10-20 09:30")));
        assert_eq!(e.organizer.as_ref().map(|o| (o.email.as_str(), o.name.as_str())), Some(("jane@example.com", "Doe, Jane")));
        assert_eq!(e.attendees[0].email, "max@example.org");
        assert_eq!(e.sequence, 2);
        // After the last Sunday of October: winter time (+01:00).
        assert_eq!(e.zone.to_utc(parse_naive("20261026T100000").unwrap()), utc("2026-10-26 09:00"));
    }

    #[test]
    fn repeats_in_wall_clock_time() {
        let mut e = parse(OUTLOOK).events.remove(0);
        e.rrule = Some("FREQ=WEEKLY;BYDAY=TU;COUNT=3".into());
        e.exdates = vec![];
        let starts: Vec<i64> = expand(&[e.clone()], 0, i64::MAX).iter().map(|e| e.start).collect();
        // 10:00 local before and after the switch to winter time on 25 October.
        assert_eq!(starts, [utc("2026-10-20 08:00"), utc("2026-10-27 09:00"), utc("2026-11-03 09:00")]);

        e.rrule = Some("FREQ=MONTHLY;BYDAY=-1FR;UNTIL=20261231T235959Z".into());
        let fridays: Vec<i64> = expand(&[e.clone()], 0, i64::MAX).iter().map(|e| e.start).collect();
        assert_eq!(fridays, [utc("2026-10-30 09:00"), utc("2026-11-27 09:00"), utc("2026-12-25 09:00")]);

        // An instance moved on its own replaces the generated one; EXDATE removes one.
        e.rrule = Some("FREQ=DAILY;COUNT=4".into());
        e.exdates = vec![utc("2026-10-21 08:00")];
        let moved = Event { recurrence_id: Some(utc("2026-10-22 08:00")), start: utc("2026-10-22 12:00"), end: utc("2026-10-22 13:00"), rrule: None, ..e.clone() };
        let starts: Vec<i64> = expand(&[e, moved], utc("2026-10-01 00:00"), utc("2026-11-01 00:00")).iter().map(|e| e.start).collect();
        assert_eq!(starts, [utc("2026-10-20 08:00"), utc("2026-10-22 12:00"), utc("2026-10-23 08:00")]);
    }

    #[test]
    fn reads_google_all_day_and_utc() {
        let text = "BEGIN:VCALENDAR\nPRODID:-//Google Inc//Google Calendar 70.9054//EN\nVERSION:2.0\nMETHOD:REQUEST\nBEGIN:VEVENT\n\
DTSTART;VALUE=DATE:20261224\nDTEND;VALUE=DATE:20261227\nUID:a1@google.com\nSUMMARY:Holidays\nEND:VEVENT\nBEGIN:VEVENT\n\
DTSTART:20261101T150000Z\nDURATION:PT45M\nUID:b2@google.com\nSUMMARY:Call\nEND:VEVENT\nEND:VCALENDAR\n";
        let cal = parse(text);
        assert_eq!(cal.events.len(), 2);
        assert!(cal.events[0].all_day);
        assert_eq!((cal.events[0].start, cal.events[0].end), (utc("2026-12-24 00:00"), utc("2026-12-27 00:00")));
        assert_eq!((cal.events[1].start, cal.events[1].end), (utc("2026-11-01 15:00"), utc("2026-11-01 15:45")));
        // Only the window's instances.
        assert_eq!(expand(&cal.events, utc("2026-12-01 00:00"), utc("2027-01-01 00:00")).len(), 1);
    }

    #[test]
    fn patches_keep_what_was_not_edited() {
        let mut text = OUTLOOK.replace("STATUS:CONFIRMED\r\n", "STATUS:CONFIRMED\r\nX-MOZ-SNOOZE:1\r\nBEGIN:VALARM\r\nACTION:DISPLAY\r\nDESCRIPTION:Reminder\r\nTRIGGER:-PT15M\r\nEND:VALARM\r\n");
        text = without_method(&text);
        let old = parse(&text).events.remove(0);
        let new = Event {
            summary: "Quarterly review (moved)".into(),
            start: old.start + 3600,
            end: old.end + 3600,
            attendees: vec![old.attendees[0].clone(), Attendee { email: "new@example.org".into(), partstat: "NEEDS-ACTION".into(), ..Default::default() }],
            ..old.clone()
        };
        let patched = patch(&text, &new, true);
        let back = parse(&patched).events.remove(0);
        assert_eq!((back.summary.as_str(), back.start, back.end, back.sequence), ("Quarterly review (moved)", old.start + 3600, old.end + 3600, 3));
        assert_eq!(back.location, "Room 4");
        let emails: Vec<&str> = back.attendees.iter().map(|a| a.email.as_str()).collect();
        assert_eq!(emails, ["max@example.org", "new@example.org"]);
        let flat = patched.replace("\r\n ", "");
        // The alarm keeps its own DESCRIPTION; other apps' properties stay.
        assert!(flat.contains("DESCRIPTION:Reminder") && flat.contains("X-MOZ-SNOOZE:1") && flat.contains("BEGIN:VTIMEZONE"), "{flat}");
        // The event's own start is replaced, not added to.
        assert_eq!(flat[flat.find("BEGIN:VEVENT").unwrap()..].matches("DTSTART").count(), 1, "{flat}");
        // A series only changes its texts, in every instance.
        let series = patch(&text, &Event { summary: "Renamed".into(), start: 0, end: 0, ..old.clone() }, false);
        assert_eq!(parse(&series).events[0].start, old.start);
        let mailed = with_method(&series, "CANCEL");
        assert_eq!(parse(&mailed).method, "CANCEL");
        assert!(mailed.starts_with("BEGIN:VCALENDAR\r\nMETHOD:CANCEL\r\n"));
    }

    #[test]
    fn records_an_answer() {
        let text = without_method(OUTLOOK).replace("STATUS:CONFIRMED\r\n", "STATUS:CONFIRMED\r\nBEGIN:VALARM\r\nACTION:EMAIL\r\nATTENDEE:mailto:max@example.org\r\nTRIGGER:-PT5M\r\nEND:VALARM\r\n");
        let answered = set_partstat(&text, "040000008200E00074C5B7101A82E008", "MAX@example.org", "ACCEPTED");
        let e = parse(&answered).events.remove(0);
        assert_eq!((e.attendees[0].email.as_str(), e.attendees[0].partstat.as_str(), e.attendees[0].name.as_str()), ("max@example.org", "ACCEPTED", "Max"));
        assert_eq!(e.sequence, 2, "an answer is no new version");
        let flat = answered.replace("\r\n ", "");
        // Only the event's attendee, not the alarm's recipient; the organizer's quoted name survives.
        assert!(flat.contains("ATTENDEE:mailto:max@example.org\r\nTRIGGER"), "{flat}");
        assert!(flat.contains("ORGANIZER;CN=\"Doe, Jane\":mailto:jane@example.com"), "{flat}");
        // Another event's UID: nothing changes.
        assert_eq!(parse(&set_partstat(&text, "other", "max@example.org", "DECLINED")).events[0].attendees[0].partstat, "NEEDS-ACTION");
    }

    #[test]
    fn writes_what_it_reads() {
        let invite = Event {
            uid: "u1@example.org".into(),
            summary: "Lunch; with, friends".into(),
            location: "Café Central".into(),
            description: "Line one\nLine two ".repeat(8),
            start: utc("2026-10-15 11:00"),
            end: utc("2026-10-15 12:00"),
            organizer: Some(Attendee { email: "me@example.org".into(), name: "Me".into(), partstat: "NEEDS-ACTION".into() }),
            attendees: vec![Attendee { email: "max@example.org".into(), name: "Max".into(), partstat: "NEEDS-ACTION".into() }],
            ..Default::default()
        };
        let text = write("REQUEST", std::slice::from_ref(&invite));
        assert!(text.lines().all(|l| l.len() <= 76), "{text}");
        assert!(text.replace("\r\n ", "").contains("ATTENDEE;CN=\"Max\";ROLE=REQ-PARTICIPANT;PARTSTAT=NEEDS-ACTION;RSVP=TRUE:mailto:max@example.org"));
        let back = parse(&text);
        assert_eq!(back.method, "REQUEST");
        assert_eq!(back.events[0], Event { status: String::new(), ..invite.clone() });

        let answer = parse(&reply(&invite, "MAX@example.org", "ACCEPTED"));
        assert_eq!(answer.method, "REPLY");
        assert_eq!(answer.events[0].attendees, [Attendee { email: "max@example.org".into(), name: "Max".into(), partstat: "ACCEPTED".into() }]);
        assert_eq!(answer.events[0].uid, "u1@example.org");

        let stored = without_method(&text);
        assert!(!stored.contains("METHOD") && !stored.contains("SCHEDULE-AGENT"));
        assert!(write("", &[invite]).replace("\r\n ", "").contains(";SCHEDULE-AGENT=CLIENT:mailto:max@example.org"));
    }
}
