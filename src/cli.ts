#!/usr/bin/env bun
import { existsSync } from "node:fs";
import { homedir } from "node:os";
import { join } from "node:path";
import { formatEvent, type Format, truncate } from "./format";
import { ALL_KINDS, DEFAULT_KINDS, type Kind, openDb, resolveThread, ThreadWatcher, type WatchEvent } from "./watcher";

const VERSION = "0.1.0";

const MAIN_HELP = `t3tap ${VERSION}: stream a T3 Code thread as it happens.

USAGE
  t3tap <command> [options]
  t3tap <command> --help

COMMANDS
  watch <thread>   Follow one T3 thread and print its messages, tool calls and
                   thinking as they arrive. Run "t3tap watch --help" for all
                   options and examples.
  help [command]   Show help for t3tap or for one command.

GLOBAL OPTIONS
  -h, --help       Show this help. Every command also accepts --help.
  -v, --version    Print the version.

WHAT IT READS
  t3tap reads T3 Code's local database (~/.t3/userdata/statev2.sqlite)
  read-only. It never writes to it and never sends anything to a thread.
  T3 stores Claude, Codex, Pi and other providers in one format, so the same
  command works for every thread, including threads that switch provider.

QUICK START
  t3tap watch <thread-id>                 # user prompts and final answers
  t3tap watch <thread-id> --tools         # ...plus tool calls, without output
  t3tap watch <thread-id> --all --last 20 # everything, starting 20 events back
`;

