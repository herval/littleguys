// File edits as line diffs, from what the runtime recorded (Claude Code's own
// patch, ACP `diff` content) or else from the edit tool's arguments.
import type { ToolDiff } from "./types";

export interface DiffLine {
  sign: " " | "-" | "+";
  text: string;
  old?: number;
  new?: number;
  /** The span that changed, when this line pairs with one on the other side. */
  mark?: [number, number];
}

export interface FileDiff {
  path: string;
  /** Every line is new: a file created, or written whole. */
  created: boolean;
  /** Line numbers are the file's own (not just positions in a snippet). */
  numbered: boolean;
  hunks: DiffLine[][];
  added: number;
  removed: number;
}

const CONTEXT = 3;
/** Past this many cells, the middle of a diff shows as all removed, then all added. */
const LCS_MAX_CELLS = 2_000_000;

type Rec = Record<string, unknown>;
const str = (v: unknown): string | undefined => (typeof v === "string" ? v : undefined);

/** The diffs to show for a tool step, or none if it didn't edit a file. */
export function fileDiffs(name: string, args: unknown, recorded?: ToolDiff[], resultText?: string): FileDiff[] {
  if (recorded?.length) return recorded.map(fromRecorded).filter((d) => d.hunks.length);
  // Tools that answer with the patch they applied (e.g. `{"success": true, "diff": "--- a/…"}`).
  const applied = resultText && jsonField(resultText, "diff");
  if (applied && PATCH.test(applied)) {
    const diffs = fromPatch(applied);
    if (diffs.length) return diffs;
  }
  return fromArguments(name, args);
}

/** A string field of a tool result that is a JSON object. */
export function jsonField(text: string, key: string): string | undefined {
  if (!text.trimStart().startsWith("{")) return undefined;
  try {
    const v = (JSON.parse(text) as Rec)[key];
    return typeof v === "string" ? v : undefined;
  } catch {
    return undefined;
  }
}

function fromRecorded(d: ToolDiff): FileDiff {
  if (d.hunks?.length) return fromHunks(d.path, d.hunks);
  return fromTexts(d.path, d.oldText ?? null, d.newText ?? "");
}

/** Edit / MultiEdit / Write (Claude Code), edit / write (OpenClaw), str_replace… */
function fromArguments(name: string, raw: unknown): FileDiff[] {
  let args = raw as Rec;
  if (typeof raw === "string") {
    try {
      args = JSON.parse(raw);
    } catch {
      return [];
    }
  }
  if (!args || typeof args !== "object") return [];
  // Patch text: Codex/OpenClaw `apply_patch`, Hermes `patch` in patch mode.
  const patch = [args.patch, args.input, args.diff].find((v): v is string => typeof v === "string" && PATCH.test(v));
  if (patch) return fromPatch(patch);
  const path = str(args.file_path) ?? str(args.path) ?? str(args.filePath);
  if (!path) return [];
  const edits = Array.isArray(args.edits) ? (args.edits as Rec[]) : [args];
  const pairs = edits
    .map((e) => ({ old: str(e?.old_string) ?? str(e?.oldText) ?? str(e?.old_str), new: str(e?.new_string) ?? str(e?.newText) ?? str(e?.new_str) }))
    .filter((p) => p.old !== undefined || p.new !== undefined);
  if (pairs.length) {
    const parts = pairs.map((p) => fromTexts(path, p.old ?? "", p.new ?? ""));
    return [{ ...parts[0], hunks: parts.flatMap((p) => p.hunks), added: sum(parts, "added"), removed: sum(parts, "removed") }];
  }
  const content = str(args.content) ?? str(args.file_text);
  if (content !== undefined && /write|create/i.test(name)) return [fromTexts(path, null, content)];
  return [];
}

const sum = (ds: FileDiff[], k: "added" | "removed") => ds.reduce((n, d) => n + d[k], 0);

const PATCH = /^\*\*\* Begin Patch|^@@ -\d/m;

