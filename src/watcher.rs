//! Reads one T3 thread from T3's local SQLite database and turns its items into events.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use regex::Regex;
use rusqlite::{params, Connection, OpenFlags};
use serde::Serialize;
use serde_json::Value;

/// The kinds of event `t3tap watch` can print.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    User,
    Final,
    Interim,
    Tool,
    Thinking,
    Event,
    Turn,
}

impl Kind {
    pub fn name(self) -> &'static str {
        match self {
            Kind::User => "user",
            Kind::Final => "final",
            Kind::Interim => "interim",
            Kind::Tool => "tool",
            Kind::Thinking => "thinking",
            Kind::Event => "event",
            Kind::Turn => "turn",
        }
    }

    pub fn parse(s: &str) -> Option<Kind> {
        ALL_KINDS.iter().copied().find(|k| k.name() == s)
    }
}

pub const ALL_KINDS: [Kind; 7] = [
    Kind::User,
    Kind::Final,
    Kind::Interim,
    Kind::Tool,
    Kind::Thinking,
    Kind::Event,
    Kind::Turn,
];
pub const DEFAULT_KINDS: [Kind; 2] = [Kind::User, Kind::Final];

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Tool {
    pub name: String,
    pub input: String,
}

/// One thing that happened in a thread. Serializes to the `--format jsonl` object.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WatchEvent {
    pub kind: Kind,
    pub thread_id: String,
    pub run_id: Option<String>,
    /// Turn number within the thread (T3 calls a turn a "run").
    pub run: Option<i64>,
    pub item_id: Option<String>,
    /// Raw T3 item type, such as assistant_message or command_execution.
    pub item_type: Option<String>,
    pub status: Option<String>,
    /// ISO 8601 time in UTC.
    pub at: String,
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool: Option<Tool>,
    /// For user messages: turn_start, steer, queued_turn and so on.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub intent: Option<String>,
    /// Provider that produced the item, when T3 records it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// Sort key inside a poll. Not printed.
    #[serde(skip)]
    pub ordinal: i64,
}

const TERMINAL_RUN: [&str; 5] = ["completed", "cancelled", "interrupted", "failed", "rolled_back"];
const TERMINAL_ITEM: [&str; 4] = ["completed", "failed", "interrupted", "cancelled"];
pub const TOOL_TYPES: [&str; 6] = [
    "command_execution",
    "dynamic_tool",
    "file_change",
    "file_search",
    "web_search",
    "subagent",
];
/// Bookkeeping items that never become events.
const SKIP_TYPES: [&str; 1] = ["checkpoint"];

fn is_tool_type(t: &str) -> bool {
    TOOL_TYPES.contains(&t)
}

fn is_work_type(t: &str) -> bool {
    is_tool_type(t) || t == "assistant_message"
}

struct ItemRow {
    turn_item_id: String,
    run_id: Option<String>,
    ordinal: i64,
    item_type: String,
    status: String,
    updated_at: String,
    payload: String,
}

struct PendingMessage {
    item_id: String,
    ordinal: i64,
    text: String,
    at: String,
    status: String,
    provider: Option<String>,
    /// Seen before the watch started in "now" mode: print it only if it turns out to be final.
    pre_start: bool,
}

struct RunState {
    ordinal: Option<i64>,
    status: Option<String>,
    completed_at: Option<String>,
    /// Highest ordinal of any assistant message or tool call in the run, in any status.
    max_work: i64,
    pending: Vec<PendingMessage>,
    end_emitted: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ThreadInfo {
    pub thread_id: String,
    pub title: String,
    pub provider: Option<String>,
    pub deleted: bool,
}

pub fn open_db(path: &str) -> rusqlite::Result<Connection> {
    let db = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX | OpenFlags::SQLITE_OPEN_URI,
    )?;
    // T3 writes while we read. Wait briefly on a lock instead of failing.
    db.busy_timeout(Duration::from_millis(600))?;
    Ok(db)
}

const THREAD_COLUMNS: &str = "thread_id, title, provider_instance_id, default_provider, deleted_at";

fn thread_rows(db: &Connection, sql: &str, p: impl rusqlite::Params) -> rusqlite::Result<Vec<ThreadInfo>> {
    let mut stmt = db.prepare_cached(sql)?;
    let rows = stmt.query_map(p, |r| {
        let instance: Option<String> = r.get(2)?;
        let default: Option<String> = r.get(3)?;
        let deleted: Option<String> = r.get(4)?;
        Ok(ThreadInfo {
            thread_id: r.get(0)?,
            title: r.get::<_, Option<String>>(1)?.unwrap_or_default(),
            provider: instance.or(default),
            deleted: deleted.is_some(),
        })
    })?;
    rows.collect()
}

