import type { Kind, WatchEvent } from "./watcher";

export type Format = "text" | "line" | "jsonl";

export interface FormatOptions {
  format: Format;
  color: boolean;
  utc: boolean;
}

const LABEL: Record<Kind, string> = {
  user: "USER",
  final: "FINAL",
  interim: "INTERIM",
  tool: "TOOL",
  thinking: "THINKING",
  event: "EVENT",
  turn: "TURN END",
};

const COLOR: Record<Kind, string> = {
  user: "\x1b[1;36m",
  final: "\x1b[1;32m",
  interim: "\x1b[32m",
  tool: "\x1b[33m",
  thinking: "\x1b[2;35m",
  event: "\x1b[1;31m",
  turn: "\x1b[2m",
};
const DIM = "\x1b[2m";
const RESET = "\x1b[0m";

const zone = Intl.DateTimeFormat().resolvedOptions().timeZone;

/** Short zone label. British Columbia stays on UTC-7 all year and calls it PDT. */
function zoneLabel(d: Date): string {
  if (zone === "America/Vancouver") return "PDT";
  const part = new Intl.DateTimeFormat("en-US", { timeZoneName: "short" })
    .formatToParts(d)
    .find((p) => p.type === "timeZoneName");
  return part?.value ?? "";
}

export function formatTime(iso: string, utc: boolean): string {
  const d = new Date(iso);
  if (Number.isNaN(d.getTime())) return iso;
  if (utc) return `${d.toISOString().slice(11, 19)} UTC`;
  const hms = d.toLocaleTimeString("en-GB", { hour12: false });
  return `${hms} ${zoneLabel(d)}`.trim();
}

export function formatEvent(e: WatchEvent, o: FormatOptions): string {
  if (o.format === "jsonl") {
    const { ordinal: _o, ...rest } = e;
    return `${JSON.stringify(rest)}\n`;
  }
  const time = formatTime(e.at, o.utc);
  const run = e.run != null ? ` run ${e.run}` : "";
  const extra =
    e.kind === "user" && e.intent && e.intent !== "turn_start"
      ? ` (${e.intent})`
      : e.kind === "event" && e.itemType
        ? ` ${e.itemType}`
        : "";
  const label = LABEL[e.kind];
  const c = o.color ? COLOR[e.kind] : "";
  const d = o.color ? DIM : "";
  const r = o.color ? RESET : "";
  const header = `${c}[${time}] ${label}${r}${d}${run}${extra}${r}`;

  if (o.format === "line") {
    const flat = e.text.replace(/\r?\n/g, "\\n");
    return `${header}: ${flat}\n`;
  }
  // text: one-line kinds stay on the header line, the rest go below it, indented.
  if (e.kind === "tool" || e.kind === "turn" || (e.kind === "event" && !e.text.includes("\n"))) {
    return `${header}: ${e.text}\n`;
  }
  const body = e.text
    .split(/\r?\n/)
    .map((l) => `  ${l}`)
    .join("\n");
  return `${header}\n${body}\n\n`;
}

export function truncate(text: string, max: number): string {
  if (!max || text.length <= max) return text;
  return `${text.slice(0, max)}… [+${text.length - max} chars]`;
}
