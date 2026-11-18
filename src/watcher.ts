import { Database } from "bun:sqlite";

/** The kinds of event `t3tap watch` can print. */
export type Kind = "user" | "final" | "interim" | "tool" | "thinking" | "event" | "turn";
export const ALL_KINDS: Kind[] = ["user", "final", "interim", "tool", "thinking", "event", "turn"];
export const DEFAULT_KINDS: Kind[] = ["user", "final"];

export interface WatchEvent {
  kind: Kind;
  threadId: string;
  runId: string | null;
  /** Turn number within the thread (T3 calls a turn a "run"). */
  run: number | null;
  itemId: string | null;
  /** Raw T3 item type, such as assistant_message or command_execution. */
  itemType: string | null;
  status: string | null;
  /** ISO 8601 time in UTC. */
  at: string;
  text: string;
  tool?: { name: string; input: string };
  /** For user messages: turn_start, steer, queued_turn and so on. */
  intent?: string;
  /** Provider that produced the item, when T3 records it. */
  provider?: string;
  /** Sort key inside a poll. */
  ordinal: number;
}

const TERMINAL_RUN = new Set(["completed", "cancelled", "interrupted", "failed", "rolled_back"]);
const TERMINAL_ITEM = new Set(["completed", "failed", "interrupted", "cancelled"]);
export const TOOL_TYPES = new Set([
  "command_execution",
  "dynamic_tool",
  "file_change",
  "file_search",
  "web_search",
  "subagent",
]);
const WORK_TYPES = new Set([...TOOL_TYPES, "assistant_message"]);
/** Bookkeeping items that never become events. */
const SKIP_TYPES = new Set(["checkpoint"]);

interface ItemRow {
  turn_item_id: string;
  run_id: string | null;
  ordinal: number;
  type: string;
  status: string;
  updated_at: string;
  payload: string;
}

interface RunRow {
  run_id: string;
  ordinal: number;
  status: string;
  completed_at: string | null;
}

interface PendingMessage {
  itemId: string;
  ordinal: number;
  text: string;
  at: string;
  status: string;
  provider?: string;
  /** Seen before the watch started in "now" mode: print it only if it turns out to be final. */
  preStart: boolean;
}

interface RunState {
  ordinal: number | null;
  status: string | null;
  completedAt: string | null;
  /** Highest ordinal of any assistant message or tool call in the run, in any status. */
  maxWork: number;
  pending: PendingMessage[];
  endEmitted: boolean;
}

export interface ThreadInfo {
  threadId: string;
  title: string;
  provider: string | null;
  deleted: boolean;
}

export function openDb(path: string): Database {
  return new Database(path, { readonly: true });
}

/** Resolve a full thread ID, a unique prefix, or text that contains a thread ID. */
export function resolveThread(db: Database, arg: string): ThreadInfo[] {
  const uuid = arg.match(/[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}/i)?.[0];
  const candidates = uuid && uuid !== arg ? [arg, uuid] : [arg];
  for (const c of candidates) {
    const exact = db
      .query(
        `SELECT thread_id, title, provider_instance_id, default_provider, deleted_at
         FROM orchestration_v2_projection_threads WHERE thread_id = ?`,
      )
      .all(c) as any[];
    if (exact.length) return exact.map(toInfo);
  }
  if (arg.length < 4) return [];
  const prefix = db
    .query(
      `SELECT thread_id, title, provider_instance_id, default_provider, deleted_at
       FROM orchestration_v2_projection_threads
       WHERE substr(thread_id, 1, ?) = ? ORDER BY updated_at DESC LIMIT 10`,
    )
    .all(arg.length, arg) as any[];
  return prefix.map(toInfo);
}

function toInfo(r: any): ThreadInfo {
  return {
    threadId: r.thread_id,
    title: r.title,
    provider: r.provider_instance_id ?? r.default_provider ?? null,
    deleted: r.deleted_at != null,
  };
}

function parseJson(s: string): any {
  try {
    return JSON.parse(s);
  } catch {
    return {};
  }
}

function runOrdinalFromId(runId: string | null): number | null {
  const m = runId?.match(/ordinal:(\d+)$/);
  return m ? Number(m[1]) : null;
}

function str(v: unknown): string {
  if (v == null) return "";
  if (typeof v === "string") return v;
  try {
    return JSON.stringify(v);
  } catch {
    return String(v);
  }
}

