//! Command line: options, filters, help and the watch loop.

use std::collections::BTreeSet;
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::{DateTime, Local, NaiveDate, NaiveDateTime, TimeZone, Utc};
use regex::{Regex, RegexBuilder};

use crate::format::{format_event, truncate, Format, FormatOptions};
use crate::watcher::{open_db, resolve_thread, Kind, ThreadWatcher, Tool, WatchEvent, ALL_KINDS, DEFAULT_KINDS};
use crate::where_expr::WhereExpr;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

pub fn main_help() -> String {
    include_str!("help/main.txt").replace("{VERSION}", VERSION)
}

pub const WATCH_HELP: &str = include_str!("help/watch.txt");

#[derive(Debug)]
pub struct UsageError(pub String);

impl std::fmt::Display for UsageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for UsageError {}

fn usage<T>(msg: impl Into<String>) -> Result<T, UsageError> {
    Err(UsageError(msg.into()))
}

#[derive(Clone, Debug, PartialEq)]
pub enum Start {
    Now,
    Last(usize),
    Since(String),
    All,
}

pub struct WatchOptions {
    pub thread: Option<String>,
    pub kinds: BTreeSet<Kind>,
    pub tool_name: Option<Regex>,
    pub match_re: Option<Regex>,
    pub exclude: Option<Regex>,
    pub where_expr: Option<WhereExpr>,
    pub thinking_chars: usize,
    pub tool_chars: usize,
    pub max_chars: usize,
    pub start: Start,
    pub follow: bool,
    pub exit_after_turn: bool,
    pub format: Format,
    pub utc: bool,
    pub color: bool,
    pub quiet: bool,
    pub interval: u64,
    pub db: PathBuf,
    pub allow_self: bool,
}

pub enum Parsed {
    Help,
    Watch(Box<WatchOptions>),
}

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

fn default_db() -> PathBuf {
    if let Some(p) = env("T3TAP_DB") {
        return PathBuf::from(p);
    }
    let home = env("T3CODE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env("HOME").unwrap_or_default()).join(".t3"));
    home.join("userdata").join("statev2.sqlite")
}

fn int_arg(name: &str, v: &str) -> Result<usize, UsageError> {
    if v.is_empty() || !v.bytes().all(|b| b.is_ascii_digit()) {
        return usage(format!("{name} needs a whole number, got \"{v}\"."));
    }
    v.parse().or_else(|_| usage(format!("{name}: {v} is too large.")))
}

fn regex_arg(name: &str, v: &str) -> Result<Regex, UsageError> {
    if v.is_empty() {
        return usage(format!("{name} needs a regular expression."));
    }
    RegexBuilder::new(v)
        .case_insensitive(true)
        .build()
        .or_else(|e| usage(format!("{name}: invalid regular expression: {e}")))
}

fn iso(d: DateTime<Utc>) -> String {
    d.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string()
}

/// Read `--since`: an age such as 10m, or an ISO 8601 time. Returns ISO 8601 in UTC.
pub fn parse_since(v: &str) -> Result<String, UsageError> {
    if v.is_empty() {
        return usage("--since needs a time, such as 10m or 2026-11-18T17:08:21Z.");
    }
    if let Some(unit) = v.chars().last().filter(|c| "smhd".contains(*c)) {
        let digits = &v[..v.len() - 1];
        if !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()) {
            let n: i64 = digits.parse().or_else(|_| usage(format!("--since: {v} is too large.")))?;
            let secs = match unit {
                's' => n,
                'm' => n.saturating_mul(60),
                'h' => n.saturating_mul(3600),
                _ => n.saturating_mul(86400),
            };
            let at = Utc::now()
                .checked_sub_signed(chrono::Duration::try_seconds(secs).unwrap_or(chrono::Duration::MAX))
                .unwrap_or(DateTime::<Utc>::MIN_UTC);
            return Ok(iso(at));
        }
    }
    if let Ok(d) = DateTime::parse_from_rfc3339(v) {
        return Ok(iso(d.with_timezone(&Utc)));
    }
    // Like JavaScript's Date: a date and time without a zone is local time,
    // a date alone is midnight UTC.
    for f in ["%Y-%m-%dT%H:%M:%S%.f", "%Y-%m-%dT%H:%M", "%Y-%m-%d %H:%M:%S%.f", "%Y-%m-%d %H:%M"] {
        if let Ok(n) = NaiveDateTime::parse_from_str(v, f) {
            if let Some(d) = Local.from_local_datetime(&n).earliest() {
                return Ok(iso(d.with_timezone(&Utc)));
            }
        }
    }
    if let Ok(d) = NaiveDate::parse_from_str(v, "%Y-%m-%d") {
        return Ok(iso(d.and_hms_opt(0, 0, 0).unwrap().and_utc()));
    }
    usage(format!("--since: cannot read \"{v}\" as a time."))
}

