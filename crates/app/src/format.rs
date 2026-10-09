use chrono::{DateTime, Datelike, Local};

fn local(ts: i64) -> Option<DateTime<Local>> {
    if ts == 0 {
        return None; // the message had no usable date
    }
    DateTime::from_timestamp(ts, 0).map(|d| d.with_timezone(&Local))
}

/// Compact date for the message list: time today, day this year, full date otherwise.
pub fn list_date(ts: i64) -> String {
    let Some(date) = local(ts) else { return String::new() };
    let now = Local::now();
    if date.date_naive() == now.date_naive() {
        date.format("%H:%M").to_string()
    } else if date.year() == now.year() {
        date.format("%b %-d").to_string()
    } else {
        date.format("%Y-%m-%d").to_string()
    }
}

pub fn full_date(ts: i64) -> String {
    local(ts).map(|d| d.format("%a, %-d %b %Y, %H:%M").to_string()).unwrap_or_default()
}

pub fn size(bytes: usize) -> String {
    match bytes {
        b if b < 1024 => format!("{b} B"),
        b if b < 1024 * 1024 => format!("{:.0} KB", b as f64 / 1024.0),
        b => format!("{:.1} MB", b as f64 / (1024.0 * 1024.0)),
    }
}

pub fn person(name: &str, addr: &str) -> String {
    match (name.is_empty(), addr.is_empty()) {
        (false, false) => format!("{name} <{addr}>"),
        (false, true) => name.to_string(),
        _ => addr.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes() {
        assert_eq!(size(512), "512 B");
        assert_eq!(size(2048), "2 KB");
        assert_eq!(size(3 * 1024 * 1024 / 2), "1.5 MB");
    }

    #[test]
    fn list_date_today_shows_time() {
        let now = Local::now().timestamp();
        assert_eq!(list_date(now).len(), 5);
        assert_eq!(list_date(0), "");
        assert_eq!(list_date(86400 * 365).len(), 10);
    }
}
