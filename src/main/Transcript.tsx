import {
  Brain,
  Check,
  ChevronRight,
  Clock,
  File as FileIcon,
  FilePen,
  FileText,
  Film,
  Folder,
  Globe,
  ImageOff,
  Maximize2,
  Music,
  Search,
  Send,
  Sparkles,
  Terminal,
  Wrench,
  X,
} from "lucide-react";
import { memo, useEffect, useRef, useState } from "react";
import { artifacts } from "../lib/gateway";
import { gatewayOf } from "../lib/guys";
import { core, errorMessage } from "../lib/ipc";
import { presentTense, summarizeSteps, type ToolKind, toolKind } from "../lib/toolPhrases";
import { cleanDirectives, extractMediaLines, type Item, type MediaRef, type ToolCallBlock, type ToolResult } from "../lib/transcript";
import type { AgentSummary } from "../lib/types";
export { toItems } from "../lib/transcript";
import { agentIdFromKey, useApp } from "../stores/app";
import type { LiveRun, ToolState } from "../stores/chat";
import { Avatar } from "../ui/Avatar";
import { Lightbox } from "../ui/Lightbox";
import { TypingDots } from "../ui/kit";
import { Markdown } from "../ui/Markdown";
import { type Save, saveIcon, saveLabel, saveTitle, useSave } from "../ui/useSave";

// ---- rendering --------------------------------------------------------------

const DAY_MS = 86_400_000;

function dayKey(ts: number): string {
  const d = new Date(ts);
  return `${d.getFullYear()}-${d.getMonth()}-${d.getDate()}`;
}

/** "Today", "Yesterday", "Monday", "Mon, Sep 29", "Sep 29, 2025". */
function dayLabel(ts: number): string {
  const d = new Date(ts);
  const startOfToday = new Date().setHours(0, 0, 0, 0);
  const startOfDay = new Date(ts).setHours(0, 0, 0, 0);
  const daysAgo = Math.round((startOfToday - startOfDay) / DAY_MS);
  if (daysAgo === 0) return "Today";
  if (daysAgo === 1) return "Yesterday";
  if (daysAgo > 1 && daysAgo < 7) return d.toLocaleDateString(undefined, { weekday: "long" });
  if (d.getFullYear() === new Date().getFullYear()) return d.toLocaleDateString(undefined, { weekday: "short", month: "short", day: "numeric" });
  return d.toLocaleDateString(undefined, { month: "short", day: "numeric", year: "numeric" });
}

/** Sticks to the top of the conversation while its day scrolls past. */
function DayChip({ label }: { label: string }) {
  return (
    <div className="pointer-events-none sticky top-1 z-10 my-3 flex justify-center">
      <span className="rounded-full bg-[var(--color-pane-solid)]/90 px-3 py-1 text-[11.5px] font-medium text-[var(--color-ink-2)] shadow-[0_1px_4px_rgb(0_0_0/0.08)] ring-1 ring-[var(--color-line)] backdrop-blur">
        {label}
      </span>
    </div>
  );
}

type Step = Extract<Item, { kind: "tool" | "thinking" }>;
type Section = { day: string; label: string; nodes: React.ReactNode[] };