pub fn parse_watch_args(argv: &[String]) -> Result<Parsed, UsageError> {
    let mut o = WatchOptions {
        thread: None,
        kinds: BTreeSet::new(),
        tool_name: None,
        match_re: None,
        exclude: None,
        where_expr: None,
        thinking_chars: 600,
        tool_chars: 300,
        max_chars: 0,
        start: Start::Now,
        follow: true,
        exit_after_turn: false,
        format: Format::Text,
        utc: false,
        color: io::stdout().is_terminal() && env("NO_COLOR").is_none(),
        quiet: false,
        interval: 500,
        db: default_db(),
        allow_self: false,
    };
    let mut explicit_kinds: Option<Vec<Kind>> = None;
    let mut added: BTreeSet<Kind> = BTreeSet::new();
    let mut i = 0;
    while i < argv.len() {
        let mut a = argv[i].as_str();
        let mut val: Option<&str> = None;
        if a.starts_with("--") {
            if let Some(eq) = a.find('=') {
                val = Some(&a[eq + 1..]);
                a = &a[..eq];
            }
        }
        let mut next = || -> Result<String, UsageError> {
            if let Some(v) = val {
                return Ok(v.to_string());
            }
            i += 1;
            match argv.get(i) {
                Some(v) => Ok(v.clone()),
                None => usage(format!("{a} needs a value.")),
            }
        };
        match a {
            "-h" | "--help" => return Ok(Parsed::Help),
            "--tools" => {
                added.insert(Kind::Tool);
            }
            "--thinking" => {
                added.insert(Kind::Thinking);
            }
            "--interim" => {
                added.insert(Kind::Interim);
            }
            "--events" => {
                added.insert(Kind::Event);
            }
            "--turns" => {
                added.insert(Kind::Turn);
            }
            "--all" => added.extend(ALL_KINDS),
            "--kinds" => {
                let raw = next()?;
                let mut list = Vec::new();
                for k in raw.split(',').map(str::trim).filter(|s| !s.is_empty()) {
                    match Kind::parse(k) {
                        Some(kind) => list.push(kind),
                        None => {
                            let all = ALL_KINDS.map(Kind::name).join(", ");
                            return usage(format!("--kinds: unknown kind \"{k}\". Use: {all}."));
                        }
                    }
                }
                explicit_kinds = Some(list);
            }
            "--tool-name" => o.tool_name = Some(regex_arg(a, &next()?)?),
            "--match" => o.match_re = Some(regex_arg(a, &next()?)?),
            "--exclude" => o.exclude = Some(regex_arg(a, &next()?)?),
            "--where" => {
                let src = next()?;
                o.where_expr = Some(WhereExpr::compile(&src).or_else(|e| usage(format!("--where: {e}")))?);
            }
            "--thinking-chars" => o.thinking_chars = int_arg(a, &next()?)?,
            "--tool-chars" => o.tool_chars = int_arg(a, &next()?)?,
            "--max-chars" => o.max_chars = int_arg(a, &next()?)?,
            "--last" => o.start = Start::Last(int_arg(a, &next()?)?),
            "--since" => o.start = Start::Since(parse_since(&next()?)?),
            "--from-start" => o.start = Start::All,
            "--no-follow" => o.follow = false,
            "--follow" => o.follow = true,
            "--exit-after-turn" => o.exit_after_turn = true,
            "--format" => {
                o.format = match next()?.as_str() {
                    "text" => Format::Text,
                    "line" => Format::Line,
                    "jsonl" => Format::Jsonl,
                    f => return usage(format!("--format must be text, line or jsonl, got \"{f}\".")),
                }
            }
            "--utc" => o.utc = true,
            "--color" => o.color = true,
            "--no-color" => o.color = false,
            "--quiet" | "-q" => o.quiet = true,
            "--interval" => o.interval = int_arg(a, &next()?)?.max(50) as u64,
            "--db" => o.db = PathBuf::from(next()?),
            "--allow-self" => o.allow_self = true,
            _ => {
                if a.starts_with('-') {
                    return usage(format!("Unknown option \"{a}\"."));
                }
                if let Some(t) = &o.thread {
                    return usage(format!("Only one thread can be watched; got \"{t}\" and \"{a}\"."));
                }
                o.thread = Some(a.to_string());
            }
        }
        i += 1;
    }
    o.kinds = explicit_kinds.unwrap_or_else(|| DEFAULT_KINDS.to_vec()).into_iter().chain(added).collect();
    if o.format == Format::Jsonl {
        o.color = false;
    }
    Ok(Parsed::Watch(Box::new(o)))
}

