//! The calendar page (month, week and day views; opening, creating, editing and deleting
//! events) and invitations in the reader.

use chrono::{DateTime, Datelike, Duration, Local, NaiveDate, NaiveTime, TimeZone, Timelike, Utc};
use mailcore::ical::{self, Attendee, Event};
use mailcore::store::{LOCAL_CALENDAR, StoredEvent};

use super::*;
use crate::{AgendaItem, AttendeeItem, CalendarChip, CalendarDay, DayColumn, EventBlock, EventDetail, EventForm};

/// Events shown in one cell of the month grid; the rest are counted as "+N more".
const CHIPS_PER_DAY: usize = 3;
/// Shortest event in the week and day views, in minutes, so it stays readable and clickable.
const MIN_BLOCK_MINUTES: i64 = 30;

/// An event on the calendar page; its index in [`State::calendar_shown`] is its id in the UI.
#[derive(Clone)]
pub(super) struct Shown {
    pub account: usize,
    pub color: slint::Color,
    pub stored: StoredEvent,
}

/// Monday of the week holding the 1st of `day`'s month: the month grid's first cell.
fn grid_start(day: NaiveDate) -> NaiveDate {
    monday(day.with_day(1).unwrap_or(day))
}

fn monday(day: NaiveDate) -> NaiveDate {
    day - Duration::days(day.weekday().num_days_from_monday() as i64)
}

fn local_midnight(day: NaiveDate) -> i64 {
    Local.from_local_datetime(&day.and_time(NaiveTime::MIN)).earliest().map_or(0, |t| t.timestamp())
}

fn local(ts: i64) -> DateTime<Local> {
    DateTime::from_timestamp(ts, 0).unwrap_or_default().with_timezone(&Local)
}

fn utc_date(ts: i64) -> NaiveDate {
    DateTime::<Utc>::from_timestamp(ts, 0).unwrap_or_default().date_naive()
}

/// First and last day an event covers. All-day events keep their date in every zone.
fn days_of(e: &Event) -> (NaiveDate, NaiveDate) {
    let last = (e.end - 1).max(e.start);
    if e.all_day { (utc_date(e.start), utc_date(last)) } else { (local(e.start).date_naive(), local(last).date_naive()) }
}

/// "Tue, 20 Oct 2026, 10:00 – 11:30", "Thu, 24 Dec – Sat, 26 Dec 2026".
fn when(e: &Event) -> String {
    let (first, last) = days_of(e);
    let mut text = match (e.all_day, first == last) {
        (true, true) => first.format("%a, %-d %b %Y").to_string(),
        (true, false) => format!("{} – {}", first.format("%a, %-d %b"), last.format("%a, %-d %b %Y")),
        (false, true) => format!("{}, {} – {}", first.format("%a, %-d %b %Y"), local(e.start).format("%H:%M"), local(e.end).format("%H:%M")),
        (false, false) => format!("{} – {}", local(e.start).format("%a, %-d %b, %H:%M"), local(e.end).format("%a, %-d %b %Y, %H:%M")),
    };
    if e.rrule.is_some() {
        text.push_str(" (repeats)");
    }
    text
}

fn hex_color(s: &str) -> Option<slint::Color> {
    let rgb = u32::from_str_radix(s.trim().strip_prefix('#')?.get(..6)?, 16).ok()?;
    Some(slint::Color::from_argb_encoded(0xff00_0000 | rgb))
}

fn title(e: &Event) -> SharedString {
    if e.summary.is_empty() { "(no title)".into() } else { e.summary.clone().into() }
}

fn partstat_verb(partstat: &str) -> &'static str {
    match partstat {
        "ACCEPTED" => "Accepted",
        "TENTATIVE" => "Tentative",
        "DECLINED" => "Declined",
        "DELEGATED" => "Delegated",
        _ => "No answer yet",
    }
}

/// "Doe, Jane" <jane@example.com>: quoted, since Outlook's names hold commas, which separate addresses.
fn address(name: &str, email: &str) -> String {
    match name {
        "" => email.to_string(),
        name => format!("\"{}\" <{email}>", name.replace('"', "")),
    }
}

