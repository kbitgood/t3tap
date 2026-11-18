//! Turns events into output lines.

use std::sync::OnceLock;

use chrono::{DateTime, Local, TimeZone, Utc};

use crate::watcher::{Kind, WatchEvent};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    Text,
    Line,
    Jsonl,
}

#[derive(Clone, Copy, Debug)]
pub struct FormatOptions {
    pub format: Format,
    pub color: bool,
    pub utc: bool,
}

fn label(k: Kind) -> &'static str {
    match k {
        Kind::User => "USER",
        Kind::Final => "FINAL",
        Kind::Interim => "INTERIM",
        Kind::Tool => "TOOL",
        Kind::Thinking => "THINKING",
        Kind::Event => "EVENT",
        Kind::Turn => "TURN END",
    }
}

fn color(k: Kind) -> &'static str {
    match k {
        Kind::User => "\x1b[1;36m",
        Kind::Final => "\x1b[1;32m",
        Kind::Interim => "\x1b[32m",
        Kind::Tool => "\x1b[33m",
        Kind::Thinking => "\x1b[2;35m",
        Kind::Event => "\x1b[1;31m",
        Kind::Turn => "\x1b[2m",
    }
}
const DIM: &str = "\x1b[2m";
const RESET: &str = "\x1b[0m";

fn zone() -> Option<&'static str> {
    static ZONE: OnceLock<Option<String>> = OnceLock::new();
    ZONE.get_or_init(|| {
        std::env::var("TZ")
            .ok()
            .map(|z| z.trim_start_matches(':').to_string())
            .filter(|z| !z.is_empty())
            .or_else(|| iana_time_zone::get_timezone().ok())
    })
    .as_deref()
}

/// Short zone label. British Columbia stays on UTC-7 all year and calls it PDT.
fn zone_label(d: DateTime<Utc>) -> String {
    match zone() {
        Some("America/Vancouver") => "PDT".to_string(),
        Some(name) => match name.parse::<chrono_tz::Tz>() {
            Ok(tz) => tz.from_utc_datetime(&d.naive_utc()).format("%Z").to_string(),
            Err(_) => d.with_timezone(&Local).format("%:z").to_string(),
        },
        None => d.with_timezone(&Local).format("%:z").to_string(),
    }
}

/// Parse an ISO 8601 / RFC 3339 time as T3 writes it.
pub fn parse_iso(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s).ok().map(|d| d.with_timezone(&Utc))
}

/// Clock time of an event, such as "10:15:41 PDT". The offset comes from the system;
/// only the label comes from the zone name.
pub fn format_time(iso: &str, utc: bool) -> String {
    let Some(d) = parse_iso(iso) else {
        return iso.to_string();
    };
    if utc {
        return format!("{} UTC", d.format("%H:%M:%S"));
    }
    let hms = d.with_timezone(&Local).format("%H:%M:%S");
    format!("{hms} {}", zone_label(d)).trim().to_string()
}

pub fn format_event(e: &WatchEvent, o: FormatOptions) -> String {
    if o.format == Format::Jsonl {
        let mut s = serde_json::to_string(e).expect("events always serialize");
        s.push('\n');
        return s;
    }
    let time = format_time(&e.at, o.utc);
    let run = e.run.map(|r| format!(" run {r}")).unwrap_or_default();
    let extra = match (e.kind, &e.intent, &e.item_type) {
        (Kind::User, Some(i), _) if !i.is_empty() && i != "turn_start" => format!(" ({i})"),
        (Kind::Event, _, Some(t)) if !t.is_empty() => format!(" {t}"),
        _ => String::new(),
    };
    let (c, d, r) = if o.color { (color(e.kind), DIM, RESET) } else { ("", "", "") };
    let header = format!("{c}[{time}] {}{r}{d}{run}{extra}{r}", label(e.kind));

    if o.format == Format::Line {
        let flat = e.text.replace("\r\n", "\\n").replace('\n', "\\n");
        return format!("{header}: {flat}\n");
    }
    // text: one-line kinds stay on the header line, the rest go below it, indented.
    if matches!(e.kind, Kind::Tool | Kind::Turn) || (e.kind == Kind::Event && !e.text.contains('\n')) {
        return format!("{header}: {}\n", e.text);
    }
    let body = e
        .text
        .split('\n')
        .map(|l| format!("  {}", l.strip_suffix('\r').unwrap_or(l)))
        .collect::<Vec<_>>()
        .join("\n");
    format!("{header}\n{body}\n\n")
}

/// Cut `text` to `max` characters. 0 means no limit.
pub fn truncate(text: &str, max: usize) -> String {
    if max == 0 {
        return text.to_string();
    }
    let total = text.chars().count();
    if total <= max {
        return text.to_string();
    }
    let kept: String = text.chars().take(max).collect();
    format!("{kept}… [+{} chars]", total - max)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncates_by_characters() {
        assert_eq!(truncate("hello", 0), "hello");
        assert_eq!(truncate("hello", 5), "hello");
        assert_eq!(truncate("héllo world", 5), "héllo… [+6 chars]");
    }

    #[test]
    fn formats_utc_time() {
        assert_eq!(format_time("2026-11-18T17:08:21.123Z", true), "17:08:21 UTC");
        assert_eq!(format_time("not a time", true), "not a time");
    }
}