function describeTool(type: string, p: any): { name: string; input: string } {
  switch (type) {
    case "command_execution":
      return { name: "shell", input: str(p.input ?? p.command) };
    case "dynamic_tool":
      return { name: str(p.toolName) || "tool", input: str(p.input) };
    case "file_change": {
      const ops = Array.isArray(p.changes)
        ? p.changes.map((c: any) => `${c.operation ?? "change"} ${c.path ?? ""}`.trim()).join(", ")
        : "";
      return { name: "file_change", input: ops || str(p.fileName) };
    }
    case "web_search":
      return { name: "web_search", input: str(p.query ?? p.patterns) };
    case "file_search":
      return { name: "file_search", input: str(p.query ?? p.patterns ?? p.input) };
    case "subagent":
      return {
        name: `subagent${p.driver ? `:${p.driver}` : ""}`,
        input: [p.title, p.prompt].filter(Boolean).map(str).join(": "),
      };
    default:
      return { name: type, input: str(p.input) };
  }
}

function describeOther(type: string, p: any): string {
  switch (type) {
    case "handoff":
      return `Provider handoff ${str(p.fromProviderInstanceIds)} -> ${str(p.toProviderInstanceId)}${p.toModel ? ` (${p.toModel})` : ""}`;
    case "compaction":
      return `Context compacted${p.beforeTokenCount ? ` ${p.beforeTokenCount} -> ${p.afterTokenCount} tokens` : ""}`;
    case "error":
      return [p.title, p.failure?.message ?? p.message].filter(Boolean).map(str).join(": ");
    case "notification":
      return str(p.summary ?? p.title ?? p.outcome);
    case "approval_request":
      return `Approval requested (${str(p.requestKind)}): ${str(p.prompt)}`;
    case "user_input_request":
      return `Question for the user: ${
        Array.isArray(p.questions) ? p.questions.map((q: any) => str(q.question)).join(" | ") : ""
      }`;
    case "todo_list":
      return Array.isArray(p.steps)
        ? p.steps.map((s: any) => `[${s.status}] ${str(s.text)}`).join("\n")
        : "Plan updated";
    case "secret_request":
      return `Secret requested: ${str(p.label ?? p.title)}`;
    default:
      return [p.title, p.message ?? p.summary].filter(Boolean).map(str).join(": ") || type;
  }
}

/**
 * Follows one T3 thread by polling T3's local database read-only.
 * Call poll() repeatedly; each call returns the new events in thread order.
 * It returns every kind; filtering is the caller's job.
 */
export class ThreadWatcher {
  private emitted = new Set<string>();
  private recordedMessages = new Set<string>();
  private runs = new Map<string, RunState>();
  private cursor = "";
  private itemQuery;
  private runQuery;

  constructor(
    db: Database,
    readonly threadId: string,
  ) {
    this.itemQuery = db.query(
      `SELECT turn_item_id, run_id, ordinal, type, status, updated_at,
         CASE WHEN json_valid(payload_json)
              THEN json_remove(payload_json, '$.output', '$.diffStr', '$.results', '$.result')
              ELSE '{}' END AS payload
       FROM orchestration_v2_projection_turn_items
       WHERE thread_id = ? AND updated_at >= ?
       ORDER BY ordinal`,
    );
    this.runQuery = db.query(
      `SELECT run_id, ordinal, status, completed_at
       FROM orchestration_v2_projection_runs WHERE thread_id = ?`,
    );
  }

  private run(runId: string): RunState {
    let r = this.runs.get(runId);
    if (!r) {
      r = {
        ordinal: runOrdinalFromId(runId),
        status: null,
        completedAt: null,
        maxWork: -1,
        pending: [],
        endEmitted: false,
      };
      this.runs.set(runId, r);
    }
    return r;
  }

  /**
   * @param preStart true for the first poll in "start from now" mode: every event it
   *   finds is history, but messages of turns still running stay pending so their final
   *   answer can still be reported.
   */
  poll(preStart = false): WatchEvent[] {
    const out: WatchEvent[] = [];
    const rows = this.itemQuery.all(this.threadId, this.cursor) as ItemRow[];
    for (const row of rows) {
      if (row.updated_at > this.cursor) this.cursor = row.updated_at;
      this.process(row, out, preStart);
    }
    for (const r of this.runQuery.all(this.threadId) as RunRow[]) {
      const st = this.run(r.run_id);
      st.ordinal = r.ordinal;
      st.status = r.status;
      st.completedAt = r.completed_at;
    }
    this.classify(out);
    out.sort((a, b) => (a.run ?? 0) - (b.run ?? 0) || a.ordinal - b.ordinal);
    return preStart ? [] : out;
  }