/// Resolve a full thread ID, a unique prefix, or text that contains a thread ID.
pub fn resolve_thread(db: &Connection, arg: &str) -> rusqlite::Result<Vec<ThreadInfo>> {
    let uuid_re = Regex::new(r"(?i)[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}").unwrap();
    let mut candidates = vec![arg.to_string()];
    if let Some(m) = uuid_re.find(arg) {
        if m.as_str() != arg {
            candidates.push(m.as_str().to_string());
        }
    }
    let exact_sql = format!("SELECT {THREAD_COLUMNS} FROM orchestration_v2_projection_threads WHERE thread_id = ?");
    for c in &candidates {
        let exact = thread_rows(db, &exact_sql, params![c])?;
        if !exact.is_empty() {
            return Ok(exact);
        }
    }
    let len = arg.chars().count();
    if len < 4 {
        return Ok(vec![]);
    }
    let prefix_sql = format!(
        "SELECT {THREAD_COLUMNS} FROM orchestration_v2_projection_threads
         WHERE substr(thread_id, 1, ?) = ? ORDER BY updated_at DESC LIMIT 10"
    );
    thread_rows(db, &prefix_sql, params![len as i64, arg])
}

fn parse_json(s: &str) -> Value {
    serde_json::from_str(s).unwrap_or_else(|_| Value::Object(Default::default()))
}

fn run_ordinal_from_id(run_id: Option<&str>) -> Option<i64> {
    let id = run_id?;
    let idx = id.rfind("ordinal:")?;
    let digits = &id[idx + "ordinal:".len()..];
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

/// JavaScript truthiness of a JSON value. A missing field is `Value::Null`.
fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0 && !f.is_nan()),
        Value::String(s) => !s.is_empty(),
        _ => true,
    }
}

/// A value as text: strings as they are, anything else as compact JSON, null as "".
pub fn str_of(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// `a ?? b`: the first value that is not null or missing.
fn coalesce<'a>(vals: &[&'a Value]) -> &'a Value {
    vals.iter().copied().find(|v| !v.is_null()).unwrap_or(&Value::Null)
}

fn opt_string(v: &Value) -> Option<String> {
    match v {
        Value::Null => None,
        Value::String(s) => Some(s.clone()),
        other => Some(other.to_string()),
    }
}

/// Join the truthy values as text, like `[a, b].filter(Boolean).map(str).join(sep)`.
fn join_truthy(vals: &[&Value], sep: &str) -> String {
    vals.iter().filter(|v| truthy(v)).map(|v| str_of(v)).collect::<Vec<_>>().join(sep)
}

fn describe_tool(item_type: &str, p: &Value) -> Tool {
    let (name, input) = match item_type {
        "command_execution" => ("shell".to_string(), str_of(coalesce(&[&p["input"], &p["command"]]))),
        "dynamic_tool" => {
            let n = str_of(&p["toolName"]);
            (if n.is_empty() { "tool".to_string() } else { n }, str_of(&p["input"]))
        }
        "file_change" => {
            let ops = match &p["changes"] {
                Value::Array(changes) => changes
                    .iter()
                    .map(|c| {
                        let op = match &c["operation"] {
                            Value::Null => "change".to_string(),
                            v => js_text(v),
                        };
                        let path = match &c["path"] {
                            Value::Null => String::new(),
                            v => js_text(v),
                        };
                        format!("{op} {path}").trim().to_string()
                    })
                    .collect::<Vec<_>>()
                    .join(", "),
                _ => String::new(),
            };
            let input = if ops.is_empty() { str_of(&p["fileName"]) } else { ops };
            ("file_change".to_string(), input)
        }
        "web_search" => ("web_search".to_string(), str_of(coalesce(&[&p["query"], &p["patterns"]]))),
        "file_search" => (
            "file_search".to_string(),
            str_of(coalesce(&[&p["query"], &p["patterns"], &p["input"]])),
        ),
        "subagent" => {
            let name = if truthy(&p["driver"]) {
                format!("subagent:{}", js_text(&p["driver"]))
            } else {
                "subagent".to_string()
            };
            (name, join_truthy(&[&p["title"], &p["prompt"]], ": "))
        }
        other => (other.to_string(), str_of(&p["input"])),
    };
    Tool { name, input }
}

