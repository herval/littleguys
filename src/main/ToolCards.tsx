// Tool calls whose arguments mean something to a person, shown as such: a
// question and what you picked, a to-do list, a plan. Anything else lists its
// arguments by name. Shapes, not tool names, decide (Claude Code's
// AskUserQuestion / TodoWrite / ExitPlanMode, Hermes' clarify / todo, …).
import { Circle, CircleCheck, CircleDot, PenLine, Square, SquareCheck } from "lucide-react";
import { Fragment, type ReactNode } from "react";
import type { ToolResult } from "../lib/transcript";
import { Markdown } from "../ui/Markdown";

type Rec = Record<string, unknown>;
const isRec = (v: unknown): v is Rec => Boolean(v) && typeof v === "object" && !Array.isArray(v);
const str = (v: unknown): string | undefined => (typeof v === "string" && v.trim() ? v : undefined);

/** A card for this call, and whether it already shows what the result says. */
export function toolCard(name: string, args: Rec, result?: ToolResult): { node: ReactNode; showsResult: boolean } | null {
  const questions = questionsOf(args);
  if (questions.length) {
    const answers = questions.map((q) => answerTo(q.question, result));
    return { node: <Questions questions={questions} answers={answers} />, showsResult: answers.some((a) => a !== undefined) };
  }
  if (Array.isArray(args.todos) && args.todos.every((t) => isRec(t) && str(t.content))) {
    return { node: <Todos todos={args.todos as Rec[]} />, showsResult: true };
  }
  const plan = str(args.plan);
  if (plan && /plan/i.test(name)) {
    return {
      node: (
        <div className="max-h-96 overflow-auto px-3 py-2 text-[12.5px]">
          <Markdown text={plan} />
        </div>
      ),
      showsResult: true,
    };
  }
  return null;
}

// ---- questions ------------------------------------------------------------------

type Question = { question: string; header?: string; multi: boolean; options: Array<{ label: string; description?: string }> };

function questionsOf(args: Rec): Question[] {
  const raw = Array.isArray(args.questions) ? args.questions : str(args.question) ? [args] : [];
  return raw.filter(isRec).flatMap((q) => {
    const question = str(q.question);
    if (!question) return [];
    const options = (Array.isArray(q.options) ? q.options : Array.isArray(q.choices) ? q.choices : [])
      .map((o) => (typeof o === "string" ? { label: o } : isRec(o) && str(o.label) ? { label: String(o.label), description: str(o.description) } : null))
      .filter((o): o is { label: string; description?: string } => Boolean(o));
    return [{ question, header: str(q.header), multi: q.multiSelect === true, options }];
  });
}

/**
 * The answer to one question: recorded by the runtime, else read from the
 * result text (`…: "Question"="Answer", "Other question"="…". You can now…`).
 */
function answerTo(question: string, result?: ToolResult): string | undefined {
  if (!result || result.isError) return undefined;
  const recorded = result.answers?.[question];
  if (recorded !== undefined) return recorded;
  const marker = `"${question}"="`;
  const at = result.text.indexOf(marker);
  if (at < 0) return undefined;
  const rest = result.text.slice(at + marker.length);
  const end = rest.search(/"(?=, "|\.\s| selected|\.?$)/);
  return end >= 0 ? rest.slice(0, end) : undefined;
}

function Questions({ questions, answers }: { questions: Question[]; answers: Array<string | undefined> }) {
  return (
    <div className="space-y-3 px-3 py-2.5">
      {questions.map((q, i) => {
        const answer = answers[i];
        const picked = (label: string) => answer !== undefined && (answer === label || (q.multi && answer.split(", ").includes(label)));
        const typed = answer !== undefined && !q.options.some((o) => picked(o.label)) ? answer : undefined;
        const [On, Off] = q.multi ? [SquareCheck, Square] : [CircleCheck, Circle];
        return (
          <div key={i}>
            {q.header && <div className="mb-0.5 text-[10.5px] font-medium uppercase tracking-wide text-[var(--color-ink-3)]">{q.header}</div>}
            <div className="text-[12.5px] font-medium text-[var(--color-ink)]">{q.question}</div>
            <ul className="mt-1.5 space-y-0.5">
              {q.options.map((o, j) => {
                const on = picked(o.label);
                const Icon = on ? On : Off;
                return (
                  <li key={j} className={`flex gap-2 rounded-md px-2 py-1 ${on ? "bg-[var(--color-accent-soft)]" : ""}`}>
                    <Icon size={13} className={`mt-[2px] shrink-0 ${on ? "text-[var(--color-accent-strong)]" : "text-[var(--color-ink-3)]"}`} />
                    <div className="min-w-0">
                      <div className={on ? "font-medium text-[var(--color-ink)]" : "text-[var(--color-ink-2)]"}>{o.label}</div>
                      {o.description && <div className="text-[11.5px] text-[var(--color-ink-3)]">{o.description}</div>}
                    </div>
                  </li>
                );
              })}
              {typed !== undefined && (
                <li className="flex gap-2 rounded-md bg-[var(--color-accent-soft)] px-2 py-1">
                  <PenLine size={13} className="mt-[2px] shrink-0 text-[var(--color-accent-strong)]" />
                  <div className="min-w-0 whitespace-pre-wrap font-medium text-[var(--color-ink)]">{typed}</div>
                </li>
              )}
            </ul>
          </div>
        );
      })}
    </div>
  );
}