  private base(row: ItemRow, p: any): Omit<WatchEvent, "kind" | "text"> {
    return {
      threadId: this.threadId,
      runId: row.run_id,
      run: row.run_id ? (this.runs.get(row.run_id)?.ordinal ?? runOrdinalFromId(row.run_id)) : null,
      itemId: row.turn_item_id,
      itemType: row.type,
      status: row.status,
      at: p.completedAt ?? p.updatedAt ?? row.updated_at,
      provider: p.nativeItemRef?.driver ?? p.driver ?? undefined,
      ordinal: row.ordinal,
    };
  }

  private process(row: ItemRow, out: WatchEvent[], preStart: boolean) {
    const { type, status, turn_item_id: id } = row;
    if (SKIP_TYPES.has(type)) return;
    const terminal = TERMINAL_ITEM.has(status);

    if (row.run_id && WORK_TYPES.has(type)) {
      const r = this.run(row.run_id);
      if (row.ordinal > r.maxWork) r.maxWork = row.ordinal;
    }

    if (type === "assistant_message") {
      if (!terminal || this.recordedMessages.has(id)) return;
      this.recordedMessages.add(id);
      const p = parseJson(row.payload);
      if (!row.run_id) {
        // No turn to judge it against: report it as a finished reply.
        if (!preStart) out.push({ ...this.base(row, p), kind: "final", text: str(p.text) });
        return;
      }
      this.run(row.run_id).pending.push({
        itemId: id,
        ordinal: row.ordinal,
        text: str(p.text),
        at: p.completedAt ?? row.updated_at,
        status,
        provider: p.nativeItemRef?.driver,
        preStart,
      });
      return;
    }

    if (this.emitted.has(id)) return;
    let ev: WatchEvent | null = null;
    if (type === "user_message") {
      if (!terminal) return;
      const p = parseJson(row.payload);
      const n = Array.isArray(p.attachments) ? p.attachments.length : 0;
      ev = {
        ...this.base(row, p),
        kind: "user",
        text: str(p.text) + (n ? `\n[${n} attachment${n > 1 ? "s" : ""}]` : ""),
        intent: p.inputIntent ?? undefined,
      };
    } else if (type === "reasoning") {
      if (!terminal) return;
      const p = parseJson(row.payload);
      ev = { ...this.base(row, p), kind: "thinking", text: str(p.text) };
    } else if (TOOL_TYPES.has(type)) {
      if (status === "pending") return;
      const p = parseJson(row.payload);
      const tool = describeTool(type, p);
      ev = { ...this.base(row, p), kind: "tool", text: `${tool.name}: ${tool.input}`, tool };
    } else {
      if (!terminal && status !== "waiting") return;
      const p = parseJson(row.payload);
      ev = { ...this.base(row, p), kind: "event", text: describeOther(type, p) };
    }
    this.emitted.add(id);
    if (!preStart) out.push(ev);
  }

  /** Decide whether held assistant messages were interim or the final answer of their turn. */
  private classify(out: WatchEvent[]) {
    for (const [runId, r] of this.runs) {
      const ended = r.status != null && TERMINAL_RUN.has(r.status);
      if (r.pending.length) {
        const keep: PendingMessage[] = [];
        for (const m of r.pending) {
          let kind: Kind | null = null;
          if (r.maxWork > m.ordinal) kind = "interim";
          else if (ended) kind = "final";
          if (!kind) {
            keep.push(m);
            continue;
          }
          if (m.preStart && kind === "interim") continue;
          out.push({
            kind,
            threadId: this.threadId,
            runId,
            run: r.ordinal,
            itemId: m.itemId,
            itemType: "assistant_message",
            status: m.status,
            at: m.at,
            text: m.text,
            provider: m.provider,
            ordinal: m.ordinal,
          });
        }
        r.pending = keep;
      }
      if (ended && !r.endEmitted && r.pending.length === 0) {
        r.endEmitted = true;
        out.push({
          kind: "turn",
          threadId: this.threadId,
          runId,
          run: r.ordinal,
          itemId: null,
          itemType: null,
          status: r.status,
          at: r.completedAt ?? new Date().toISOString(),
          text: r.status ?? "ended",
          ordinal: Number.MAX_SAFE_INTEGER,
        });
      }
    }
  }
}