export function Transcript({
  gatewayId,
  sessionKey,
  items,
  agent,
  live,
  waiting = 0,
  toolStatus,
}: {
  gatewayId: string;
  sessionKey: string;
  items: Item[];
  agent: AgentSummary;
  live: LiveRun | null;
  /** The last `waiting` items are messages queued behind the reply in progress. */
  waiting?: number;
  toolStatus: Record<string, ToolState>;
}) {
  const sections: Section[] = [];
  let prevKind: string | null = null;
  let lastTs = 0;
  const where = { gatewayId, sessionKey };

  // Items are grouped by day so each day's chip can stick while it scrolls by.
  const sectionFor = (ts: number): Section => {
    const current = sections[sections.length - 1];
    if (!ts) {
      if (current) return current;
      sections.push({ day: "", label: "", nodes: [] });
      return sections[0];
    }
    const day = dayKey(ts);
    if (current && (current.day === day || !current.day)) {
      if (!current.day) Object.assign(current, { day, label: dayLabel(ts) });
      return current;
    }
    sections.push({ day, label: dayLabel(ts), nodes: [] });
    prevKind = null;
    return sections[sections.length - 1];
  };

  // Messages sent while the guy is still replying go below that reply.
  const queued = live && waiting ? (items.slice(-waiting) as Array<Extract<Item, { kind: "user" }>>) : [];
  if (queued.length) items = items.slice(0, -queued.length);

  for (let i = 0; i < items.length; i++) {
    const item = items[i];
    const ts = "ts" in item && item.ts ? item.ts : 0;
    const section = sectionFor(ts);
    if (ts && lastTs && ts - lastTs > 30 * 60_000 && dayKey(ts) === dayKey(lastTs)) {
      section.nodes.push(
        <div key={`ts-${item.id}`} className="py-2 text-center text-[11px] text-[var(--color-ink-3)]">
          {new Date(ts).toLocaleTimeString([], { hour: "numeric", minute: "2-digit" })}
        </div>,
      );
      prevKind = null;
    }
    if (ts) lastTs = ts;
    const side = item.kind === "user" ? `user:${item.sender ?? ""}` : item.kind === "note" ? null : "guy";
    const startsGroup = side !== prevKind;
    prevKind = side;

    if (item.kind === "user") {
      section.nodes.push(<UserBubble key={item.id} item={item} first={startsGroup} where={where} />);
    } else if (item.kind === "note") {
      section.nodes.push(
        <p
          key={item.id}
          className={`mx-auto my-2 max-w-[80%] text-center text-[11.5px] ${
            item.tone === "error" ? "text-[var(--color-bad)]" : "text-[var(--color-ink-3)]"
          }`}
        >
          {item.text}
        </p>,
      );
    } else if (item.kind === "tool" || item.kind === "thinking") {
      // Back-to-back steps fold into one line; the reply stays the focus.
      const steps: Step[] = [item];
      while (i + 1 < items.length && (items[i + 1].kind === "tool" || items[i + 1].kind === "thinking")) steps.push(items[++i] as Step);
      section.nodes.push(
        <GuyLine key={item.id} agent={agent} showAvatar={startsGroup}>
          <WorkLog steps={steps} toolStatus={toolStatus} live={Boolean(live)} />
        </GuyLine>,
      );
    } else {
      section.nodes.push(
        <GuyLine key={item.id} agent={agent} showAvatar={startsGroup}>
          {item.kind === "text" && (
            <div className="rounded-2xl rounded-tl-md bg-[var(--color-bubble-them)] px-3.5 py-2">
              <Markdown text={item.text} />
            </div>
          )}
          {item.kind === "media" && <MediaView media={item.media} where={where} />}
        </GuyLine>,
      );
    }
  }

  if (live) {
    const section = sectionFor(Date.now());
    const startsGroup = prevKind !== "guy";
    section.nodes.push(
      <GuyLine key="live" agent={agent} showAvatar={startsGroup}>
        {live.text ? (
          <div className="rounded-2xl rounded-tl-md bg-[var(--color-bubble-them)] px-3.5 py-2">
            <Markdown text={extractMediaLines(cleanDirectives(live.text)).text} />
            <span className="ml-0.5 inline-block h-3.5 w-[2px] translate-y-0.5 animate-pulse bg-[var(--color-accent)]" />
          </div>
        ) : (
          <div className="flex items-center gap-2 rounded-2xl rounded-tl-md bg-[var(--color-bubble-them)] px-3.5 py-2.5 text-[var(--color-ink-3)]">
            <TypingDots />
            {live.phase && <span className="text-[11.5px]">{phaseLabel(live.phase)}</span>}
          </div>
        )}
      </GuyLine>,
    );
    queued.forEach((item, i) => {
      sectionFor(Date.now()).nodes.push(<UserBubble key={item.id} item={item} first={i === 0} where={where} />);
    });
  }
  return (
    <>
      {sections.map((s, i) => (
        <section key={`${s.day}-${i}`}>
          {s.label && <DayChip label={s.label} />}
          {s.nodes}
        </section>
      ))}
    </>
  );
}

export function phaseLabel(phase: string): string {
  const labels: Record<string, string> = {
    sending: "sending…",
    preparing_workspace: "getting ready…",
    preparing_context: "reading up…",
    starting_model: "thinking…",
    queued: "waiting in line…",
  };
  return labels[phase] ?? phase.replace(/_/g, " ") + "…";
}