// ---- to-dos ---------------------------------------------------------------------

function Todos({ todos }: { todos: Rec[] }) {
  return (
    <ul className="space-y-1 px-3 py-2.5 text-[12px]">
      {todos.map((t, i) => {
        const status = String(t.status ?? "pending");
        const done = status === "completed" || status === "done";
        const doing = status === "in_progress" || status === "active";
        const Icon = done ? CircleCheck : doing ? CircleDot : Circle;
        return (
          <li key={i} className="flex gap-2">
            <Icon
              size={13}
              className={`mt-[2px] shrink-0 ${done ? "text-[var(--color-ok)]" : doing ? "text-[var(--color-accent-strong)]" : "text-[var(--color-ink-3)]"}`}
            />
            <span className={done ? "text-[var(--color-ink-3)] line-through" : doing ? "font-medium text-[var(--color-ink)]" : "text-[var(--color-ink-2)]"}>
              {String(t.content)}
            </span>
          </li>
        );
      })}
    </ul>
  );
}

// ---- anything else: arguments by name ------------------------------------------------

/** A call's arguments as name / value pairs, nested ones indented (not JSON). */
export function ArgList({ args }: { args: Rec | string | null }) {
  if (!args) return null;
  if (typeof args === "string") {
    return <pre className="max-h-40 overflow-auto whitespace-pre-wrap bg-[var(--color-chip)] p-2.5 font-mono text-[var(--color-ink-2)] [overflow-wrap:anywhere]">{args}</pre>;
  }
  if (!Object.values(args).some(present)) return null;
  return (
    <div className="max-h-64 overflow-auto bg-[var(--color-chip)] px-2.5 py-2">
      <Fields obj={args} depth={0} />
    </div>
  );
}

const present = (v: unknown) => v !== undefined && v !== null && v !== "";

function Fields({ obj, depth }: { obj: Rec; depth: number }) {
  return (
    <div className="grid grid-cols-[auto_minmax(0,1fr)] gap-x-3 gap-y-0.5">
      {Object.entries(obj)
        .filter(([, v]) => present(v))
        .map(([k, v]) => (
          <Fragment key={k}>
            <span className="text-[var(--color-ink-3)]">{k}</span>
            <div className="min-w-0 text-[var(--color-ink-2)]">
              <Value v={v} depth={depth} name={k} />
            </div>
          </Fragment>
        ))}
    </div>
  );
}

/** Arguments that hold code or a schedule, whatever they look like. */
const CODE_KEYS = /^(command|cmd|code|script|pattern|regex|glob|jq|sql|cron|schedule|selector|path|file_?path|url|patch|diff)$/i;

function Value({ v, depth, name = "" }: { v: unknown; depth: number; name?: string }): ReactNode {
  if (typeof v === "string") {
    // Paths, URLs, ids and code read better in mono; sentences don't.
    const code = CODE_KEYS.test(name) || !/\s/.test(v.trim());
    return <span className={`whitespace-pre-wrap [overflow-wrap:anywhere] ${code ? "font-mono" : ""}`}>{v}</span>;
  }
  if (v === null || typeof v !== "object") return <span className="font-mono">{String(v)}</span>;
  if (depth >= 3) return <span className="font-mono [overflow-wrap:anywhere]">{JSON.stringify(v)}</span>;
  if (Array.isArray(v)) {
    if (!v.length) return <span className="text-[var(--color-ink-3)]">none</span>;
    if (v.every((x) => x === null || typeof x !== "object")) return <span className="[overflow-wrap:anywhere]">{v.map(String).join(", ")}</span>;
    return (
      <div className="space-y-1">
        {v.map((x, i) => (
          <div key={i} className="border-l-2 border-[var(--color-line)] pl-2">
            <Value v={x} depth={depth + 1} name={name} />
          </div>
        ))}
      </div>
    );
  }
  return <Fields obj={v as Rec} depth={depth + 1} />;
}
