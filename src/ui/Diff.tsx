// A file's changes as a unified diff, shared by the transcript's edit cards and the
// Changes panel (src/main/ChangesPanel.tsx).
import { Fragment, useState } from "react";
import type { DiffLine, FileDiff } from "../lib/diff";

export function DiffStat({ added, removed }: { added: number; removed: number }) {
  return (
    <span className="shrink-0 font-mono text-[11px] tabular-nums">
      {added > 0 && <span className="text-[var(--color-ok)]">+{added}</span>}
      {added > 0 && removed > 0 && " "}
      {removed > 0 && <span className="text-[var(--color-bad)]">−{removed}</span>}
    </span>
  );
}

const DIFF_LINES = 40;

/**
 * One file's changes, unified: removed lines in red, added in green, what changed within a line marked.
 * `onLine`: lines can be clicked (to quote one in the composer).
 */
export function DiffView({ diff, header, onLine, lines: max = DIFF_LINES }: { diff: FileDiff; header: boolean; onLine?: (line: DiffLine) => void; lines?: number }) {
  const [all, setAll] = useState(false);
  const total = diff.hunks.reduce((n, h) => n + h.length, 0);
  const clamped = total > max + 3;
  let budget = all || !clamped ? Infinity : max;
  const hunks = diff.hunks.map((h) => {
    const shown = h.slice(0, Math.max(0, budget));
    budget -= shown.length;
    return shown;
  });
  const lastNo = diff.hunks.flat().reduce((n, l) => Math.max(n, l.old ?? 0, l.new ?? 0), 0);
  const gutter = diff.numbered ? `${String(lastNo).length + 1}ch` : undefined;
  return (
    <div className="font-mono text-[11px] leading-[1.6] [&+&]:border-t [&+&]:border-[var(--color-line)]">
      {header && (
        <div className="truncate border-b border-[var(--color-line)] bg-[var(--color-chip)] px-2.5 py-1 text-[var(--color-ink-2)]">
          {diff.path.replace(/^\/Users\/[^/]+/, "~")}
          {diff.created && <span className="text-[var(--color-ink-3)]"> · new</span>}
        </div>
      )}
      <div className="py-1">
        {hunks.map(
          (lines, i) =>
            lines.length > 0 && (
              <Fragment key={i}>
                {i > 0 && <div className="select-none px-2.5 text-[var(--color-ink-3)]">⋯</div>}
                {lines.map((line, j) => (
                  <DiffRow key={j} line={line} gutter={gutter} onClick={onLine && (() => onLine(line))} />
                ))}
              </Fragment>
            ),
        )}
        {clamped && (
          <button onClick={() => setAll(!all)} className="px-2.5 pt-1 font-sans text-[11px] text-[var(--color-ink-3)] hover:text-[var(--color-ink)]">
            {all ? "Show less" : `Show all ${total} lines`}
          </button>
        )}
      </div>
    </div>
  );
}

const DIFF_ROW = { " ": "", "-": "bg-[var(--color-diff-del)]", "+": "bg-[var(--color-diff-add)]" };
const DIFF_MARK = { " ": "", "-": "bg-[var(--color-diff-del-mark)]", "+": "bg-[var(--color-diff-add-mark)]" };
const DIFF_SIGN = { " ": "", "-": "text-[var(--color-bad)]", "+": "text-[var(--color-ok)]" };

export function DiffRow({ line, gutter, onClick }: { line: DiffLine; gutter?: string; onClick?: () => void }) {
  const { sign, text, mark } = line;
  return (
    <div
      onClick={onClick}
      title={onClick ? "Comment on this line" : undefined}
      className={`flex min-h-[1.6em] ${DIFF_ROW[sign]} ${onClick ? "cursor-pointer hover:shadow-[inset_2px_0_0_var(--color-accent)] hover:brightness-95 dark:hover:brightness-125" : ""}`}
    >
      {gutter && (
        <span style={{ width: gutter }} className="shrink-0 select-none text-right text-[var(--color-ink-3)] opacity-70">
          {sign === "-" ? line.old : line.new}
        </span>
      )}
      <span className={`w-5 shrink-0 select-none text-center ${DIFF_SIGN[sign]}`}>{sign === " " ? "" : sign === "-" ? "−" : "+"}</span>
      <span className="min-w-0 flex-1 whitespace-pre-wrap pr-2.5 text-[var(--color-ink)] [overflow-wrap:anywhere]">
        {mark && mark[1] > mark[0] ? (
          <>
            {text.slice(0, mark[0])}
            <span className={`rounded-[3px] ${DIFF_MARK[sign]}`}>{text.slice(mark[0], mark[1])}</span>
            {text.slice(mark[1])}
          </>
        ) : (
          text
        )}
      </span>
    </div>
  );
}