function GuyLine({ agent, showAvatar, children }: { agent: AgentSummary; showAvatar: boolean; children: React.ReactNode }) {
  return (
    <div className={`flex items-start gap-2 pr-12 ${showAvatar ? "mt-3" : "mt-1"}`}>
      <div className="w-7 shrink-0">{showAvatar && <Avatar agent={agent} size={28} />}</div>
      <div className="min-w-0 max-w-[720px]">{children}</div>
    </div>
  );
}

type Where = { gatewayId: string; sessionKey: string };

const UserBubble = memo(function UserBubble({ item, first, where }: { item: Extract<Item, { kind: "user" }>; first: boolean; where: Where }) {
  return (
    <div className={`flex justify-end pl-16 ${first ? "mt-3" : "mt-1"} ${item.pending ? "opacity-60" : ""}`}>
      <div className="flex min-w-0 max-w-[640px] flex-col items-end gap-1">
        {first && item.sender && <span className="px-1 text-[11px] text-[var(--color-ink-3)]">{item.sender}</span>}
        {item.media.map((m, i) => (
          <MediaView key={i} media={m} where={where} />
        ))}
        {item.text && (
          <div className="bubble-me max-w-full rounded-2xl rounded-tr-md bg-[var(--color-bubble-me)] px-3.5 py-2 text-white">
            <Markdown text={item.text} />
          </div>
        )}
      </div>
    </div>
  );
});

function Thinking({ text }: { text: string }) {
  const [open, setOpen] = useState(false);
  const summary = text.replace(/\*\*/g, "").split("\n")[0].slice(0, 120);
  return (
    <button
      onClick={() => setOpen(!open)}
      className="flex max-w-full items-start gap-1.5 rounded-md px-1 py-0.5 text-left text-[12px] italic text-[var(--color-ink-3)] hover:text-[var(--color-ink-2)]"
    >
      <Sparkles size={12} className="mt-0.5 shrink-0" />
      <span className={open ? "selectable whitespace-pre-wrap not-italic" : "truncate"}>{open ? text : summary}</span>
    </button>
  );
}

// ---- tool steps ---------------------------------------------------------------

const KIND_ICON: Record<ToolKind, typeof Globe> = {
  browse: Globe,
  fetch: Globe,
  websearch: Search,
  search: Search,
  exec: Terminal,
  edit: FilePen,
  read: FileText,
  list: Folder,
  memory: Brain,
  routine: Clock,
  message: Send,
  other: Wrench,
};

function toolSummary(call: ToolCallBlock): string {
  let args = (call.arguments ?? {}) as Record<string, unknown>;
  if (typeof call.arguments === "string") {
    try {
      args = JSON.parse(call.arguments);
    } catch {
      return call.arguments.slice(0, 80);
    }
  }
  const pick = ["url", "command", "cmd", "query", "path", "file_path", "filePath", "pattern", "description", "skill", "name", "text"]
    .map((k) => args[k])
    .find((v) => typeof v === "string" && v.trim()) as string | undefined;
  const subject = pick ? (Array.isArray(args.command) ? (args.command as string[]).join(" ") : pick) : "";
  const pretty = subject.replace(/^\/Users\/[^/]+/, "~").replace(/\s+/g, " ");
  return pretty.length > 80 ? `${pretty.slice(0, 77)}…` : pretty;
}

function stepState(step: Extract<Item, { kind: "tool" }>, toolStatus: Record<string, ToolState>, live: boolean): ToolState {
  if (step.result) return step.result.isError ? "error" : "completed";
  // No result and nothing running: the transcript just didn't keep it.
  return toolStatus[step.call.id] ?? (live ? "running" : "completed");
}