/// A value as JavaScript would print it inside a template string.
fn js_text(v: &Value) -> String {
    match v {
        Value::Null => "undefined".to_string(),
        Value::String(s) => s.clone(),
        Value::Array(a) => a.iter().map(|x| if x.is_null() { String::new() } else { js_text(x) }).collect::<Vec<_>>().join(","),
        Value::Object(_) => "[object Object]".to_string(),
        other => other.to_string(),
    }
}

fn describe_other(item_type: &str, p: &Value) -> String {
    match item_type {
        "handoff" => {
            let model = if truthy(&p["toModel"]) { format!(" ({})", js_text(&p["toModel"])) } else { String::new() };
            format!(
                "Provider handoff {} -> {}{model}",
                str_of(&p["fromProviderInstanceIds"]),
                str_of(&p["toProviderInstanceId"])
            )
        }
        "compaction" => {
            if truthy(&p["beforeTokenCount"]) {
                format!(
                    "Context compacted {} -> {} tokens",
                    js_text(&p["beforeTokenCount"]),
                    js_text(&p["afterTokenCount"])
                )
            } else {
                "Context compacted".to_string()
            }
        }
        "error" => {
            let msg = coalesce(&[&p["failure"]["message"], &p["message"]]);
            join_truthy(&[&p["title"], msg], ": ")
        }
        "notification" => str_of(coalesce(&[&p["summary"], &p["title"], &p["outcome"]])),
        "approval_request" => format!("Approval requested ({}): {}", str_of(&p["requestKind"]), str_of(&p["prompt"])),
        "user_input_request" => {
            let qs = match &p["questions"] {
                Value::Array(a) => a.iter().map(|q| str_of(&q["question"])).collect::<Vec<_>>().join(" | "),
                _ => String::new(),
            };
            format!("Question for the user: {qs}")
        }
        "todo_list" => match &p["steps"] {
            Value::Array(steps) => steps
                .iter()
                .map(|s| format!("[{}] {}", js_text(&s["status"]), str_of(&s["text"])))
                .collect::<Vec<_>>()
                .join("\n"),
            _ => "Plan updated".to_string(),
        },
        "secret_request" => format!("Secret requested: {}", str_of(coalesce(&[&p["label"], &p["title"]]))),
        other => {
            let msg = coalesce(&[&p["message"], &p["summary"]]);
            let s = join_truthy(&[&p["title"], msg], ": ");
            if s.is_empty() {
                other.to_string()
            } else {
                s
            }
        }
    }
}

/// The current time as `new Date().toISOString()` prints it.
pub fn now_iso() -> String {
    chrono::Utc::now().format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string()
}

/// Follows one T3 thread by polling T3's local database read-only.
/// Call `poll()` repeatedly; each call returns the new events in thread order.
/// It returns every kind; filtering is the caller's job.
pub struct ThreadWatcher<'a> {
    db: &'a Connection,
    pub thread_id: String,
    emitted: HashSet<String>,
    recorded_messages: HashSet<String>,
    runs: HashMap<String, RunState>,
    cursor: String,
}

const ITEM_SQL: &str = "SELECT turn_item_id, run_id, ordinal, type, status, updated_at,
   CASE WHEN json_valid(payload_json)
        THEN json_remove(payload_json, '$.output', '$.diffStr', '$.results', '$.result')
        ELSE '{}' END AS payload
 FROM orchestration_v2_projection_turn_items
 WHERE thread_id = ? AND updated_at >= ?
 ORDER BY ordinal";

const RUN_SQL: &str = "SELECT run_id, ordinal, status, completed_at
 FROM orchestration_v2_projection_runs WHERE thread_id = ?";

impl<'a> ThreadWatcher<'a> {
    pub fn new(db: &'a Connection, thread_id: impl Into<String>) -> Self {
        ThreadWatcher {
            db,
            thread_id: thread_id.into(),
            emitted: HashSet::new(),
            recorded_messages: HashSet::new(),
            runs: HashMap::new(),
            cursor: String::new(),
        }
    }

    fn run(&mut self, run_id: &str) -> &mut RunState {
        self.runs.entry(run_id.to_string()).or_insert_with(|| RunState {
            ordinal: run_ordinal_from_id(Some(run_id)),
            status: None,
            completed_at: None,
            max_work: -1,
            pending: Vec::new(),
            end_emitted: false,
        })
    }