const WATCH_HELP = `t3tap watch: follow one T3 Code thread and print what happens in it.

USAGE
  t3tap watch <thread> [options]

ARGUMENTS
  <thread>   Which thread to follow. Any of:
               - the full thread ID, e.g. a1b2c3d4-5e6f-4a7b-8c9d-0e1f2a3b4c5d
               - a unique prefix of at least 4 characters, e.g. a1b2c3d4
               - any text that contains the ID, such as a T3 thread link
               - "self": the thread this command runs in (from $T3CODE_THREAD_ID)
             Inside a T3 agent the current thread ID is in $T3CODE_THREAD_ID.
             The T3 tool t3_thread_list also lists thread IDs.

WHAT IT PRINTS
  Each thing that happens in the thread is an event with one kind:

    user      A message from the person: a new prompt, a steer sent mid-turn,
              or a queued message. Printed as soon as T3 records it.
    final     The agent's finished answer: the last assistant message of a
              turn. Printed when the turn ends, because only then is it known
              to be the last one.
    interim   Assistant messages written between tool calls, before the final
              answer ("I'll check the config first..."). Printed once a later
              tool call or message shows it was not the last one.
    tool      A tool call: shell commands, MCP and built-in tools, file edits,
              web searches, subagents. Shows the tool name and its input,
              never its output. Printed when the call starts.
    thinking  The agent's reasoning or reasoning summary, when the provider
              exposes it. Printed when each block finishes. Cut to
              --thinking-chars characters by default.
    event     Everything else T3 records: errors, provider handoffs, context
              compaction, approval requests, questions to the user,
              interrupts, plan updates, notifications, system notices.
    turn      A marker printed when a turn ends, with its status (completed,
              interrupted, failed, cancelled). T3 calls a turn a "run".

  Default kinds: user, final. Each option below adds kinds to that set.

CHOOSING WHAT TO PRINT
  --tools               Add tool calls (name and input, no output).
  --thinking            Add thinking, each block cut to --thinking-chars.
  --interim             Add assistant messages written between tool calls.
  --events              Add other events (errors, handoffs, approvals...).
  --turns               Add a marker at the end of each turn.
  --all                 Add every kind.
  --kinds <list>        Replace the default set with this comma-separated list,
                        e.g. --kinds final or --kinds user,tool. The adding
                        options above still add to it.

CUSTOM FILTERS (applied after the kinds; an event must pass all of them)
  --tool-name <regex>   Keep only tool calls whose tool name matches, e.g.
                        --tool-name 'shell|Bash' or --tool-name '^mcp__'.
                        Events of other kinds are not affected.
  --match <regex>       Keep only events whose text matches. Case-insensitive.
                        For tool calls the text is "name: input".
  --exclude <regex>     Drop events whose text matches. Case-insensitive.
  --where <js>          A JavaScript expression over the event "e"; keep the
                        event when it is truthy. Fields of e:
                          e.kind      user|final|interim|tool|thinking|event|turn
                          e.text      message text, or "name: input" for tools
                          e.tool      {name, input} for tool calls, else undefined
                          e.itemType  raw T3 type: user_message, assistant_message,
                                      reasoning, command_execution, dynamic_tool,
                                      file_change, web_search, subagent, error,
                                      handoff, compaction, approval_request, ...
                          e.status    completed, running, failed, waiting, ...
                          e.run       turn number in the thread
                          e.intent    for user messages: turn_start, steer, queued_turn
                          e.provider  claudeAgent, codex, ... when known
                          e.at        ISO 8601 time in UTC
                        Examples:
                          --where 'e.kind !== "tool" || e.tool.name === "shell"'
                          --where 'e.text.length > 200'
                          --where 'e.status === "failed"'
                        Errors inside the expression count as false and are
                        reported once on stderr.

LENGTH LIMITS (0 means no limit; cut text ends with "… [+N chars]")
  --thinking-chars <n>  Characters of each thinking block. Default 600.
  --tool-chars <n>      Characters of each tool input. Default 300.
  --max-chars <n>       Characters of any event's text. Default 0.

WHERE TO START
  (default)             Print only what happens from now on. If a turn is
                        already running, its final answer is still printed
                        when it ends.
  --last <n>            First print the last n matching events, then follow.
  --since <time>        First print matching events newer than <time>, then
                        follow. <time> is ISO 8601 (2026-11-18T17:08:21Z) or
                        an age like 90s, 10m, 2h, 1d. Use the "at" field of the
                        last event you saw to resume without a gap.
  --from-start          Print the whole thread first, then follow.

WHEN TO STOP
  (default)             Follow until interrupted (Ctrl+C or SIGTERM).
  --no-follow           Print the starting events and exit. Combine with
                        --last, --since or --from-start.
  --exit-after-turn     Exit once the next turn of the thread ends (after its
                        final answer is printed). Good for "tell me when the
                        agent replies".

OUTPUT
  --format <fmt>        text  (default) Header line, then the text indented.
                              Tool calls and short events fit on one line.
                        line  One line per event; newlines become "\\n".
                        jsonl One JSON object per line with all fields above.
                              Best for scripts and agents.
  --utc                 Show times in UTC instead of local time.
  --color / --no-color  Force colors on or off. Default: on for a terminal,
                        off when piped or when NO_COLOR is set.
  --quiet               Do not print the "watching ..." line on stderr.

OTHER
  --interval <ms>       How often to check the database. Default 500.
  --db <path>           T3 database file. Default: $T3TAP_DB, else
                        $T3CODE_HOME/userdata/statev2.sqlite, else
                        ~/.t3/userdata/statev2.sqlite.
  --allow-self          Allow following the thread this command runs in.
                        Refused by default: an agent that reacts to its own
                        output loops forever. --no-follow needs no override.
  -h, --help            Show this help.

EXAMPLES
  Watch a thread like a chat log:
    t3tap watch a1b2c3d4

  Catch up on the last 10 exchanges, then keep following:
    t3tap watch a1b2c3d4 --last 10

  See what the agent is doing, not only what it says:
    t3tap watch a1b2c3d4 --tools --interim

  Everything, including thinking cut to 200 characters:
    t3tap watch a1b2c3d4 --all --thinking-chars 200

  Only shell commands, one line each:
    t3tap watch a1b2c3d4 --kinds tool --tool-name shell --format line

  Only failures and errors:
    t3tap watch a1b2c3d4 --all --where 'e.status === "failed" || e.itemType === "error"'

  Wait for the next answer, print it, exit:
    t3tap watch a1b2c3d4 --kinds final --exit-after-turn

  Feed a script or another agent:
    t3tap watch a1b2c3d4 --format jsonl | jq -r 'select(.kind=="final") | .text'

FOR AGENTS
  - To get woken by each event, run this under a tool that turns output lines
    into notifications (Claude Code's Monitor tool) or tail its output file
    from a background process.
  - Use --format jsonl or --format line so each event is one line and
    nothing else is printed on stdout. Status and errors go to stderr.
  - Keep output small: the default kinds are usually enough for a reviewer or
    critic. Add --tools only when you need to see actions.
  - When a watch is restarted (for example after a monitor times out), pass
    --since <at of the last event you saw> so nothing is missed or repeated.
  - Never follow your own thread with a watcher that replies to what it sees.

EXIT CODES
  0  stopped normally, or --no-follow / --exit-after-turn finished
  1  database error
  2  bad command line
  3  thread not found, or the prefix matches more than one thread
`;

