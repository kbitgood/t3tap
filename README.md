# t3tap

Follow a [T3 Code](https://github.com/pingdotgg/t3code) thread from the terminal while it runs. t3tap prints prompts, answers, tool calls and thinking as they happen. You pick which ones.

```console
$ t3tap watch a1b2c3d4 --tools --interim --format line
[10:15:41 PDT] USER run 3: Add a rule that guests must book at least two nights.
[10:15:44 PDT] INTERIM run 3: I'll find where bookings are validated first.
[10:15:44 PDT] TOOL run 3: shell: rg -n "validateBooking" src
[10:15:52 PDT] TOOL run 3: file_change: update src/booking/validate.ts
[10:16:03 PDT] FINAL run 3: Added a two-night minimum in validateBooking.
```

I built it so one agent can watch another. A critic agent runs t3tap on the main thread. Each prompt and answer arrives as one line, so the critic can comment while the work happens.

## Install

You need a stable [Rust](https://rustup.rs) toolchain and T3 Code on the same machine. I have tested it only on macOS.

```sh
cargo install --git https://github.com/kbitgood/t3tap.git
```

This puts `t3tap` in `~/.cargo/bin`. In a checkout, use `cargo install --path .`. The binary includes SQLite and the JavaScript engine for `--where`.

## Use

You need the thread ID. Inside a T3 agent it is in `$T3CODE_THREAD_ID`. A unique prefix, usually 8 characters, is enough.

```sh
t3tap watch a1b2c3d4              # prompts and final answers, from now on
t3tap watch a1b2c3d4 --last 10    # last 10 first, then follow
t3tap watch a1b2c3d4 --all        # everything
```

`t3tap watch --help` documents every option, field and exit code.

## Event kinds

The default is `user` and `final`, which reads like a chat log.

| Kind | What it is | Option |
|---|---|---|
| `user` | Prompt, steer or queued message | default |
| `final` | Last assistant message of a turn | default |
| `interim` | Assistant messages between tool calls | `--interim` |
| `tool` | Tool name and input, never output | `--tools` |
| `thinking` | Reasoning, cut to 600 characters | `--thinking` |
| `event` | Errors, handoffs, compaction, approvals, questions | `--events` |
| `turn` | Marker when a turn ends, with its status | `--turns` |

`--all` adds every kind. `--kinds final,tool` replaces the default set.

A `final` message prints when its turn ends. Only then is it known to be the last message. Use `--interim` to see text earlier.

## Filters

```sh
t3tap watch a1b2c3d4 --kinds tool --tool-name shell        # shell commands only
t3tap watch a1b2c3d4 --all --match migration --exclude test
t3tap watch a1b2c3d4 --all --where 'e.status === "failed"' # any JS expression
```

`--where` runs in [QuickJS](https://bellard.org/quickjs/). Regexes use Rust syntax: no lookaround or backreferences.

`--thinking-chars`, `--tool-chars` and `--max-chars` cut long text. Cut text ends with `… [+N chars]`.

## Start, stop and format

| Option | Effect |
|---|---|
| (none) | Only new events. A turn already running still prints its final answer. |
| `--last <n>` / `--since <time>` / `--from-start` | Print history first, then follow. `--since` takes ISO 8601 or `10m`, `2h`. |
| `--no-follow` | Print history and exit. |
| `--exit-after-turn` | Exit when the next turn ends. |
| `--format text\|line\|jsonl` | Indented text (default), one line per event, or JSON per line. |

Status and errors go to stderr. Stdout has only events.

## Use from an agent

Any tool that turns output lines into notifications works, such as Claude Code's `Monitor` tool:

```sh
t3tap watch <main-thread-id> --format line --max-chars 2000
```

- Keep output small. The default kinds are usually enough for a critic.
- Monitors time out. Use `--format jsonl`, keep the last event's `at`, and restart with `--since <at>`.
- Do not follow your own thread. The agent would react to itself forever. `watch self` needs `--allow-self` to follow.

## How it works

T3 stores every thread in a local SQLite database, `~/.t3/userdata/statev2.sqlite`, in one format for all providers. t3tap opens it read-only and polls every 500 ms. So one command works for Claude, Codex and Pi threads, and for threads that change provider.

t3tap holds each assistant message until a later step appears (`interim`) or the turn ends (`final`). It never writes to the database or sends anything to a thread.

## Limitations

- It reads T3's internal tables. A T3 update can break it.
- A live tool call prints when it starts, so failures show only in history: `--no-follow --where 'e.status === "failed"'`.
- Thinking is what T3 stores. For some providers that is a summary.
- It must run on the machine with the T3 server.

## Development

```sh
cargo test          # tests use an in-memory copy of T3's tables
cargo clippy --all-targets
```

`src/watcher.rs` reads and classifies items. `src/cli.rs` has options, filters and the watch loop. `src/help/` has the help text. `src/format.rs` prints events. `src/where_expr.rs` runs `--where`.

## License

[MIT](LICENSE)