    /// `pre_start` is true for the first poll in "start from now" mode: every event it
    /// finds is history, but messages of turns still running stay pending so their final
    /// answer can still be reported.
    pub fn poll(&mut self, pre_start: bool) -> rusqlite::Result<Vec<WatchEvent>> {
        let mut out = Vec::new();
        let rows: Vec<ItemRow> = {
            let mut stmt = self.db.prepare_cached(ITEM_SQL)?;
            let rows = stmt.query_map(params![self.thread_id, self.cursor], |r| {
                Ok(ItemRow {
                    turn_item_id: r.get(0)?,
                    run_id: r.get(1)?,
                    ordinal: r.get(2)?,
                    item_type: r.get(3)?,
                    status: r.get(4)?,
                    updated_at: r.get(5)?,
                    payload: r.get::<_, Option<String>>(6)?.unwrap_or_else(|| "{}".into()),
                })
            })?;
            rows.collect::<rusqlite::Result<_>>()?
        };
        for row in &rows {
            if row.updated_at > self.cursor {
                self.cursor = row.updated_at.clone();
            }
            self.process(row, &mut out, pre_start);
        }
        let runs: Vec<(String, i64, Option<String>, Option<String>)> = {
            let mut stmt = self.db.prepare_cached(RUN_SQL)?;
            let rows = stmt.query_map(params![self.thread_id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?;
            rows.collect::<rusqlite::Result<_>>()?
        };
        for (run_id, ordinal, status, completed_at) in runs {
            let st = self.run(&run_id);
            st.ordinal = Some(ordinal);
            st.status = status;
            st.completed_at = completed_at;
        }
        self.classify(&mut out);
        out.sort_by(|a, b| a.run.unwrap_or(0).cmp(&b.run.unwrap_or(0)).then(a.ordinal.cmp(&b.ordinal)));
        Ok(if pre_start { Vec::new() } else { out })
    }

    fn base(&self, row: &ItemRow, p: &Value, kind: Kind, text: String) -> WatchEvent {
        let run = row.run_id.as_deref().and_then(|id| {
            self.runs.get(id).and_then(|r| r.ordinal).or_else(|| run_ordinal_from_id(Some(id)))
        });
        let at = opt_string(coalesce(&[&p["completedAt"], &p["updatedAt"]])).unwrap_or_else(|| row.updated_at.clone());
        WatchEvent {
            kind,
            thread_id: self.thread_id.clone(),
            run_id: row.run_id.clone(),
            run,
            item_id: Some(row.turn_item_id.clone()),
            item_type: Some(row.item_type.clone()),
            status: Some(row.status.clone()),
            at,
            text,
            tool: None,
            intent: None,
            provider: opt_string(coalesce(&[&p["nativeItemRef"]["driver"], &p["driver"]])),
            ordinal: row.ordinal,
        }
    }

    fn process(&mut self, row: &ItemRow, out: &mut Vec<WatchEvent>, pre_start: bool) {
        let t = row.item_type.as_str();
        let id = &row.turn_item_id;
        if SKIP_TYPES.contains(&t) {
            return;
        }
        let terminal = TERMINAL_ITEM.contains(&row.status.as_str());

        if let Some(run_id) = &row.run_id {
            if is_work_type(t) {
                let r = self.run(run_id);
                if row.ordinal > r.max_work {
                    r.max_work = row.ordinal;
                }
            }
        }

        if t == "assistant_message" {
            if !terminal || self.recorded_messages.contains(id) {
                return;
            }
            self.recorded_messages.insert(id.clone());
            let p = parse_json(&row.payload);
            let text = str_of(&p["text"]);
            let Some(run_id) = &row.run_id else {
                // No turn to judge it against: report it as a finished reply.
                if !pre_start {
                    out.push(self.base(row, &p, Kind::Final, text));
                }
                return;
            };
            let at = opt_string(&p["completedAt"]).unwrap_or_else(|| row.updated_at.clone());
            let provider = opt_string(&p["nativeItemRef"]["driver"]);
            self.run(run_id).pending.push(PendingMessage {
                item_id: id.clone(),
                ordinal: row.ordinal,
                text,
                at,
                status: row.status.clone(),
                provider,
                pre_start,
            });
            return;
        }

        if self.emitted.contains(id) {
            return;
        }
        let ev = if t == "user_message" {
            if !terminal {
                return;
            }
            let p = parse_json(&row.payload);
            let n = p["attachments"].as_array().map_or(0, |a| a.len());
            let mut text = str_of(&p["text"]);
            if n > 0 {
                text.push_str(&format!("\n[{n} attachment{}]", if n > 1 { "s" } else { "" }));
            }
            let mut ev = self.base(row, &p, Kind::User, text);
            ev.intent = opt_string(&p["inputIntent"]);
            ev
        } else if t == "reasoning" {
            if !terminal {
                return;
            }
            let p = parse_json(&row.payload);
            let text = str_of(&p["text"]);
            self.base(row, &p, Kind::Thinking, text)
        } else if is_tool_type(t) {
            if row.status == "pending" {
                return;
            }
            let p = parse_json(&row.payload);
            let tool = describe_tool(t, &p);
            let mut ev = self.base(row, &p, Kind::Tool, format!("{}: {}", tool.name, tool.input));
            ev.tool = Some(tool);
            ev
        } else {
            if !terminal && row.status != "waiting" {
                return;
            }
            let p = parse_json(&row.payload);
            let text = describe_other(t, &p);
            self.base(row, &p, Kind::Event, text)
        };
        self.emitted.insert(id.clone());
        if !pre_start {
            out.push(ev);
        }
    }

    /// Decide whether held assistant messages were interim or the final answer of their turn.
    fn classify(&mut self, out: &mut Vec<WatchEvent>) {
        for (run_id, r) in self.runs.iter_mut() {
            let ended = r.status.as_deref().is_some_and(|s| TERMINAL_RUN.contains(&s));
            if !r.pending.is_empty() {
                let mut keep = Vec::new();
                for m in std::mem::take(&mut r.pending) {
                    let kind = if r.max_work > m.ordinal {
                        Kind::Interim
                    } else if ended {
                        Kind::Final
                    } else {
                        keep.push(m);
                        continue;
                    };
                    if m.pre_start && kind == Kind::Interim {
                        continue;
                    }
                    out.push(WatchEvent {
                        kind,
                        thread_id: self.thread_id.clone(),
                        run_id: Some(run_id.clone()),
                        run: r.ordinal,
                        item_id: Some(m.item_id),
                        item_type: Some("assistant_message".into()),
                        status: Some(m.status),
                        at: m.at,
                        text: m.text,
                        tool: None,
                        intent: None,
                        provider: m.provider,
                        ordinal: m.ordinal,
                    });
                }
                r.pending = keep;
            }
            if ended && !r.end_emitted && r.pending.is_empty() {
                r.end_emitted = true;
                out.push(WatchEvent {
                    kind: Kind::Turn,
                    thread_id: self.thread_id.clone(),
                    run_id: Some(run_id.clone()),
                    run: r.ordinal,
                    item_id: None,
                    item_type: None,
                    status: r.status.clone(),
                    at: r.completed_at.clone().unwrap_or_else(now_iso),
                    text: r.status.clone().unwrap_or_else(|| "ended".into()),
                    tool: None,
                    intent: None,
                    provider: None,
                    ordinal: i64::MAX,
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_ordinal() {
        assert_eq!(run_ordinal_from_id(Some("run:thread:x:ordinal:12")), Some(12));
        assert_eq!(run_ordinal_from_id(Some("run:thread:x")), None);
        assert_eq!(run_ordinal_from_id(None), None);
    }

    #[test]
    fn describes_tools() {
        let p = serde_json::json!({"changes": [{"operation": "update", "path": "a.ts"}, {"path": "b.ts"}]});
        assert_eq!(describe_tool("file_change", &p).input, "update a.ts, change b.ts");
        let p = serde_json::json!({"driver": "codex", "title": "Fix", "prompt": "do it"});
        assert_eq!(describe_tool("subagent", &p), Tool { name: "subagent:codex".into(), input: "Fix: do it".into() });
        let p = serde_json::json!({"toolName": "Read", "input": {"b": 1, "a": 2}});
        assert_eq!(describe_tool("dynamic_tool", &p).input, r#"{"b":1,"a":2}"#);
    }

    #[test]
    fn describes_other_items() {
        let p = serde_json::json!({"title": "Boom", "failure": {"message": "bad"}});
        assert_eq!(describe_other("error", &p), "Boom: bad");
        assert_eq!(describe_other("mystery", &serde_json::json!({})), "mystery");
        assert_eq!(describe_other("compaction", &serde_json::json!({})), "Context compacted");
    }
}