/** A patch as text: V4A (`*** Update File: p` … `@@` …) or unified (`--- a/p` / `+++ b/p` / `@@ -1,2 +1,3 @@`). */
export function fromPatch(patch: string): FileDiff[] {
  const files: Array<{ path: string; created: boolean; numbered: boolean; hunks: DiffLine[][] }> = [];
  let file: (typeof files)[number] | null = null;
  let hunk: DiffLine[] | null = null;
  let o = 0;
  let n = 0;
  const lines = patch.replace(/\r\n/g, "\n").split("\n");
  for (let i = 0; i < lines.length; i++) {
    const line = lines[i];
    const v4a = /^\*\*\* (Add|Update|Delete) File: (.+)$/.exec(line);
    const unified = line.startsWith("--- ") && lines[i + 1]?.startsWith("+++ ");
    if (v4a || unified) {
      const path = v4a ? v4a[2].trim() : lines[++i].slice(4).replace(/^b\//, "").split("\t")[0].trim();
      const created = v4a ? v4a[1] === "Add" : line.slice(4).trim() === "/dev/null";
      files.push((file = { path, created, numbered: created, hunks: [] }));
      hunk = null;
      o = n = 1;
      continue;
    }
    if (!file) continue;
    if (line.startsWith("*** Move to: ")) file.path = line.slice(13).trim();
    if (line.startsWith("*** ") || line.startsWith("diff ") || line.startsWith("index ")) continue;
    if (line.startsWith("@@")) {
      const at = /^@@ -(\d+)(?:,\d+)? \+(\d+)/.exec(line);
      if (at) [o, n, file.numbered] = [Number(at[1]), Number(at[2]), true];
      hunk = null;
      continue;
    }
    const sign = line[0];
    if (sign !== " " && sign !== "-" && sign !== "+") continue;
    if (!hunk) file.hunks.push((hunk = []));
    if (sign === "-") hunk.push({ sign, text: line.slice(1), old: o++ });
    else if (sign === "+") hunk.push({ sign, text: line.slice(1), new: n++ });
    else hunk.push({ sign, text: line.slice(1), old: o++, new: n++ });
  }
  return files
    .filter((f) => f.hunks.length)
    .map((f) => {
      const all = f.hunks.flat();
      return { ...f, hunks: f.hunks.map(markPairs), added: all.filter((l) => l.sign === "+").length, removed: all.filter((l) => l.sign === "-").length };
    });
}

function fromHunks(path: string, hunks: NonNullable<ToolDiff["hunks"]>): FileDiff {
  let added = 0;
  let removed = 0;
  const out = hunks.map((h) => {
    let o = h.oldStart;
    let n = h.newStart;
    const lines: DiffLine[] = [];
    for (const raw of h.lines ?? []) {
      const sign = raw[0];
      const text = raw.slice(1);
      if (sign === "-") {
        lines.push({ sign, text, old: o++ });
        removed++;
      } else if (sign === "+") {
        lines.push({ sign, text, new: n++ });
        added++;
      } else if (sign === " " || raw === "") {
        lines.push({ sign: " ", text, old: o++, new: n++ });
      } // "\ No newline at end of file"
    }
    return markPairs(lines);
  });
  return { path, created: false, numbered: true, hunks: out.filter((l) => l.length), added, removed };
}

function fromTexts(path: string, oldText: string | null, newText: string): FileDiff {
  const b = splitLines(newText);
  if (oldText === null) {
    const lines = b.map((text, i): DiffLine => ({ sign: "+", text, new: i + 1 }));
    return { path, created: true, numbered: true, hunks: lines.length ? [lines] : [], added: lines.length, removed: 0 };
  }
  const a = splitLines(oldText);
  const ops = diffLines(a, b);
  let o = 1;
  let n = 1;
  const numbered = ops.map((op): DiffLine => {
    if (op.sign === "-") return { ...op, old: o++ };
    if (op.sign === "+") return { ...op, new: n++ };
    return { ...op, old: o++, new: n++ };
  });
  return {
    path,
    created: false,
    // An edit tool's old/new strings are snippets: their positions aren't the file's lines.
    numbered: false,
    hunks: withContext(numbered).map(markPairs),
    added: ops.filter((op) => op.sign === "+").length,
    removed: ops.filter((op) => op.sign === "-").length,
  };
}

function splitLines(text: string): string[] {
  if (!text) return [];
  const lines = text.replace(/\r\n/g, "\n").split("\n");
  if (lines[lines.length - 1] === "") lines.pop();
  return lines;
}

type Op = { sign: " " | "-" | "+"; text: string };

/** Line diff: common head and tail trimmed, LCS on the middle, removals before additions. */
export function diffLines(a: string[], b: string[]): Op[] {
  let head = 0;
  while (head < a.length && head < b.length && a[head] === b[head]) head++;
  let endA = a.length;
  let endB = b.length;
  while (endA > head && endB > head && a[endA - 1] === b[endB - 1]) {
    endA--;
    endB--;
  }
  const ops: Op[] = a.slice(0, head).map((text) => ({ sign: " ", text }));
  const n = endA - head;
  const m = endB - head;
  const middle: Op[] = [];
  if (n && m && n * m <= LCS_MAX_CELLS) {
    // lcs[i][j]: longest common run of a[head+i..endA) and b[head+j..endB).
    const w = m + 1;
    const lcs = new Uint32Array((n + 1) * w);
    for (let i = n - 1; i >= 0; i--) {
      for (let j = m - 1; j >= 0; j--) {
        lcs[i * w + j] = a[head + i] === b[head + j] ? lcs[(i + 1) * w + j + 1] + 1 : Math.max(lcs[(i + 1) * w + j], lcs[i * w + j + 1]);
      }
    }
    let i = 0;
    let j = 0;
    while (i < n && j < m) {
      if (a[head + i] === b[head + j]) {
        middle.push({ sign: " ", text: a[head + i] });
        i++;
        j++;
      } else if (lcs[(i + 1) * w + j] >= lcs[i * w + j + 1]) middle.push({ sign: "-", text: a[head + i++] });
      else middle.push({ sign: "+", text: b[head + j++] });
    }
    while (i < n) middle.push({ sign: "-", text: a[head + i++] });
    while (j < m) middle.push({ sign: "+", text: b[head + j++] });
  } else {
    for (let i = head; i < endA; i++) middle.push({ sign: "-", text: a[i] });
    for (let j = head; j < endB; j++) middle.push({ sign: "+", text: b[j] });
  }
  // Within each changed run, all removals first, then all additions.
  for (let k = 0; k < middle.length; ) {
    if (middle[k].sign === " ") {
      ops.push(middle[k++]);
      continue;
    }
    const run: Op[] = [];
    while (k < middle.length && middle[k].sign !== " ") run.push(middle[k++]);
    ops.push(...run.filter((op) => op.sign === "-"), ...run.filter((op) => op.sign === "+"));
  }
  for (let i = endA; i < a.length; i++) ops.push({ sign: " ", text: a[i] });
  return ops;
}

/** Changed lines with up to CONTEXT unchanged lines around them, in separate hunks. */
function withContext(lines: DiffLine[]): DiffLine[][] {
  const keep = new Array<boolean>(lines.length).fill(false);
  lines.forEach((l, i) => {
    if (l.sign === " ") return;
    for (let k = Math.max(0, i - CONTEXT); k <= Math.min(lines.length - 1, i + CONTEXT); k++) keep[k] = true;
  });
  const hunks: DiffLine[][] = [];
  let current: DiffLine[] | null = null;
  lines.forEach((l, i) => {
    if (!keep[i]) {
      current = null;
      return;
    }
    if (!current) hunks.push((current = []));
    current.push(l);
  });
  return hunks;
}

/** Where a run of removed lines meets as many added ones, marks what changed on each pair. */
function markPairs(lines: DiffLine[]): DiffLine[] {
  for (let i = 0; i < lines.length; ) {
    if (lines[i].sign !== "-") {
      i++;
      continue;
    }
    let j = i;
    while (j < lines.length && lines[j].sign === "-") j++;
    let k = j;
    while (k < lines.length && lines[k].sign === "+") k++;
    if (k - j === j - i) {
      for (let p = 0; p < j - i; p++) markPair(lines[i + p], lines[j + p]);
    }
    i = k;
  }
  return lines;
}

function markPair(del: DiffLine, add: DiffLine) {
  const a = del.text;
  const b = add.text;
  let pre = 0;
  while (pre < a.length && pre < b.length && a[pre] === b[pre]) pre++;
  let suf = 0;
  while (suf < a.length - pre && suf < b.length - pre && a[a.length - 1 - suf] === b[b.length - 1 - suf]) suf++;
  // Lines with little in common read better as plain removed/added.
  if (pre + suf < Math.min(a.length, b.length) / 3) return;
  del.mark = [pre, a.length - suf];
  add.mark = [pre, b.length - suf];
}
