use rusqlite::{params, Connection};
use serde_json::{json, Value};
use t3tap::cli::{parse_watch_args, Parsed};
use t3tap::watcher::{resolve_thread, Kind, ThreadWatcher, WatchEvent};

const T: &str = "11111111-2222-3333-4444-555555555555";

/// An in-memory copy of the T3 tables t3tap reads, with a clock that ticks once per write.
struct Fixture {
    db: Connection,
    clock: std::cell::Cell<u32>,
}

impl Fixture {
    fn new() -> Self {
        let db = Connection::open_in_memory().unwrap();
        db.execute_batch(
            "CREATE TABLE orchestration_v2_projection_threads (thread_id TEXT PRIMARY KEY, title TEXT,
               provider_instance_id TEXT, default_provider TEXT, deleted_at TEXT, updated_at TEXT);
             CREATE TABLE orchestration_v2_projection_runs (run_id TEXT PRIMARY KEY, thread_id TEXT,
               ordinal INTEGER, status TEXT, completed_at TEXT);
             CREATE TABLE orchestration_v2_projection_turn_items (turn_item_id TEXT PRIMARY KEY,
               thread_id TEXT, run_id TEXT, ordinal INTEGER, type TEXT, status TEXT, updated_at TEXT,
               payload_json TEXT);",
        )
        .unwrap();
        db.execute(
            "INSERT INTO orchestration_v2_projection_threads VALUES (?, 'Main', 'claudeAgent', 'claudeAgent', NULL, '')",
            params![T],
        )
        .unwrap();
        Fixture { db, clock: Default::default() }
    }

    fn ts(&self) -> String {
        let c = self.clock.get() + 1;
        self.clock.set(c);
        format!("2026-11-18T17:{:02}:{:02}.000Z", c / 60, c % 60)
    }

    fn set_run(&self, n: i64, status: &str) {
        let completed = if status == "running" { None } else { Some(self.ts()) };
        self.db
            .execute(
                "INSERT INTO orchestration_v2_projection_runs (run_id, thread_id, ordinal, status, completed_at)
                 VALUES (?, ?, ?, ?, ?)
                 ON CONFLICT(run_id) DO UPDATE SET status = excluded.status, completed_at = excluded.completed_at",
                params![run_id(n), T, n, status, completed],
            )
            .unwrap();
    }

    fn item(&self, id: &str, run: i64, ordinal: i64, item_type: &str, status: &str, payload: Value) {
        let at = self.ts();
        let mut payload = payload;
        payload["completedAt"] = json!(at);
        self.db
            .execute(
                "INSERT INTO orchestration_v2_projection_turn_items
                   (turn_item_id, thread_id, run_id, ordinal, type, status, updated_at, payload_json)
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?)
                 ON CONFLICT(turn_item_id) DO UPDATE SET status = excluded.status,
                   updated_at = excluded.updated_at, payload_json = excluded.payload_json",
                params![id, T, run_id(run), ordinal, item_type, status, at, payload.to_string()],
            )
            .unwrap();
    }
}

fn run_id(n: i64) -> String {
    format!("run:thread:{T}:ordinal:{n}")
}

fn kinds(evs: &[WatchEvent]) -> Vec<String> {
    evs.iter().map(|e| format!("{}:{}", e.kind.name(), e.text)).collect()
}

#[test]
fn holds_assistant_messages_until_it_knows_whether_they_are_interim_or_final() {
    let f = Fixture::new();
    f.set_run(1, "running");
    f.item("u1", 1, 1, "user_message", "completed", json!({"text": "Add a minimum stay", "inputIntent": "turn_start"}));
    f.item("a1", 1, 2, "assistant_message", "completed", json!({"text": "Checking the booking code."}));
    let mut w = ThreadWatcher::new(&f.db, T);
    assert_eq!(kinds(&w.poll(false).unwrap()), ["user:Add a minimum stay"]);

    f.item("t1", 1, 3, "command_execution", "running", json!({"input": "rg minStay", "output": "big"}));
    assert_eq!(
        kinds(&w.poll(false).unwrap()),
        ["interim:Checking the booking code.", "tool:shell: rg minStay"]
    );

    f.item("t1", 1, 3, "command_execution", "completed", json!({"input": "rg minStay", "output": "big"}));
    f.item("r1", 1, 4, "reasoning", "completed", json!({"text": "No rule exists yet."}));
    f.item("a2", 1, 5, "assistant_message", "completed", json!({"text": "Done: 2-night minimum."}));
    assert_eq!(kinds(&w.poll(false).unwrap()), ["thinking:No rule exists yet."]);

    f.set_run(1, "completed");
    assert_eq!(kinds(&w.poll(false).unwrap()), ["final:Done: 2-night minimum.", "turn:completed"]);
    assert!(w.poll(false).unwrap().is_empty());
}