/** A run of tool calls (and thinking) as one quiet line: "Searched the web and browsed a.com". */
function WorkLog({ steps, toolStatus, live }: { steps: Step[]; toolStatus: Record<string, ToolState>; live: boolean }) {
  const [open, setOpen] = useState(false);
  const tools = steps.filter((s): s is Extract<Item, { kind: "tool" }> => s.kind === "tool");
  const states = tools.map((t) => stepState(t, toolStatus, live));
  const runningIdx = states.lastIndexOf("running");
  const failed = states.filter((st) => st === "error").length;
  const lead = runningIdx >= 0 ? tools[runningIdx] : tools[0];
  const Icon = lead ? KIND_ICON[toolKind(lead.call.name)] : Sparkles;
  const text =
    runningIdx >= 0
      ? `${presentTense(tools[runningIdx].call).replace(/^./, (c) => c.toUpperCase())}…`
      : tools.length
        ? summarizeSteps(tools.map((t) => t.call))
        : "Thought it over";

  return (
    <div className="max-w-full">
      <button
        onClick={() => setOpen(!open)}
        className="group flex max-w-full items-center gap-2 rounded-full py-0.5 pl-0.5 pr-2 text-left text-[12.5px] text-[var(--color-ink-3)] transition-colors hover:bg-[var(--color-chip)] hover:text-[var(--color-ink-2)]"
      >
        <span
          className={`flex h-[22px] w-[22px] shrink-0 items-center justify-center rounded-full ${
            runningIdx >= 0 ? "bg-[var(--color-accent-soft)] text-[var(--color-accent-strong)]" : "bg-[var(--color-chip)]"
          }`}
        >
          <Icon size={12} />
        </span>
        <span className="truncate">{text}</span>
        {runningIdx >= 0 && <TypingDots className="text-[var(--color-accent)]" />}
        {failed > 0 && <span className="shrink-0 text-[var(--color-bad)]">{failed === 1 ? "1 step failed" : `${failed} steps failed`}</span>}
        <ChevronRight size={12} className={`shrink-0 opacity-50 transition-transform ${open ? "rotate-90" : ""}`} />
      </button>
      {open && (
        <div className="ml-[11px] mt-1 space-y-0.5 border-l border-[var(--color-line)] pb-1 pl-3">
          {steps.map((step) =>
            step.kind === "thinking" ? (
              <Thinking key={step.id} text={step.text} />
            ) : (
              <ToolStep key={step.id} call={step.call} result={step.result} state={stepState(step, toolStatus, live)} />
            ),
          )}
        </div>
      )}
    </div>
  );
}

function ToolStep({ call, result, state }: { call: ToolCallBlock; result?: ToolResult; state: ToolState }) {
  const [open, setOpen] = useState(false);
  const summary = toolSummary(call);
  const Icon = KIND_ICON[toolKind(call.name)];
  return (
    <div className="max-w-full">
      <button
        onClick={() => setOpen(!open)}
        className="flex max-w-full items-center gap-1.5 rounded-md px-1 py-0.5 text-left text-[12px] text-[var(--color-ink-2)] hover:bg-[var(--color-chip)]"
      >
        <Icon size={12} className="shrink-0 text-[var(--color-ink-3)]" />
        <span className="shrink-0 font-medium text-[var(--color-ink)]">{call.name}</span>
        {summary && <span className="truncate font-mono text-[11px] text-[var(--color-ink-3)]">{summary}</span>}
        {state === "running" ? (
          <TypingDots className="ml-0.5 text-[var(--color-accent)]" />
        ) : state === "error" ? (
          <X size={12} className="shrink-0 text-[var(--color-bad)]" />
        ) : (
          <Check size={12} className="shrink-0 text-[var(--color-ok)]" />
        )}
      </button>
      {open && (
        <div className="selectable mb-1 ml-1 mt-1 overflow-hidden rounded-lg border border-[var(--color-line)] text-[11.5px]">
          <pre className="max-h-40 overflow-auto bg-[var(--color-chip)] p-2.5 font-mono text-[var(--color-ink-2)]">
            {typeof call.arguments === "string" ? call.arguments : JSON.stringify(call.arguments ?? {}, null, 2)}
          </pre>
          {result && (
            <pre
              className={`max-h-64 overflow-auto whitespace-pre-wrap border-t border-[var(--color-line)] p-2.5 font-mono ${
                result.isError ? "text-[var(--color-bad)]" : "text-[var(--color-ink-2)]"
              }`}
            >
              {result.text.slice(0, 20_000) || "(no output)"}
            </pre>
          )}
        </div>
      )}
    </div>
  );
}

// ---- media & files ------------------------------------------------------------------

function gatewayHttpOrigin(url: string | null | undefined): string | null {
  if (!url) return null;
  try {
    return new URL(url.replace(/^ws(s?):/, "http$1:")).origin;
  } catch {
    return null;
  }
}

type SessionArtifact = { id: string; type?: string; mimeType?: string; title?: string; fileName?: string; sizeBytes?: number };