/// The plain-text part of an invitation, for clients that show no calendar card.
fn invitation_text(e: &Event) -> String {
    let mut text = format!("{}\n\nWhen: {}\n", e.summary, when(e));
    if !e.location.is_empty() {
        text.push_str(&format!("Where: {}\n", e.location));
    }
    if !e.attendees.is_empty() {
        let who: Vec<String> = e.attendees.iter().map(|a| format::person(&a.name, &a.email)).collect();
        text.push_str(&format!("Invited: {}\n", who.join(", ")));
    }
    if !e.description.is_empty() {
        text.push_str(&format!("\n{}\n", e.description));
    }
    text
}

/// (lane, lanes) for events given as (start, end), sorted by start: overlapping events share
/// their column side by side, each group of overlapping events as wide as it needs.
fn lanes(spans: &[(i64, i64)]) -> Vec<(i32, i32)> {
    let mut out = vec![(0, 1); spans.len()];
    let (mut group_start, mut group_end) = (0, i64::MIN);
    let mut lane_ends: Vec<i64> = Vec::new();
    for (i, &(start, end)) in spans.iter().enumerate() {
        if start >= group_end && i > 0 {
            for o in &mut out[group_start..i] {
                o.1 = lane_ends.len() as i32;
            }
            lane_ends.clear();
            group_start = i;
        }
        let lane = match lane_ends.iter().position(|&e| e <= start) {
            Some(free) => free,
            None => {
                lane_ends.push(0);
                lane_ends.len() - 1
            }
        };
        lane_ends[lane] = end;
        out[i].0 = lane as i32;
        group_end = if i == group_start { end } else { group_end.max(end) };
    }
    for o in &mut out[group_start..] {
        o.1 = lane_ends.len().max(1) as i32;
    }
    out
}

impl Controller {
    pub(super) fn open_calendar(&self) {
        self.state.borrow_mut().calendar_day.get_or_insert_with(|| Local::now().date_naive());
        let window = self.window();
        window.set_calendar_editing(false);
        window.set_calendar_showing_event(false);
        window.set_show_calendar(true);
        self.render_calendar();
        self.sync_calendars();
    }

    pub(super) fn set_calendar_day(&self, day: NaiveDate) {
        self.state.borrow_mut().calendar_day = Some(day);
        self.render_calendar();
    }

    pub(super) fn calendar_day(&self) -> NaiveDate {
        self.state.borrow().calendar_day.unwrap_or_else(|| Local::now().date_naive())
    }

    pub(super) fn set_calendar_view(&self, view: i32) {
        self.window().set_calendar_view(view);
        self.render_calendar();
    }

    /// Previous (-1) or next (1) month, week or day.
    pub(super) fn shift_calendar(&self, n: i32) {
        let day = self.calendar_day();
        let shifted = match self.window().get_calendar_view() {
            0 => {
                let first = day.with_day(1).unwrap_or(day);
                let months = chrono::Months::new(n.unsigned_abs());
                if n < 0 { first.checked_sub_months(months) } else { first.checked_add_months(months) }
            }
            1 => Some(day + Duration::weeks(n as i64)),
            _ => Some(day + Duration::days(n as i64)),
        };
        if let Some(day) = shifted {
            self.set_calendar_day(day);
        }
    }

    /// A month cell selects its day; a week column's heading opens that day.
    pub(super) fn calendar_day_clicked(&self, index: usize) {
        let day = self.calendar_day();
        match self.window().get_calendar_view() {
            0 => self.set_calendar_day(grid_start(day) + Duration::days(index as i64)),
            1 => {
                self.state.borrow_mut().calendar_day = Some(monday(day) + Duration::days(index as i64));
                self.set_calendar_view(2);
            }
            _ => {}
        }
    }

    /// The calendar page's banner: each failing account's last error.
    pub(super) fn show_calendar_errors(&self) {
        let state = self.state.borrow();
        let several = state.accounts.len() > 1;
        let lines: Vec<String> = state
            .accounts
            .iter()
            .filter_map(|a| {
                let e = state.calendar_errors.get(&a.id)?;
                Some(if several { format!("{}: {e}", a.config.email) } else { e.clone() })
            })
            .collect();
        self.window().set_calendar_error(lines.join("\n").into());
    }