interface WatchOptions {
  thread?: string;
  kinds: Set<Kind>;
  toolName?: RegExp;
  match?: RegExp;
  exclude?: RegExp;
  where?: (e: WatchEvent) => unknown;
  whereSource?: string;
  thinkingChars: number;
  toolChars: number;
  maxChars: number;
  start: { mode: "now" } | { mode: "last"; n: number } | { mode: "since"; at: string } | { mode: "all" };
  follow: boolean;
  exitAfterTurn: boolean;
  format: Format;
  utc: boolean;
  color: boolean;
  quiet: boolean;
  interval: number;
  db: string;
  allowSelf: boolean;
}

class UsageError extends Error {}

function defaultDb(): string {
  if (process.env.T3TAP_DB) return process.env.T3TAP_DB;
  const home = process.env.T3CODE_HOME ?? join(homedir(), ".t3");
  return join(home, "userdata", "statev2.sqlite");
}

function intArg(name: string, v: string | undefined): number {
  if (v == null || !/^\d+$/.test(v)) throw new UsageError(`${name} needs a whole number, got "${v ?? ""}".`);
  return Number(v);
}

function regexArg(name: string, v: string | undefined): RegExp {
  if (!v) throw new UsageError(`${name} needs a regular expression.`);
  try {
    return new RegExp(v, "i");
  } catch (e) {
    throw new UsageError(`${name}: invalid regular expression: ${(e as Error).message}`);
  }
}

function parseSince(v: string | undefined): string {
  if (!v) throw new UsageError("--since needs a time, such as 10m or 2026-11-18T17:08:21Z.");
  const rel = v.match(/^(\d+)(s|m|h|d)$/);
  if (rel) {
    const ms = Number(rel[1]) * { s: 1e3, m: 6e4, h: 36e5, d: 864e5 }[rel[2] as "s" | "m" | "h" | "d"];
    return new Date(Date.now() - ms).toISOString();
  }
  const d = new Date(v);
  if (Number.isNaN(d.getTime())) throw new UsageError(`--since: cannot read "${v}" as a time.`);
  return d.toISOString();
}