/** Per-thread artifact index (to match `MEDIA:` file names to stored files). */
const artifactIndex = new Map<string, Promise<SessionArtifact[]>>();
function sessionArtifacts(gatewayId: string, sessionKey: string): Promise<SessionArtifact[]> {
  const key = `${gatewayId}|${sessionKey}`;
  let p = artifactIndex.get(key);
  if (!p) {
    p = import("../lib/ipc")
      .then(({ gwCall }) => gwCall<{ artifacts?: SessionArtifact[] }>(gatewayId, "artifacts.list", { sessionKey }))
      .then((r) => r.artifacts ?? [])
      .catch(() => []);
    artifactIndex.set(key, p);
    setTimeout(() => artifactIndex.delete(key), 60_000);
  }
  return p;
}

/** Short-lived download URLs, cached until they expire. */
const urlCache = new Map<string, { url: string; until: number }>();
async function resolveArtifactUrl(gatewayId: string, sessionKey: string, artifactId: string, origin: string | null): Promise<string> {
  const key = `${gatewayId}|${artifactId}`;
  const hit = urlCache.get(key);
  if (hit && hit.until > Date.now()) return hit.url;
  const res = await artifacts.download(gatewayId, sessionKey, artifactId);
  let url: string;
  if (res.data) url = `data:${res.artifact?.mimeType ?? "application/octet-stream"};base64,${res.data}`;
  else if (res.url) url = /^(https?:|data:|blob:)/.test(res.url) ? res.url : origin ? origin + res.url : res.url;
  else throw new Error("The gateway didn't return the file.");
  const expires = (res as { expiresAt?: string }).expiresAt;
  urlCache.set(key, { url, until: expires ? Date.parse(expires) - 15_000 : Date.now() + 120_000 });
  return url;
}

/** `MEDIA:/path` files served by the gateway itself, cached until their ticket runs out. */
type PathMedia = Awaited<ReturnType<typeof core.media>>;
const pathCache = new Map<string, { result: Promise<PathMedia>; until: number }>();
function pathMedia(gatewayId: string, source: string, sessionKey: string): Promise<PathMedia> {
  const key = `${gatewayId}|${sessionKey}|${source}`;
  const hit = pathCache.get(key);
  if (hit && hit.until > Date.now()) return hit.result;
  const result = core.media(gatewayId, source, sessionKey, agentIdFromKey(sessionKey) ?? undefined);
  pathCache.set(key, { result, until: Date.now() + 60_000 });
  result
    .then((r) => {
      const expires = r.expiresAt ? Date.parse(r.expiresAt) - 30_000 : Date.now() + 5 * 60_000;
      pathCache.set(key, { result, until: r.available ? expires : Date.now() + 60_000 });
    })
    .catch(() => pathCache.delete(key));
  return result;
}

function kindFromMime(mime: string | undefined, fallback: MediaRef["kind"]): MediaRef["kind"] {
  if (!mime) return fallback;
  if (mime.startsWith("image/")) return "image";
  if (mime.startsWith("video/")) return "video";
  if (mime.startsWith("audio/")) return "audio";
  return "file";
}

function formatSize(bytes?: number): string {
  if (!bytes) return "";
  if (bytes < 1024 * 1024) return `${Math.max(1, Math.round(bytes / 1024))} KB`;
  return `${(bytes / 1024 / 1024).toFixed(1)} MB`;
}