    pub(super) fn sync_calendars(&self) {
        for account in &self.state.borrow().accounts {
            account.engine.send(Command::SyncCalendars);
        }
    }

    /// Every account's events overlapping `from..to`, with the color to draw them in.
    fn calendar_events(&self, from: i64, to: i64) -> Vec<Shown> {
        let state = self.state.borrow();
        let count = state.accounts.len();
        let mut all = Vec::new();
        for (i, account) in state.accounts.iter().enumerate() {
            let fallback = match folders::account_color(i, count) {
                c if c.alpha() == 0 => folders::account_color(0, 2),
                c => c,
            };
            match account.store.events(from, to) {
                Ok(events) => all.extend(
                    events.into_iter().map(|stored| Shown { account: i, color: hex_color(&stored.color).unwrap_or(fallback), stored }),
                ),
                Err(e) => log::warn!("reading events of {}: {e:#}", account.config.email),
            }
        }
        // All-day events first, as calendars show them on top.
        all.sort_by_key(|s| (!s.stored.event.all_day, s.stored.event.start));
        all
    }

    pub(super) fn render_calendar(&self) {
        let window = self.window();
        if !window.get_show_calendar() {
            return;
        }
        let view = window.get_calendar_view();
        let selected = self.calendar_day();
        let (start, count) = match view {
            0 => (grid_start(selected), 42),
            1 => (monday(selected), 7),
            _ => (selected, 1),
        };
        let today = Local::now().date_naive();
        // A day of margin: all-day events are placed by their UTC date.
        let shown = self.calendar_events(local_midnight(start) - 86400, local_midnight(start + Duration::days(count)) + 86400);
        let spans: Vec<(NaiveDate, NaiveDate)> = shown.iter().map(|s| days_of(&s.stored.event)).collect();
        let on = |day: NaiveDate| shown.iter().enumerate().zip(&spans).filter(move |(_, (first, last))| *first <= day && day <= *last).map(|((id, s), (first, _))| (id as i32, s, *first));

        if view == 0 {
            let days: Vec<CalendarDay> = (0..count)
                .map(|i| {
                    let day = start + Duration::days(i);
                    let chips: Vec<CalendarChip> = on(day)
                        .map(|(id, s, first)| {
                            let e = &s.stored.event;
                            CalendarChip {
                                id,
                                time: if e.all_day || first != day { "".into() } else { local(e.start).format("%H:%M").to_string().into() },
                                title: title(e),
                                color: s.color,
                            }
                        })
                        .collect();
                    CalendarDay {
                        day: day.day() as i32,
                        in_month: day.month() == selected.month(),
                        today: day == today,
                        more: chips.len().saturating_sub(CHIPS_PER_DAY) as i32,
                        chips: ModelRc::new(VecModel::from(chips.into_iter().take(CHIPS_PER_DAY).collect::<Vec<_>>())),
                    }
                })
                .collect();
            window.set_calendar_days(ModelRc::new(VecModel::from(days)));
            window.set_calendar_selected((selected - start).num_days() as i32);
            window.set_calendar_period_label(selected.format("%B %Y").to_string().into());
        } else {
            let columns: Vec<DayColumn> = (0..count)
                .map(|i| {
                    let day = start + Duration::days(i);
                    let (day_start, day_end) = (local_midnight(day), local_midnight(day + Duration::days(1)));
                    let all_day: Vec<EventBlock> = on(day)
                        .filter(|(_, s, _)| s.stored.event.all_day)
                        .map(|(id, s, _)| EventBlock { id, title: title(&s.stored.event), color: s.color, ..Default::default() })
                        .collect();
                    // Timed events, cut to this day.
                    let timed: Vec<(i32, &Shown, i64, i64)> = on(day)
                        .filter(|(_, s, _)| !s.stored.event.all_day)
                        .map(|(id, s, _)| (id, s, s.stored.event.start.max(day_start), s.stored.event.end.min(day_end).max(s.stored.event.start)))
                        .collect();
                    let drawn: Vec<(i64, i64)> = timed.iter().map(|&(_, _, from, to)| (from, to.max(from + MIN_BLOCK_MINUTES * 60))).collect();
                    let blocks: Vec<EventBlock> = timed
                        .iter()
                        .zip(lanes(&drawn))
                        .map(|(&(id, s, from, to), (lane, lanes))| {
                            let e = &s.stored.event;
                            EventBlock {
                                id,
                                title: title(e),
                                time: format!("{} – {}", local(e.start).format("%H:%M"), local(e.end).format("%H:%M")).into(),
                                color: s.color,
                                start: ((from - day_start) / 60) as i32,
                                length: ((to - from) / 60) as i32,
                                lane,
                                lanes,
                            }
                        })
                        .collect();
                    DayColumn {
                        label: day.format(if view == 1 { "%a %-d" } else { "%A, %-d %B" }).to_string().into(),
                        today: day == today,
                        all_day: ModelRc::new(VecModel::from(all_day)),
                        blocks: ModelRc::new(VecModel::from(blocks)),
                    }
                })
                .collect();
            window.set_calendar_columns(ModelRc::new(VecModel::from(columns)));
            let last = start + Duration::days(count - 1);
            let label = match (view, start.month() == last.month()) {
                (1, true) => format!("{} – {}", start.format("%-d"), last.format("%-d %b %Y")),
                (1, false) => format!("{} – {}", start.format("%-d %b"), last.format("%-d %b %Y")),
                _ => selected.format("%A, %-d %B %Y").to_string(),
            };
            window.set_calendar_period_label(label.into());
        }

        let several_accounts = self.state.borrow().accounts.len() > 1;
        let agenda: Vec<AgendaItem> = on(selected)
            .map(|(id, s, _)| {
                let e = &s.stored.event;
                let mut detail = Vec::new();
                if let Some(org) = &e.organizer {
                    detail.push(format!("Organizer: {}", format::person(&org.name, &org.email)));
                }
                if several_accounts {
                    detail.push(self.state.borrow().accounts.get(s.account).map(|a| a.config.email.clone()).unwrap_or_default());
                }
                let notes: String = e.description.trim().chars().take(300).collect();
                if !notes.is_empty() {
                    detail.push(notes);
                }
                AgendaItem {
                    id,
                    time: if e.all_day { "All day".into() } else { format!("{} – {}", local(e.start).format("%H:%M"), local(e.end).format("%H:%M")).into() },
                    title: title(e),
                    location: e.location.clone().into(),
                    detail: detail.join("\n").into(),
                    color: s.color,
                }
            })
            .collect();
        window.set_calendar_day_label(selected.format("%A, %-d %B").to_string().into());
        window.set_calendar_agenda(ModelRc::new(VecModel::from(agenda)));
        // The opened event shows what the calendar now holds (answers that arrived, an edit saved).
        let open = self.state.borrow().calendar_open.clone();
        let fresh = open.and_then(|open| {
            shown
                .iter()
                .find(|s| {
                    s.account == open.account
                        && s.stored.event.uid == open.stored.event.uid
                        // A series' instances share the UID; a single event may have moved.
                        && (!open.stored.recurring || s.stored.event.start == open.stored.event.start)
                })
                .cloned()
        });
        self.state.borrow_mut().calendar_shown = shown;
        if let Some(fresh) = fresh {
            self.show_event_detail(fresh);
        }
    }