export function parseWatchArgs(argv: string[]): WatchOptions | "help" {
  const o: WatchOptions = {
    kinds: new Set(DEFAULT_KINDS),
    thinkingChars: 600,
    toolChars: 300,
    maxChars: 0,
    start: { mode: "now" },
    follow: true,
    exitAfterTurn: false,
    format: "text",
    utc: false,
    color: Boolean(process.stdout.isTTY) && !process.env.NO_COLOR,
    quiet: false,
    interval: 500,
    db: defaultDb(),
    allowSelf: false,
  };
  let explicitKinds: Kind[] | null = null;
  const added = new Set<Kind>();
  for (let i = 0; i < argv.length; i++) {
    let a = argv[i];
    let val: string | undefined;
    if (a.startsWith("--") && a.includes("=")) {
      val = a.slice(a.indexOf("=") + 1);
      a = a.slice(0, a.indexOf("="));
    }
    const next = () => {
      if (val !== undefined) return val;
      const v = argv[++i];
      if (v === undefined) throw new UsageError(`${a} needs a value.`);
      return v;
    };
    switch (a) {
      case "-h":
      case "--help":
        return "help";
      case "--tools":
        added.add("tool");
        break;
      case "--thinking":
        added.add("thinking");
        break;
      case "--interim":
        added.add("interim");
        break;
      case "--events":
        added.add("event");
        break;
      case "--turns":
        added.add("turn");
        break;
      case "--all":
        for (const k of ALL_KINDS) added.add(k);
        break;
      case "--kinds": {
        const list = next()
          .split(",")
          .map((s) => s.trim())
          .filter(Boolean);
        const bad = list.filter((k) => !ALL_KINDS.includes(k as Kind));
        if (bad.length) throw new UsageError(`--kinds: unknown kind "${bad[0]}". Use: ${ALL_KINDS.join(", ")}.`);
        explicitKinds = list as Kind[];
        break;
      }
      case "--tool-name":
        o.toolName = regexArg(a, next());
        break;
      case "--match":
        o.match = regexArg(a, next());
        break;
      case "--exclude":
        o.exclude = regexArg(a, next());
        break;
      case "--where": {
        const src = next();
        try {
          o.where = new Function("e", `return (${src});`) as (e: WatchEvent) => unknown;
        } catch (e) {
          throw new UsageError(`--where: ${(e as Error).message}`);
        }
        o.whereSource = src;
        break;
      }
      case "--thinking-chars":
        o.thinkingChars = intArg(a, next());
        break;
      case "--tool-chars":
        o.toolChars = intArg(a, next());
        break;
      case "--max-chars":
        o.maxChars = intArg(a, next());
        break;
      case "--last":
        o.start = { mode: "last", n: intArg(a, next()) };
        break;
      case "--since":
        o.start = { mode: "since", at: parseSince(next()) };
        break;
      case "--from-start":
        o.start = { mode: "all" };
        break;
      case "--no-follow":
        o.follow = false;
        break;
      case "--follow":
        o.follow = true;
        break;
      case "--exit-after-turn":
        o.exitAfterTurn = true;
        break;
      case "--format": {
        const f = next();
        if (!["text", "line", "jsonl"].includes(f)) throw new UsageError(`--format must be text, line or jsonl, got "${f}".`);
        o.format = f as Format;
        break;
      }
      case "--utc":
        o.utc = true;
        break;
      case "--color":
        o.color = true;
        break;
      case "--no-color":
        o.color = false;
        break;
      case "--quiet":
      case "-q":
        o.quiet = true;
        break;
      case "--interval":
        o.interval = Math.max(50, intArg(a, next()));
        break;
      case "--db":
        o.db = next();
        break;
      case "--allow-self":
        o.allowSelf = true;
        break;
      default:
        if (a.startsWith("-")) throw new UsageError(`Unknown option "${a}".`);
        if (o.thread) throw new UsageError(`Only one thread can be watched; got "${o.thread}" and "${a}".`);
        o.thread = a;
    }
  }
  o.kinds = new Set([...(explicitKinds ?? DEFAULT_KINDS), ...added]);
  if (o.format === "jsonl") o.color = false;
  return o;
}

function makeFilter(o: WatchOptions): (e: WatchEvent) => WatchEvent | null {
  let whereFailed = false;
  return (e) => {
    if (!o.kinds.has(e.kind)) return null;
    if (o.toolName && e.kind === "tool" && !o.toolName.test(e.tool?.name ?? "")) return null;
    if (o.match && !o.match.test(e.text)) return null;
    if (o.exclude && o.exclude.test(e.text)) return null;
    if (o.where) {
      try {
        if (!o.where(e)) return null;
      } catch (err) {
        if (!whereFailed) {
          whereFailed = true;
          console.error(`t3tap: --where failed on an event (treated as false): ${(err as Error).message}`);
        }
        return null;
      }
    }
    let text = e.text;
    let tool = e.tool;
    if (e.kind === "thinking") text = truncate(text, o.thinkingChars);
    if (e.kind === "tool" && tool) {
      tool = { ...tool, input: truncate(tool.input, o.toolChars) };
      text = `${tool.name}: ${tool.input}`;
    }
    text = truncate(text, o.maxChars);
    return { ...e, text, ...(tool ? { tool } : {}) };
  };
}