function MediaView({ media, where }: { media: MediaRef; where: Where }) {
  const gateway = useApp((s) => gatewayOf(s.roster, where.gatewayId));
  const origin = gatewayHttpOrigin(gateway?.status.url);
  const [artifactId, setArtifactId] = useState<string | undefined>(media.artifactId);
  const [src, setSrc] = useState<string | null>(() => {
    if (media.data) return media.data.startsWith("data:") ? media.data : `data:${media.mimeType ?? "image/png"};base64,${media.data}`;
    if (media.url && /^(https?:|data:|blob:)/.test(media.url)) return media.url;
    return null;
  });
  const [failed, setFailed] = useState<string | null>(null);
  const [lookedUp, setLookedUp] = useState(false);
  // For `MEDIA:/path` files with no stored copy: asking the gateway for the file itself.
  const [pathState, setPathState] = useState<"idle" | "loading" | "ready" | "gone">("idle");
  const [served, setServed] = useState<{ mimeType?: string; sizeBytes?: number } | null>(null);
  const [viewing, setViewing] = useState<{ startAt: number } | null>(null);
  const videoRef = useRef<HTMLVideoElement>(null);
  const kind = kindFromMime(served?.mimeType, media.kind);
  const sizeBytes = served?.sizeBytes ?? media.sizeBytes;
  const inline = kind !== "file";

  // `MEDIA:/path` references: find the stored copy of that file in this thread.
  useEffect(() => {
    if (!media.path || artifactId) return;
    let cancelled = false;
    sessionArtifacts(where.gatewayId, where.sessionKey).then((list) => {
      if (cancelled) return;
      const name = media.fileName?.toLowerCase();
      const hit = list.find((a) => [a.title, a.fileName].some((n) => n && n.toLowerCase() === name));
      if (hit) setArtifactId(hit.id);
      setLookedUp(true);
    });
    return () => {
      cancelled = true;
    };
  }, [media.path, media.fileName, artifactId, where.gatewayId, where.sessionKey]);

  // No stored copy: OpenClaw can serve the file straight from the guy's machine.
  // (Not keyed on pathState: setting it to "loading" would cancel this very request.)
  useEffect(() => {
    if (!media.path || artifactId || !lookedUp) return;
    if (gateway?.status.kind !== "openclaw") return setPathState("gone");
    let cancelled = false;
    setPathState("loading");
    pathMedia(where.gatewayId, media.path, where.sessionKey)
      .then((r) => {
        if (cancelled) return;
        if (!r.available || !r.url) return setPathState("gone");
        setServed({ mimeType: r.mimeType, sizeBytes: r.sizeBytes });
        setSrc(r.url);
        setPathState("ready");
      })
      .catch(() => !cancelled && setPathState("gone"));
    return () => {
      cancelled = true;
    };
  }, [media.path, artifactId, lookedUp, gateway?.status.kind, where.gatewayId, where.sessionKey]);

  // Inline media resolves its URL right away; files wait for a click.
  useEffect(() => {
    if (src || !inline || failed) return;
    if (!artifactId) {
      if (media.url && origin && !media.path) setSrc(origin + media.url);
      return;
    }
    let cancelled = false;
    resolveArtifactUrl(where.gatewayId, where.sessionKey, artifactId, origin)
      .then((u) => !cancelled && setSrc(u))
      .catch((e) => !cancelled && setFailed(errorMessage(e)));
    return () => {
      cancelled = true;
    };
  }, [src, inline, failed, artifactId, media.url, media.path, origin, where.gatewayId, where.sessionKey]);

  const name = media.fileName ?? (kind === "image" ? "Image" : kind === "video" ? "Video" : "File");
  const save = useSave(
    () => (artifactId ? resolveArtifactUrl(where.gatewayId, where.sessionKey, artifactId, origin) : Promise.resolve(src)),
    name,
    where.gatewayId,
  );
  const onBot = media.path && !artifactId && !src && pathState === "gone";

  // Not retrievable: a file that only exists on the bot's machine.
  if (onBot || (!src && !artifactId && !inline && !media.path)) {
    return (
      <FileCard
        icon={kind}
        name={name}
        detail={media.path ? `on ${gateway?.name ?? "the bot's computer"}` : formatSize(sizeBytes)}
        title={media.path}
      />
    );
  }

  if (failed) {
    // Can't show it here (e.g. a video format WebKit won't play), but it may still save fine.
    const savable = Boolean(src || artifactId);
    return <FileCard icon={kind} name={name} detail="couldn't load" title={failed} broken save={savable ? save : undefined} />;
  }

  if (kind === "file") {
    return <FileCard icon="file" name={name} detail={formatSize(sizeBytes)} save={save} />;
  }

  if (!src) {
    return kind === "image" ? (
      <div className="h-40 w-56 animate-pulse rounded-xl bg-[var(--color-chip)]" />
    ) : (
      <FileCard icon={kind} name={name} detail="loading…" />
    );
  }

  const expand = () => {
    const v = videoRef.current;
    v?.pause();
    setViewing({ startAt: v?.currentTime ?? 0 });
  };
  const viewer = viewing && src && (kind === "image" || kind === "video") && (
    <Lightbox kind={kind} src={src} name={name} startAt={viewing.startAt} save={save} onClose={() => setViewing(null)} />
  );

  if (kind === "video") {
    return (
      <div className="group/video relative max-w-[440px]">
        <video
          ref={videoRef}
          src={src}
          controls
          preload="metadata"
          playsInline
          onError={() => setFailed("This video couldn't be played here.")}
          className="max-h-80 w-full rounded-xl border border-[var(--color-line)] bg-black"
        />
        <button
          onClick={expand}
          title="View larger"
          aria-label="View larger"
          className="absolute right-2 top-2 flex h-7 w-7 items-center justify-center rounded-full bg-black/55 text-white opacity-0 backdrop-blur transition group-hover/video:opacity-100 focus-visible:opacity-100"
        >
          <Maximize2 size={13} />
        </button>
        <FileCaption name={name} size={sizeBytes} save={save} onView={expand} />
        {viewer}
      </div>
    );
  }
  if (kind === "audio") {
    return (
      <div className="w-[320px]">
        <audio src={src} controls preload="metadata" onError={() => setFailed("This audio couldn't be played here.")} className="w-full" />
        <FileCaption name={name} size={sizeBytes} save={save} />
      </div>
    );
  }
  return (
    <>
      <button onClick={() => setViewing({ startAt: 0 })} className="block cursor-zoom-in" title={`View ${name}`}>
        <img
          src={src}
          alt={name}
          onError={() => setFailed("This image couldn't be loaded.")}
          className="max-h-80 max-w-[360px] rounded-xl border border-[var(--color-line)] object-contain"
        />
      </button>
      {viewer}
    </>
  );
}