    // ---- opening, editing, deleting ----

    /// Whether the account may change this event: it organizes it, or nobody does.
    fn owns(&self, account: usize, e: &Event) -> bool {
        let me = self.state.borrow().accounts.get(account).map(|a| a.config.email.clone()).unwrap_or_default();
        e.organizer.as_ref().is_none_or(|o| o.email.eq_ignore_ascii_case(&me))
    }

    pub(super) fn open_event(&self, id: usize) {
        let Some(shown) = self.state.borrow().calendar_shown.get(id).cloned() else { return };
        self.show_event_detail(shown);
        let window = self.window();
        window.set_calendar_confirm_delete(false);
        window.set_calendar_editing(false);
        window.set_calendar_showing_event(true);
    }

    /// Fills the side panel with `shown` and makes it the opened event.
    fn show_event_detail(&self, shown: Shown) {
        let e = &shown.stored.event;
        let state = self.state.borrow();
        let account = state.accounts.get(shown.account).map(|a| a.config.email.clone()).unwrap_or_default();
        let mut calendar = match shown.stored.calendar.as_str() {
            LOCAL_CALENDAR => "On this device".to_string(),
            _ if shown.stored.calendar_name.is_empty() => "Calendar".to_string(),
            _ => shown.stored.calendar_name.clone(),
        };
        if state.accounts.len() > 1 {
            calendar.push_str(&format!(" · {account}"));
        }
        drop(state);
        let mut people: Vec<String> = e.organizer.iter().map(|o| format!("Organizer: {}", format::person(&o.name, &o.email))).collect();
        people.extend(e.attendees.iter().map(|a| format!("{}: {}", format::person(&a.name, &a.email), partstat_verb(&a.partstat))));
        let window = self.window();
        window.set_event_detail(EventDetail {
            title: title(e),
            when: when(e).into(),
            location: e.location.clone().into(),
            calendar: calendar.into(),
            people: people.join("\n").into(),
            notes: e.description.clone().into(),
            can_edit: self.owns(shown.account, e),
            recurring: shown.stored.recurring,
        });
        self.state.borrow_mut().calendar_open = Some(shown);
    }