/// Applies the kinds, filters and length limits. Returns the event to print, or None.
struct Filter<'a> {
    o: &'a WatchOptions,
    where_failed: bool,
}

impl Filter<'_> {
    fn apply(&mut self, e: &WatchEvent) -> Option<WatchEvent> {
        let o = self.o;
        if !o.kinds.contains(&e.kind) {
            return None;
        }
        if let Some(re) = &o.tool_name {
            if e.kind == Kind::Tool && !re.is_match(e.tool.as_ref().map_or("", |t| t.name.as_str())) {
                return None;
            }
        }
        if o.match_re.as_ref().is_some_and(|re| !re.is_match(&e.text)) {
            return None;
        }
        if o.exclude.as_ref().is_some_and(|re| re.is_match(&e.text)) {
            return None;
        }
        if let Some(w) = &o.where_expr {
            match w.test(e) {
                Ok(true) => {}
                Ok(false) => return None,
                Err(msg) => {
                    if !self.where_failed {
                        self.where_failed = true;
                        eprintln!("t3tap: --where failed on an event (treated as false): {msg}");
                    }
                    return None;
                }
            }
        }
        let mut out = e.clone();
        if e.kind == Kind::Thinking {
            out.text = truncate(&out.text, o.thinking_chars);
        }
        if let (Kind::Tool, Some(t)) = (e.kind, &e.tool) {
            let tool = Tool { name: t.name.clone(), input: truncate(&t.input, o.tool_chars) };
            out.text = format!("{}: {}", tool.name, tool.input);
            out.tool = Some(tool);
        }
        out.text = truncate(&out.text, o.max_chars);
        Some(out)
    }
}

/// Writes one event to stdout. A closed pipe (`t3tap ... | head`) ends the program quietly.
fn write_out(s: &str) {
    let mut out = io::stdout().lock();
    if let Err(e) = out.write_all(s.as_bytes()).and_then(|_| out.flush()) {
        if e.kind() == io::ErrorKind::BrokenPipe {
            std::process::exit(0);
        }
        eprintln!("t3tap: cannot write output: {e}");
        std::process::exit(1);
    }
}