#[test]
fn start_from_now_hides_history_but_still_reports_the_running_turns_final_answer() {
    let f = Fixture::new();
    f.set_run(1, "completed");
    f.item("u1", 1, 1, "user_message", "completed", json!({"text": "old prompt"}));
    f.item("a1", 1, 2, "assistant_message", "completed", json!({"text": "old answer"}));
    f.set_run(2, "running");
    f.item("u2", 2, 1, "user_message", "completed", json!({"text": "current prompt"}));
    f.item("a2", 2, 2, "assistant_message", "completed", json!({"text": "current answer"}));

    let mut w = ThreadWatcher::new(&f.db, T);
    assert!(w.poll(true).unwrap().is_empty());
    f.set_run(2, "completed");
    assert_eq!(kinds(&w.poll(false).unwrap()), ["final:current answer", "turn:completed"]);
}

#[test]
fn history_poll_classifies_a_finished_thread() {
    let f = Fixture::new();
    f.set_run(1, "completed");
    f.item("u1", 1, 1, "user_message", "completed", json!({"text": "q"}));
    f.item("a1", 1, 2, "assistant_message", "completed", json!({"text": "looking"}));
    f.item("d1", 1, 3, "dynamic_tool", "completed", json!({"toolName": "Read", "input": {"file_path": "/x"}}));
    f.item("c1", 1, 4, "checkpoint", "completed", json!({}));
    f.item("a2", 1, 5, "assistant_message", "completed", json!({"text": "answer"}));
    f.item("e1", 1, 6, "compaction", "completed", json!({"beforeTokenCount": 10, "afterTokenCount": 2}));
    let evs = ThreadWatcher::new(&f.db, T).poll(false).unwrap();
    assert_eq!(
        kinds(&evs),
        [
            "user:q",
            "interim:looking",
            r#"tool:Read: {"file_path":"/x"}"#,
            "final:answer",
            "event:Context compacted 10 -> 2 tokens",
            "turn:completed",
        ]
    );
    assert_eq!(evs[0].run, Some(1));
    assert_eq!(evs[2].tool.as_ref().unwrap().name, "Read");
}

#[test]
fn resolves_full_ids_prefixes_and_links() {
    let f = Fixture::new();
    assert_eq!(resolve_thread(&f.db, T).unwrap()[0].thread_id, T);
    assert_eq!(resolve_thread(&f.db, "11111111").unwrap()[0].thread_id, T);
    let link = format!("https://app.example/threads/{T}?x=1");
    assert_eq!(resolve_thread(&f.db, &link).unwrap()[0].thread_id, T);
    assert!(resolve_thread(&f.db, "999").unwrap().is_empty());
}

#[test]
fn jsonl_keeps_the_documented_fields() {
    let f = Fixture::new();
    f.set_run(1, "running");
    f.item("u1", 1, 1, "user_message", "completed", json!({"text": "hi", "inputIntent": "steer"}));
    let evs = ThreadWatcher::new(&f.db, T).poll(false).unwrap();
    let v: Value = serde_json::to_value(&evs[0]).unwrap();
    assert_eq!(v["kind"], "user");
    assert_eq!(v["threadId"], T);
    assert_eq!(v["run"], 1);
    assert_eq!(v["itemType"], "user_message");
    assert_eq!(v["intent"], "steer");
    assert!(v.get("ordinal").is_none());
    assert!(v.get("tool").is_none());
}

fn args(a: &[&str]) -> Vec<String> {
    a.iter().map(|s| s.to_string()).collect()
}

fn parsed_kinds(a: &[&str]) -> Vec<Kind> {
    match parse_watch_args(&args(a)).unwrap() {
        Parsed::Watch(o) => o.kinds.into_iter().collect(),
        Parsed::Help => panic!("unexpected help"),
    }
}

#[test]
fn parse_defaults_to_user_and_final() {
    assert_eq!(parsed_kinds(&["abc"]), [Kind::User, Kind::Final]);
}

#[test]
fn parse_adding_options_and_kinds_combine() {
    assert_eq!(
        parsed_kinds(&["abc", "--kinds", "final", "--tools", "--thinking"]),
        [Kind::Final, Kind::Tool, Kind::Thinking]
    );
    assert_eq!(parsed_kinds(&["abc", "--kinds=tool"]), [Kind::Tool]);
}

#[test]
fn parse_help_wins_anywhere() {
    assert!(matches!(parse_watch_args(&args(&["abc", "--tools", "--help"])).unwrap(), Parsed::Help));
}

#[test]
fn parse_rejects_unknown_options_and_kinds() {
    let err = |a: &[&str]| match parse_watch_args(&args(a)) {
        Err(e) => e.0,
        Ok(_) => panic!("expected an error"),
    };
    assert!(err(&["abc", "--nope"]).contains("Unknown option"));
    assert!(err(&["abc", "--kinds", "foo"]).contains("unknown kind"));
    assert!(err(&["abc", "--where", "e.kind ==="]).starts_with("--where:"));
    assert!(err(&["abc", "--last", "x"]).contains("whole number"));
}