    pub(super) fn new_event(&self) {
        let day = self.calendar_day();
        let now = Local::now();
        // The next full hour today (if a whole hour is left), else the morning of the chosen day.
        let start = if day == now.date_naive() && now.hour() < 22 { now.hour() + 1 } else { 9 };
        self.state.borrow_mut().calendar_editing = None;
        self.set_form_attendees(Vec::new());
        let window = self.window();
        window.set_event_form(EventForm {
            date: day.format("%Y-%m-%d").to_string().into(),
            start: format!("{start:02}:00").into(),
            end: format!("{:02}:00", start + 1).into(),
            ..Default::default()
        });
        let options: Vec<SharedString> = self.state.borrow().accounts.iter().map(|a| a.config.email.clone().into()).collect();
        window.set_event_from_options(ModelRc::new(VecModel::from(options)));
        window.set_event_from_index(self.compose_account() as i32);
        window.set_event_form_existing(false);
        window.set_event_error("".into());
        window.set_calendar_showing_event(false);
        window.set_calendar_editing(true);
    }

    /// Opens the form on the opened event.
    pub(super) fn edit_event(&self) {
        let Some(shown) = self.state.borrow().calendar_open.clone() else { return };
        let e = &shown.stored.event;
        let window = self.window();
        window.set_event_form(EventForm {
            title: e.summary.clone().into(),
            date: if e.all_day { utc_date(e.start) } else { local(e.start).date_naive() }.format("%Y-%m-%d").to_string().into(),
            start: local(e.start).format("%H:%M").to_string().into(),
            end: local(e.end).format("%H:%M").to_string().into(),
            all_day: e.all_day,
            location: e.location.clone().into(),
            notes: e.description.clone().into(),
        });
        self.set_form_attendees(e.attendees.clone());
        window.set_event_from_index(shown.account as i32);
        window.set_event_form_existing(true);
        window.set_event_error("".into());
        self.state.borrow_mut().calendar_editing = Some(shown);
        window.set_calendar_editing(true);
    }

    fn set_form_attendees(&self, attendees: Vec<Attendee>) {
        let items: Vec<AttendeeItem> = attendees
            .iter()
            .map(|a| AttendeeItem {
                name: a.name.clone().into(),
                email: a.email.clone().into(),
                status: match a.partstat.as_str() {
                    "ACCEPTED" | "TENTATIVE" | "DECLINED" => partstat_verb(&a.partstat).into(),
                    _ => "".into(),
                },
            })
            .collect();
        self.window().set_event_attendees(ModelRc::new(VecModel::from(items)));
        self.state.borrow_mut().form_attendees = attendees;
    }