function FileCaption({ name, size, save, onView }: { name: string; size?: number; save: Save; onView?: () => void }) {
  const SaveIcon = saveIcon(save);
  return (
    <div className="mt-1 flex items-center gap-1.5 px-1 text-[11px] text-[var(--color-ink-3)]">
      <span className="truncate">{name}</span>
      {size ? <span className="shrink-0">· {formatSize(size)}</span> : null}
      <span className="ml-auto flex shrink-0 items-center gap-2.5">
        {onView && (
          <button onClick={onView} className="flex items-center gap-0.5 hover:text-[var(--color-ink)]">
            <Maximize2 size={11} /> View
          </button>
        )}
        <button
          onClick={save.run}
          disabled={save.status === "saving"}
          title={saveTitle(save)}
          className={`flex items-center gap-0.5 hover:text-[var(--color-ink)] ${save.status === "saving" ? "animate-pulse" : ""}`}
        >
          <SaveIcon size={11} /> {saveLabel(save)}
        </button>
      </span>
    </div>
  );
}

function FileCard({
  icon,
  name,
  detail,
  title,
  save,
  broken,
}: {
  icon: MediaRef["kind"];
  name: string;
  detail?: string;
  title?: string;
  /** Clicking the card saves the file to Downloads. */
  save?: Save;
  broken?: boolean;
}) {
  const Icon = broken ? ImageOff : icon === "video" ? Film : icon === "audio" ? Music : icon === "image" ? FileIcon : FileIcon;
  const SaveIcon = save && saveIcon(save);
  const busy = save?.status === "saving";
  if (save && save.status !== "idle") {
    detail = saveLabel(save);
    title = saveTitle(save);
  }
  return (
    <button
      onClick={save?.run}
      disabled={!save || busy}
      title={title}
      className="flex max-w-[360px] items-center gap-2.5 rounded-xl border border-[var(--color-line)] bg-[var(--color-pane-solid)] px-3 py-2 text-left transition enabled:hover:border-[var(--color-ink-3)]/40"
    >
      <span className="flex h-8 w-8 shrink-0 items-center justify-center rounded-lg bg-[var(--color-chip)] text-[var(--color-ink-2)]">
        <Icon size={15} />
      </span>
      <span className="min-w-0">
        <span className="block truncate text-[12.5px] font-medium text-[var(--color-ink)]">{name}</span>
        {detail && <span className="block truncate text-[11px] text-[var(--color-ink-3)]">{detail}</span>}
      </span>
      {SaveIcon && <SaveIcon size={13} className={`ml-1 shrink-0 text-[var(--color-ink-3)] ${busy ? "animate-pulse" : ""}`} />}
    </button>
  );
}