fn watch(argv: &[String]) -> Result<i32, UsageError> {
    let o = match parse_watch_args(argv)? {
        Parsed::Help => {
            write_out(WATCH_HELP);
            return Ok(0);
        }
        Parsed::Watch(o) => o,
    };
    let Some(arg) = o.thread.clone() else {
        return usage("Missing <thread>. Example: t3tap watch a1b2c3d4");
    };
    let self_id = env("T3CODE_THREAD_ID");
    let arg = if arg == "self" {
        match &self_id {
            Some(id) => id.clone(),
            None => return usage("\"self\" works only inside a T3 thread ($T3CODE_THREAD_ID is not set)."),
        }
    } else {
        arg
    };
    let db_path = o.db.to_string_lossy().to_string();
    if !Path::new(&o.db).exists() {
        eprintln!("t3tap: no T3 database at {db_path}. Pass --db or set T3TAP_DB.");
        return Ok(1);
    }
    let db = match open_db(&db_path) {
        Ok(db) => db,
        Err(e) => {
            eprintln!("t3tap: cannot open {db_path}: {e}");
            return Ok(1);
        }
    };
    let matches = match resolve_thread(&db, &arg) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("t3tap: database error: {e}");
            return Ok(1);
        }
    };
    if matches.is_empty() {
        eprintln!("t3tap: no thread matches \"{arg}\".");
        return Ok(3);
    }
    if matches.len() > 1 {
        eprintln!("t3tap: \"{arg}\" matches {} threads; use more of the ID:", matches.len());
        for m in &matches {
            eprintln!("  {}  {}", m.thread_id, m.title);
        }
        return Ok(3);
    }
    let thread = &matches[0];
    if o.follow && !o.allow_self && self_id.as_deref() == Some(thread.thread_id.as_str()) {
        eprintln!(
            "t3tap: refusing to follow the thread this command runs in. An agent that reacts to its own output loops.\n\
             Use --no-follow to read it once, or --allow-self if you are sure."
        );
        return Ok(2);
    }
    if !o.quiet {
        let provider = thread.provider.as_ref().map(|p| format!(" on {p}")).unwrap_or_default();
        let deleted = if thread.deleted { " [deleted]" } else { "" };
        let kinds = o.kinds.iter().map(|k| k.name()).collect::<Vec<_>>().join(", ");
        eprintln!(
            "t3tap: watching \"{}\" ({}){provider}{deleted}; kinds: {kinds}",
            thread.title, thread.thread_id
        );
    }

    let mut watcher = ThreadWatcher::new(&db, thread.thread_id.clone());
    let mut filter = Filter { o: &o, where_failed: false };
    let fmt = FormatOptions { format: o.format, color: o.color, utc: o.utc };

    let stopping = Arc::new(AtomicBool::new(false));
    for sig in [signal_hook::consts::SIGINT, signal_hook::consts::SIGTERM] {
        // If registration fails the default handler stays, which also stops the program.
        let _ = signal_hook::flag::register(sig, Arc::clone(&stopping));
    }

    // First pass: history.
    let history = match watcher.poll(o.start == Start::Now) {
        Ok(h) => h,
        Err(e) => {
            eprintln!("t3tap: database error: {e}");
            return Ok(1);
        }
    };
    if o.start != Start::Now {
        let mut shown: Vec<WatchEvent> = history.iter().filter_map(|e| filter.apply(e)).collect();
        match &o.start {
            Start::Last(n) => {
                let skip = shown.len().saturating_sub(*n);
                shown.drain(..skip);
            }
            Start::Since(at) => shown.retain(|e| e.at.as_str() > at.as_str()),
            _ => {}
        }
        for e in &shown {
            write_out(&format_event(e, fmt));
        }
    }
    if !o.follow {
        return Ok(0);
    }

    let interval = Duration::from_millis(o.interval);
    while !stopping.load(Ordering::Relaxed) {
        // Sleep in short steps so Ctrl+C stops the program quickly.
        let until = Instant::now() + interval;
        while Instant::now() < until && !stopping.load(Ordering::Relaxed) {
            std::thread::sleep((until - Instant::now()).min(Duration::from_millis(50)));
        }
        if stopping.load(Ordering::Relaxed) {
            break;
        }
        let events = match watcher.poll(false) {
            Ok(evs) => evs,
            Err(e) => {
                eprintln!("t3tap: database error: {e}");
                return Ok(1);
            }
        };
        let mut turn_ended = false;
        for e in &events {
            if e.kind == Kind::Turn {
                turn_ended = true;
            }
            if let Some(f) = filter.apply(e) {
                write_out(&format_event(&f, fmt));
            }
        }
        if o.exit_after_turn && turn_ended {
            return Ok(0);
        }
    }
    Ok(0)
}

/// Runs t3tap with the arguments after the program name. Returns the exit code.
pub fn run(argv: &[String]) -> i32 {
    let Some(cmd) = argv.first().map(String::as_str) else {
        write_out(&main_help());
        return 2;
    };
    let rest = &argv[1..];
    match cmd {
        "-h" | "--help" => {
            write_out(&main_help());
            0
        }
        "-v" | "--version" => {
            write_out(&format!("{VERSION}\n"));
            0
        }
        "help" => match rest.first().map(String::as_str) {
            None => {
                write_out(&main_help());
                0
            }
            Some("watch") => {
                write_out(WATCH_HELP);
                0
            }
            Some(topic) => {
                eprintln!("t3tap: no command \"{topic}\". Commands: watch.");
                2
            }
        },
        "watch" => match watch(rest) {
            Ok(code) => code,
            Err(UsageError(msg)) => {
                eprintln!("t3tap: {msg}\nRun \"t3tap watch --help\" for usage.");
                2
            }
        },
        _ => {
            eprintln!("t3tap: unknown command \"{cmd}\". Run \"t3tap --help\".");
            2
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn since_accepts_ages_and_iso() {
        assert_eq!(parse_since("2026-11-18T17:08:21Z").unwrap(), "2026-11-18T17:08:21.000Z");
        assert_eq!(parse_since("2026-11-18T10:08:21-07:00").unwrap(), "2026-11-18T17:08:21.000Z");
        assert_eq!(parse_since("2026-11-18").unwrap(), "2026-11-18T00:00:00.000Z");
        assert!(parse_since("10m").unwrap() < iso(Utc::now()));
        assert!(parse_since("yesterday").is_err());
    }
}