    /// Adds the typed addresses ("Name <a@x>", several at once) to the form's attendees.
    /// False, with the form's error set, when one is not an address.
    pub(super) fn add_attendees(&self, text: &str) -> bool {
        let window = self.window();
        let me = {
            let state = self.state.borrow();
            let index = state.calendar_editing.as_ref().map_or(window.get_event_from_index().max(0) as usize, |s| s.account);
            state.accounts.get(index).map(|a| a.config.email.clone()).unwrap_or_default()
        };
        let parsed = mime::parse_address_list(text);
        if let Some((_, bad)) = parsed.iter().find(|(_, email)| !email.contains('@') || email.contains(char::is_whitespace)) {
            window.set_event_error(format!("not an email address: {bad}").into());
            return false;
        }
        let mut attendees = self.state.borrow().form_attendees.clone();
        for (name, email) in parsed {
            // The organizer is not their own attendee; nobody is invited twice.
            if email.eq_ignore_ascii_case(&me) || attendees.iter().any(|a| a.email.eq_ignore_ascii_case(&email)) {
                continue;
            }
            attendees.push(Attendee { email, name, partstat: "NEEDS-ACTION".into() });
        }
        window.set_event_error("".into());
        self.set_form_attendees(attendees);
        true
    }

    pub(super) fn remove_attendee(&self, index: usize) {
        let mut attendees = self.state.borrow().form_attendees.clone();
        if index < attendees.len() {
            attendees.remove(index);
            self.set_form_attendees(attendees);
        }
    }

    pub(super) fn delete_event(&self) {
        let Some(shown) = self.state.borrow_mut().calendar_open.take() else { return };
        self.send(shown.account, Command::DeleteEvent { target: shown.stored });
        let window = self.window();
        window.set_calendar_showing_event(false);
        window.set_calendar_confirm_delete(false);
        self.set_status("Deleting the event…");
    }

    /// The form as an event of the chosen account, with the user as organizer if anyone is invited.
    fn event_from_form(&self, form: &EventForm, account: &AccountConfig) -> Result<Event> {
        let title = form.title.trim();
        if title.is_empty() {
            return Err(anyhow!("enter a title"));
        }
        let date = NaiveDate::parse_from_str(form.date.trim(), "%Y-%m-%d").map_err(|_| anyhow!("enter the date as YYYY-MM-DD"))?;
        let (start, end) = if form.all_day {
            let midnight = date.and_time(NaiveTime::MIN).and_utc().timestamp();
            (midnight, midnight + 86400)
        } else {
            let time = |s: &SharedString, what: &str| NaiveTime::parse_from_str(s.trim(), "%H:%M").map_err(|_| anyhow!("enter the {what} time as HH:MM"));
            let at = |t: NaiveTime| Local.from_local_datetime(&date.and_time(t)).earliest().map(|d| d.timestamp()).ok_or_else(|| anyhow!("that time does not exist here"));
            let (start, end) = (at(time(&form.start, "start")?)?, at(time(&form.end, "end")?)?);
            if end <= start {
                return Err(anyhow!("the event must end after it starts"));
            }
            (start, end)
        };
        let attendees = self.state.borrow().form_attendees.clone();
        Ok(Event {
            uid: mime::new_message_id(&account.email).trim_matches(['<', '>']).to_string(),
            summary: title.to_string(),
            location: form.location.trim().to_string(),
            description: form.notes.trim().to_string(),
            start,
            end,
            all_day: form.all_day,
            organizer: (!attendees.is_empty()).then(|| Attendee { email: account.email.clone(), name: account.name.clone(), partstat: String::new() }),
            attendees,
            status: "CONFIRMED".into(),
            ..Default::default()
        })
    }