async function watch(argv: string[]): Promise<number> {
  const o = parseWatchArgs(argv);
  if (o === "help") {
    process.stdout.write(WATCH_HELP);
    return 0;
  }
  if (!o.thread) throw new UsageError("Missing <thread>. Example: t3tap watch a1b2c3d4");

  let arg = o.thread;
  if (arg === "self") {
    if (!process.env.T3CODE_THREAD_ID) throw new UsageError('"self" works only inside a T3 thread ($T3CODE_THREAD_ID is not set).');
    arg = process.env.T3CODE_THREAD_ID;
  }
  if (!existsSync(o.db)) {
    console.error(`t3tap: no T3 database at ${o.db}. Pass --db or set T3TAP_DB.`);
    return 1;
  }
  let db;
  try {
    db = openDb(o.db);
  } catch (e) {
    console.error(`t3tap: cannot open ${o.db}: ${(e as Error).message}`);
    return 1;
  }
  const matches = resolveThread(db, arg);
  if (matches.length === 0) {
    console.error(`t3tap: no thread matches "${arg}".`);
    return 3;
  }
  if (matches.length > 1) {
    console.error(`t3tap: "${arg}" matches ${matches.length} threads; use more of the ID:`);
    for (const m of matches) console.error(`  ${m.threadId}  ${m.title}`);
    return 3;
  }
  const thread = matches[0];
  if (o.follow && !o.allowSelf && thread.threadId === process.env.T3CODE_THREAD_ID) {
    console.error(
      "t3tap: refusing to follow the thread this command runs in. An agent that reacts to its own output loops.\n" +
        "Use --no-follow to read it once, or --allow-self if you are sure.",
    );
    return 2;
  }
  if (!o.quiet) {
    console.error(
      `t3tap: watching "${thread.title}" (${thread.threadId})${thread.provider ? ` on ${thread.provider}` : ""}` +
        `${thread.deleted ? " [deleted]" : ""}; kinds: ${[...o.kinds].join(", ")}`,
    );
  }

  const watcher = new ThreadWatcher(db, thread.threadId);
  const filter = makeFilter(o);
  const fmt = { format: o.format, color: o.color, utc: o.utc };
  const write = (e: WatchEvent) => process.stdout.write(formatEvent(e, fmt));

  let stopping = false;
  const stop = () => {
    stopping = true;
  };
  process.on("SIGINT", stop);
  process.on("SIGTERM", stop);
  process.stdout.on("error", (err: NodeJS.ErrnoException) => {
    if (err.code === "EPIPE") process.exit(0);
  });

  const readPoll = (preStart: boolean) => {
    for (let attempt = 0; ; attempt++) {
      try {
        return watcher.poll(preStart);
      } catch (e) {
        if (attempt < 5 && /busy|locked/i.test((e as Error).message)) {
          Bun.sleepSync(100);
          continue;
        }
        throw e;
      }
    }
  };

  // First pass: history.
  const history = readPoll(o.start.mode === "now");
  if (o.start.mode !== "now") {
    let shown = history.map(filter).filter((e): e is WatchEvent => e !== null);
    if (o.start.mode === "last") shown = o.start.n ? shown.slice(-o.start.n) : [];
    if (o.start.mode === "since") {
      const since = o.start.at;
      shown = shown.filter((e) => e.at > since);
    }
    for (const e of shown) write(e);
  }
  if (!o.follow) return 0;

  while (!stopping) {
    await Bun.sleep(o.interval);
    let events: WatchEvent[];
    try {
      events = readPoll(false);
    } catch (e) {
      console.error(`t3tap: database error: ${(e as Error).message}`);
      return 1;
    }
    let turnEnded = false;
    for (const e of events) {
      if (e.kind === "turn") turnEnded = true;
      const f = filter(e);
      if (f) write(f);
    }
    if (o.exitAfterTurn && turnEnded) return 0;
  }
  return 0;
}

async function main(argv: string[]): Promise<number> {
  const [cmd, ...rest] = argv;
  if (!cmd || cmd === "-h" || cmd === "--help") {
    process.stdout.write(MAIN_HELP);
    return cmd ? 0 : 2;
  }
  if (cmd === "-v" || cmd === "--version") {
    console.log(VERSION);
    return 0;
  }
  if (cmd === "help") {
    const topic = rest[0];
    if (!topic) process.stdout.write(MAIN_HELP);
    else if (topic === "watch") process.stdout.write(WATCH_HELP);
    else {
      console.error(`t3tap: no command "${topic}". Commands: watch.`);
      return 2;
    }
    return 0;
  }
  if (cmd === "watch") return watch(rest);
  console.error(`t3tap: unknown command "${cmd}". Run "t3tap --help".`);
  return 2;
}

if (import.meta.main) {
  main(process.argv.slice(2))
    .then((code) => process.exit(code))
    .catch((e) => {
      if (e instanceof UsageError) {
        console.error(`t3tap: ${e.message}\nRun "t3tap watch --help" for usage.`);
        process.exit(2);
      }
      console.error(`t3tap: ${(e as Error).stack ?? e}`);
      process.exit(1);
    });
}
