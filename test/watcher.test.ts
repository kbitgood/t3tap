import { Database } from "bun:sqlite";
import { beforeEach, describe, expect, test } from "bun:test";
import { parseWatchArgs } from "../src/cli";
import { resolveThread, ThreadWatcher } from "../src/watcher";

const T = "11111111-2222-3333-4444-555555555555";
let db: Database;
let clock = 0;

function ts() {
  clock += 1;
  return new Date(Date.UTC(2026, 10, 18, 17, 0, clock)).toISOString();
}

function runId(n: number) {
  return `run:thread:${T}:ordinal:${n}`;
}

function setRun(n: number, status: string) {
  db.run(
    `INSERT INTO orchestration_v2_projection_runs (run_id, thread_id, ordinal, status, completed_at)
     VALUES (?, ?, ?, ?, ?)
     ON CONFLICT(run_id) DO UPDATE SET status = excluded.status, completed_at = excluded.completed_at`,
    [runId(n), T, n, status, status === "running" ? null : ts()],
  );
}

function item(id: string, run: number, ordinal: number, type: string, status: string, payload: object) {
  const at = ts();
  db.run(
    `INSERT INTO orchestration_v2_projection_turn_items
       (turn_item_id, thread_id, run_id, ordinal, type, status, updated_at, payload_json)
     VALUES (?, ?, ?, ?, ?, ?, ?, ?)
     ON CONFLICT(turn_item_id) DO UPDATE SET status = excluded.status,
       updated_at = excluded.updated_at, payload_json = excluded.payload_json`,
    [id, T, runId(run), ordinal, type, status, at, JSON.stringify({ ...payload, completedAt: at })],
  );
}

beforeEach(() => {
  db = new Database(":memory:");
  db.run(`CREATE TABLE orchestration_v2_projection_threads (thread_id TEXT PRIMARY KEY, title TEXT,
    provider_instance_id TEXT, default_provider TEXT, deleted_at TEXT, updated_at TEXT)`);
  db.run(`CREATE TABLE orchestration_v2_projection_runs (run_id TEXT PRIMARY KEY, thread_id TEXT,
    ordinal INTEGER, status TEXT, completed_at TEXT)`);
  db.run(`CREATE TABLE orchestration_v2_projection_turn_items (turn_item_id TEXT PRIMARY KEY,
    thread_id TEXT, run_id TEXT, ordinal INTEGER, type TEXT, status TEXT, updated_at TEXT, payload_json TEXT)`);
  db.run(`INSERT INTO orchestration_v2_projection_threads VALUES (?, 'Main', 'claudeAgent', 'claudeAgent', NULL, '')`, [T]);
});

const kinds = (evs: { kind: string; text: string }[]) => evs.map((e) => `${e.kind}:${e.text}`);

describe("ThreadWatcher", () => {
  test("holds assistant messages until it knows whether they are interim or final", () => {
    const w = new ThreadWatcher(db, T);
    setRun(1, "running");
    item("u1", 1, 1, "user_message", "completed", { text: "Add a minimum stay", inputIntent: "turn_start" });
    item("a1", 1, 2, "assistant_message", "completed", { text: "Checking the booking code." });
    expect(kinds(w.poll())).toEqual(["user:Add a minimum stay"]);

    item("t1", 1, 3, "command_execution", "running", { input: "rg minStay", output: "big" });
    expect(kinds(w.poll())).toEqual(["interim:Checking the booking code.", "tool:shell: rg minStay"]);

    item("t1", 1, 3, "command_execution", "completed", { input: "rg minStay", output: "big" });
    item("r1", 1, 4, "reasoning", "completed", { text: "No rule exists yet." });
    item("a2", 1, 5, "assistant_message", "completed", { text: "Done: 2-night minimum." });
    expect(kinds(w.poll())).toEqual(["thinking:No rule exists yet."]);

    setRun(1, "completed");
    expect(kinds(w.poll())).toEqual(["final:Done: 2-night minimum.", "turn:completed"]);
    expect(w.poll()).toEqual([]);
  });

  test("start-from-now hides history but still reports the running turn's final answer", () => {
    setRun(1, "completed");
    item("u1", 1, 1, "user_message", "completed", { text: "old prompt" });
    item("a1", 1, 2, "assistant_message", "completed", { text: "old answer" });
    setRun(2, "running");
    item("u2", 2, 1, "user_message", "completed", { text: "current prompt" });
    item("a2", 2, 2, "assistant_message", "completed", { text: "current answer" });

    const w = new ThreadWatcher(db, T);
    expect(w.poll(true)).toEqual([]);
    setRun(2, "completed");
    expect(kinds(w.poll())).toEqual(["final:current answer", "turn:completed"]);
  });

  test("history poll classifies a finished thread", () => {
    setRun(1, "completed");
    item("u1", 1, 1, "user_message", "completed", { text: "q" });
    item("a1", 1, 2, "assistant_message", "completed", { text: "looking" });
    item("d1", 1, 3, "dynamic_tool", "completed", { toolName: "Read", input: { file_path: "/x" } });
    item("c1", 1, 4, "checkpoint", "completed", {});
    item("a2", 1, 5, "assistant_message", "completed", { text: "answer" });
    item("e1", 1, 6, "compaction", "completed", { beforeTokenCount: 10, afterTokenCount: 2 });
    const evs = new ThreadWatcher(db, T).poll();
    expect(kinds(evs)).toEqual([
      "user:q",
      "interim:looking",
      'tool:Read: {"file_path":"/x"}',
      "final:answer",
      "event:Context compacted 10 -> 2 tokens",
      "turn:completed",
    ]);
  });

  test("resolves full IDs, prefixes and links", () => {
    expect(resolveThread(db, T)[0].threadId).toBe(T);
    expect(resolveThread(db, "11111111")[0].threadId).toBe(T);
    expect(resolveThread(db, `https://app.example/threads/${T}?x=1`)[0].threadId).toBe(T);
    expect(resolveThread(db, "999")).toEqual([]);
  });
});

describe("parseWatchArgs", () => {
  test("defaults to user and final", () => {
    const o = parseWatchArgs(["abc"]);
    if (o === "help") throw new Error();
    expect([...o.kinds].sort()).toEqual(["final", "user"]);
  });
  test("adding options and --kinds combine", () => {
    const o = parseWatchArgs(["abc", "--kinds", "final", "--tools", "--thinking"]);
    if (o === "help") throw new Error();
    expect([...o.kinds].sort()).toEqual(["final", "thinking", "tool"]);
  });
  test("--help wins anywhere", () => {
    expect(parseWatchArgs(["abc", "--tools", "--help"])).toBe("help");
  });
  test("rejects unknown options and kinds", () => {
    expect(() => parseWatchArgs(["abc", "--nope"])).toThrow(/Unknown option/);
    expect(() => parseWatchArgs(["abc", "--kinds", "foo"])).toThrow(/unknown kind/);
  });
});