    pub(super) fn save_event(&self) {
        let window = self.window();
        let editing = self.state.borrow().calendar_editing.clone();
        let index = editing.as_ref().map_or(window.get_event_from_index().max(0) as usize, |s| s.account);
        let Some(account) = self.state.borrow().accounts.get(index).map(|a| a.config.clone()) else { return };
        let mut event = match self.event_from_form(&window.get_event_form(), &account) {
            Ok(e) => e,
            Err(e) => return window.set_event_error(format!("{e:#}").into()),
        };
        window.set_calendar_editing(false);
        window.set_calendar_showing_event(false);
        if let Some(target) = editing {
            let old = &target.stored.event;
            event.uid = old.uid.clone();
            event.status = String::new();
            // shortcut: the form edits one day; a multi-day all-day event keeps its length, a multi-day timed one is cut to the form's day.
            if event.all_day && old.all_day {
                event.end = event.start + (old.end - old.start).max(86400);
            }
            self.state.borrow_mut().calendar_editing = None;
            self.state.borrow_mut().calendar_open = Some(target.clone());
            // Back to its details, which update once the change is saved.
            window.set_calendar_showing_event(true);
            self.send(index, Command::UpdateEvent { target: target.stored, new: event });
            return self.set_status("Saving the event…");
        }
        // Microsoft's servers mail the invitations for events created through Graph; for the
        // rest the invitation goes out by mail like Gmail's and Outlook's do.
        if !event.attendees.is_empty() && account.auth != Auth::Microsoft {
            let draft = Draft {
                to: event.attendees.iter().map(|a| address(&a.name, &a.email)).collect::<Vec<_>>().join(", "),
                subject: format!("Invitation: {}", event.summary),
                body: invitation_text(&event),
                message_id: mime::new_message_id(&account.email),
                calendar: Some(("REQUEST".into(), ical::write("REQUEST", std::slice::from_ref(&event)))),
                ..Default::default()
            };
            let id = self.next_send_id.replace(self.next_send_id.get() + 1);
            self.send(index, Command::Send { id, draft, replied_to: None });
        }
        self.send(index, Command::SaveEvent { ics: ical::write("", std::slice::from_ref(&event)), invitation: false });
        self.set_calendar_day(days_of(&event).0);
    }

    // ---- invitations in the reader ----

    /// Shows the invitation card for the open message's calendar part, if it has one.
    pub(super) fn show_invite(&self, account: usize, body: &Body) {
        let window = self.window();
        let event = body.calendar.as_deref().map(ical::parse).and_then(|cal| Some((cal.method.clone(), cal.events.into_iter().next()?)));
        let Some((method, e)) = event else {
            window.set_invite_kind("".into());
            return;
        };
        let me = self.state.borrow().accounts.get(account).map(|a| a.config.email.clone()).unwrap_or_default();
        let organizer = e.organizer.as_ref().map(|o| format::person(&o.name, &o.email)).unwrap_or_default();
        let answered = self.state.borrow().invite_answers.get(&e.uid).cloned();
        let (kind, detail) = match method.as_str() {
            "REQUEST" if e.organizer.as_ref().is_some_and(|o| o.email.eq_ignore_ascii_case(&me)) => ("PUBLISH", "You organize this event.".to_string()),
            "REQUEST" => {
                let others = e.attendees.iter().filter(|a| !a.email.eq_ignore_ascii_case(&me)).count();
                let mut detail = format!("Organizer: {organizer}");
                if others > 0 {
                    detail.push_str(&format!(" · {others} other{} invited", if others == 1 { "" } else { "s" }));
                }
                if let Some(answer) = answered {
                    detail.push_str(&format!("\nYou answered: {}", partstat_verb(&answer)));
                }
                ("REQUEST", detail)
            }
            "REPLY" => {
                let lines: Vec<String> = e.attendees.iter().map(|a| format!("{}: {}", format::person(&a.name, &a.email), partstat_verb(&a.partstat))).collect();
                ("REPLY", lines.join("\n"))
            }
            "CANCEL" => ("CANCEL", format!("{organizer} cancelled this event.")),
            _ if organizer.is_empty() => ("PUBLISH", String::new()),
            _ => ("PUBLISH", format!("Organizer: {organizer}")),
        };
        window.set_invite_kind(kind.into());
        window.set_invite_title(if e.summary.is_empty() { "(no title)".into() } else { e.summary.clone().into() });
        window.set_invite_when(when(&e).into());
        window.set_invite_where(e.location.clone().into());
        window.set_invite_detail(detail.into());
    }

    pub(super) fn invite_respond(&self, answer: &str) {
        let Some((account, message)) = self.selected_summary() else { return };
        let Some(text) = self.state.borrow().body.as_ref().and_then(|b| b.calendar.clone()) else { return };
        let Some(config) = self.state.borrow().accounts.get(account).map(|a| a.config.clone()) else { return };
        let Some(e) = ical::parse(&text).events.into_iter().next() else { return };
        match answer {
            "ADD" => {
                self.send(account, Command::SaveEvent { ics: text, invitation: false });
                self.set_status("Added to the calendar");
            }
            "REMOVE" => {
                // Calendar servers process cancellations themselves; this clears copies kept on this device.
                let result = self.state.borrow_mut().accounts[account].store.remove_local_events(&e.uid);
                match result {
                    Ok(()) => self.set_status("Removed from the calendar"),
                    Err(e) => self.show_error(&e),
                }
                self.render_calendar();
            }
            partstat => {
                let Some(organizer) = e.organizer.clone() else {
                    return self.show_error(&anyhow!("the invitation names no organizer to answer"));
                };
                let verb = partstat_verb(partstat);
                let draft = Draft {
                    to: address(&organizer.name, &organizer.email),
                    // The subject Outlook and Gmail use, so the organizer's client files it with the event.
                    subject: format!("{verb}: {}", e.summary),
                    body: format!("{} has {} the invitation: {}\n{}\n", format::person(&config.name, &config.email), verb.to_lowercase(), e.summary, when(&e)),
                    in_reply_to: message.message_id.clone(),
                    references: message.message_id.clone(),
                    message_id: mime::new_message_id(&config.email),
                    calendar: Some(("REPLY".into(), ical::reply(&e, &config.email, partstat))),
                    ..Default::default()
                };
                let id = self.next_send_id.replace(self.next_send_id.get() + 1);
                self.send(account, Command::Send { id, draft, replied_to: None });
                if partstat != "DECLINED" {
                    self.send(account, Command::SaveEvent { ics: text, invitation: true });
                }
                self.state.borrow_mut().invite_answers.insert(e.uid.clone(), partstat.to_string());
                if let Some(body) = self.state.borrow().body.clone() {
                    self.show_invite(account, &body);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grid_starts_on_monday_before_the_first() {
        let day = NaiveDate::from_ymd_opt(2026, 10, 11).unwrap();
        // 1 October 2026 is a Thursday.
        assert_eq!(grid_start(day), NaiveDate::from_ymd_opt(2026, 9, 28).unwrap());
        assert_eq!(monday(day), NaiveDate::from_ymd_opt(2026, 10, 5).unwrap());
    }

    #[test]
    fn all_day_events_keep_their_dates() {
        let start = NaiveDate::from_ymd_opt(2026, 12, 24).unwrap().and_time(NaiveTime::MIN).and_utc().timestamp();
        let e = Event { start, end: start + 3 * 86400, all_day: true, ..Default::default() };
        let (first, last) = days_of(&e);
        assert_eq!((first.day(), last.day()), (24, 26));
        assert_eq!(when(&e), "Thu, 24 Dec – Sat, 26 Dec 2026");
    }

    #[test]
    fn overlapping_events_share_their_column() {
        // 9-10 and 9:30-11 overlap; 10:30-11:30 fits under the first; 12-13 stands alone.
        let h = |x: f64| (x * 3600.0) as i64;
        let spans = [(h(9.0), h(10.0)), (h(9.5), h(11.0)), (h(10.5), h(11.5)), (h(12.0), h(13.0))];
        assert_eq!(lanes(&spans), [(0, 2), (1, 2), (0, 2), (0, 1)]);
        assert!(lanes(&[]).is_empty());
    }

    #[test]
    fn quotes_names_with_commas() {
        assert_eq!(address("Doe, Jane", "jane@example.com"), "\"Doe, Jane\" <jane@example.com>");
        assert_eq!(mime::parse_address_list(&address("Doe, Jane", "jane@example.com")), [("Doe, Jane".to_string(), "jane@example.com".to_string())]);
    }
}
