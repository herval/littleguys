// A guy's browser, live, docked beside the chat (OpenClaw only). Frames come
// from the gateway's screencast (relayed by the Rust core); clicks, keys and
// scrolling go back through `browser.request` /act — the same recipe as
// OpenClaw's own Control UI.
import { ArrowLeft, ArrowRight, Globe, MousePointerClick, Play, RotateCw, X } from "lucide-react";
import { useCallback, useEffect, useRef, useState } from "react";
import { browser } from "../lib/gateway";
import { browserProfileName, hasOwnBrowser } from "../lib/guySetup";
import { guyHue, guyName } from "../lib/guys";
import { type BrowserStreamMessage, core, errorMessage } from "../lib/ipc";
import type { BrowserTab, Guy } from "../lib/types";
import { useApp } from "../stores/app";
import { Avatar } from "../ui/Avatar";
import { IconButton, Spinner } from "../ui/kit";

type Phase = "starting" | "live" | "stills" | "not-running" | "paused" | "error";
type Frame = { src: string; cssWidth: number; cssHeight: number };

const MIN_WIDTH = 320;

/** Playwright key names for the keys we forward. */
const KEY_NAMES: Record<string, string> = {
  Enter: "Enter",
  Backspace: "Backspace",
  Tab: "Tab",
  Escape: "Escape",
  Delete: "Delete",
  ArrowUp: "ArrowUp",
  ArrowDown: "ArrowDown",
  ArrowLeft: "ArrowLeft",
  ArrowRight: "ArrowRight",
  Home: "Home",
  End: "End",
  PageUp: "PageUp",
  PageDown: "PageDown",
  " ": "Space",
};

function keyFor(e: React.KeyboardEvent): string | null {
  const base = KEY_NAMES[e.key] ?? (e.key.length === 1 ? e.key : null);
  if (!base) return null;
  const mods = [e.metaKey && "Meta", e.ctrlKey && "Control", e.altKey && "Alt", e.shiftKey && e.key.length > 1 && "Shift"].filter(Boolean);
  return [...mods, base].join("+");
}

function normalizeUrl(input: string): string {
  const v = input.trim();
  if (/^[a-z][a-z0-9+.-]*:/i.test(v)) return v;
  if (/^[^\s]+\.[^\s]{2,}/.test(v)) return `https://${v}`;
  return `https://www.google.com/search?q=${encodeURIComponent(v)}`;
}

function isUnsupported(e: unknown): boolean {
  const text = JSON.stringify(e ?? "");
  return text.includes("SCREENCAST_UNSUPPORTED") || text.includes("existing-session");
}

export function BrowserPanel({ guy }: { guy: Guy }) {
  const gw = guy.gatewayId;
  const width = useApp((s) => s.browser.width);
  const setWidth = useApp((s) => s.setBrowserWidth);
  const setOpen = useApp((s) => s.setBrowserOpen);
  const [profile, setProfile] = useState<string | undefined | null>(null); // null = still checking
  const [phase, setPhase] = useState<Phase>("starting");
  const [error, setError] = useState<string | null>(null);
  const [tabs, setTabs] = useState<BrowserTab[]>([]);
  const [targetId, setTargetId] = useState<string | undefined>();
  const [url, setUrl] = useState("");
  const [address, setAddress] = useState("");
  const [editing, setEditing] = useState(false);
  const [frame, setFrame] = useState<Frame | null>(null);
  const [restart, setRestart] = useState(0);
  const [visible, setVisible] = useState(() => document.visibilityState === "visible");
  const imgRef = useRef<HTMLImageElement>(null);
  const evalOff = useRef(false);
  const keyQueue = useRef<Promise<unknown>>(Promise.resolve());
  const wheel = useRef({ dx: 0, dy: 0, timer: 0 as number | 0 });
  // Reconnects since the last frame. Kept across restarts so a stream that keeps dropping gives up.
  const attempts = useRef(0);

  const req = useCallback(
    <T,>(method: "GET" | "POST" | "DELETE", path: string, body?: object) => browser.request<T>(gw, method, path, { profile: profile ?? undefined, body }),
    [gw, profile],
  );

  // Which browser: the guy's own profile if it has one, else the gateway's default.
  useEffect(() => {
    let cancelled = false;
    hasOwnBrowser(gw, guy.id)
      .then((own) => !cancelled && setProfile(own ? browserProfileName(guy.id) : undefined))
      .catch(() => !cancelled && setProfile(undefined));
    return () => {
      cancelled = true;
    };
  }, [gw, guy.id]);

  useEffect(() => {
    const onVis = () => setVisible(document.visibilityState === "visible");
    document.addEventListener("visibilitychange", onVis);
    return () => document.removeEventListener("visibilitychange", onVis);
  }, []);

  const showFrame = useCallback((next: Frame) => {
    setFrame((prev) => {
      if (prev && prev.src.startsWith("blob:") && prev.src !== next.src) setTimeout(() => URL.revokeObjectURL(prev.src), 500);
      return next;
    });
  }, []);

  const loadTabs = useCallback(async (): Promise<BrowserTab[]> => {
    const r = await req<{ running?: boolean; tabs?: BrowserTab[] }>("GET", "/tabs");
    const pages = (r.tabs ?? []).filter((t) => !t.type || t.type === "page");
    setTabs(pages);
    return r.running === false ? [] : pages;
  }, [req]);

  // The stream itself: (re)started when the guy, tab or visibility changes.
  useEffect(() => {
    if (profile === null || !visible) {
      if (!visible) setPhase("paused");
      return;
    }
    let cancelled = false;
    let stop: (() => void) | null = null;
    let poll = 0;
    let noFrames = 0;
    let live = false;
    let stilling = false;
    let target: string | undefined = targetId;

    const onMessage = (m: BrowserStreamMessage) => {
      if (cancelled) return;
      if (m.kind === "frame") {
        live = true;
        attempts.current = 0;
        clearTimeout(poll); // stills may have been standing in until the tab painted
        const blob = new Blob([m.jpeg.slice()], { type: "image/jpeg" });
        showFrame({ src: URL.createObjectURL(blob), cssWidth: m.header.cssWidth, cssHeight: m.header.cssHeight });
        if (m.header.url) setUrl(m.header.url);
        setPhase("live");
      } else if (m.kind === "text") {
        if (m.data.url) setUrl(m.data.url);
        if (m.data.type === "meta") loadTabs().catch(() => {});
        if (m.data.targetId) setTargetId((target = m.data.targetId));
        if (m.data.type === "error") setError(m.data.message ?? "The browser stream reported an error.");
      } else if (m.kind === "ended" && m.code !== 1000) {
        // A stream that never painted won't start to on a retry: stills stand in, and
        // restarting would only cancel them. Tab closed → pick another; hiccup → try again.
        if (!live && m.code !== 4004) {
          clearTimeout(noFrames);
          stills();
        } else if (attempts.current++ < 5) {
          setTimeout(() => !cancelled && setRestart((n) => n + 1), m.code === 4004 ? 300 : 1500);
        } else {
          setPhase("error");
          setError(`The stream stopped (${m.reason || m.code}).`);
        }
      }
    };

    const stills = async () => {
      if (stilling) return;
      stilling = true;
      setPhase("stills");
      // The page's size maps clicks onto the picture; the first picture doesn't wait for it.
      let size = { w: 1280, h: 800 };
      req<{ result?: { w: number; h: number } }>("POST", "/act", {
        kind: "evaluate",
        targetId: target,
        fn: "() => ({ w: innerWidth, h: innerHeight })",
      })
        .then((r) => {
          if (r.result?.w) size = r.result;
        })
        .catch(() => {});
      let shown = false;
      // One screenshot at a time: a slow one shouldn't stack more behind it on the gateway.
      const tick = async () => {
        try {
          const shot = await core.browser.screenshot(gw, profile ?? undefined, target);
          if (cancelled || live) return;
          shown = true;
          setError(null);
          showFrame({ src: shot.src, cssWidth: size.w, cssHeight: size.h });
          if (shot.url) setUrl(shot.url);
        } catch (e) {
          if (cancelled || live) return;
          setError(errorMessage(e));
          if (!shown) return setPhase("error");
        }
        if (!cancelled && !live) poll = window.setTimeout(tick, 1500);
      };
      tick();
    };

    (async () => {
      setError(null);
      try {
        const status = await req<{ running?: boolean }>("GET", "/");
        if (cancelled) return;
        if (status.running === false) return setPhase("not-running");
        const pages = await loadTabs();
        if (cancelled) return;
        target = target && pages.some((t) => t.targetId === target) ? target : pages[0]?.targetId;
        if (!target) return setPhase("not-running");
        setTargetId(target);
        const started = await core.browser.stream(gw, { profile: profile ?? undefined, targetId: target, maxWidth: 1600, quality: 70 }, onMessage);
        if (cancelled) return started.stop();
        stop = started.stop;
        if (started.url) setUrl(started.url);
        // Chrome only paints the tab in front of a visible window: a background tab, a
        // minimized window or a sleeping display sends no frames. Show stills until one comes.
        noFrames = window.setTimeout(() => !cancelled && !live && stills(), 1500);
      } catch (e) {
        if (cancelled) return;
        if (isUnsupported(e)) return stills();
        setPhase("error");
        setError(errorMessage(e));
      }
    })();

    return () => {
      cancelled = true;
      stop?.();
      clearTimeout(noFrames);
      clearTimeout(poll);
    };
    // targetId changes come from the stream itself; switching tabs bumps `restart`.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [gw, profile, visible, restart, req, loadTabs, showFrame]);

  useEffect(() => {
    if (!editing) setAddress(url);
  }, [url, editing]);

  const act = useCallback(
    (body: object) => req("POST", "/act", { targetId, ...body }).catch((e) => setError(errorMessage(e))),
    [req, targetId],
  );

  const evaluate = useCallback(
    async (fn: string) => {
      if (evalOff.current) return false;
      try {
        await req("POST", "/act", { kind: "evaluate", targetId, fn });
        return true;
      } catch (e) {
        if (/EVALUATE/i.test(JSON.stringify(e))) evalOff.current = true;
        return false;
      }
    },
    [req, targetId],
  );

  const toPage = (e: React.MouseEvent) => {
    const img = imgRef.current;
    if (!img || !frame) return null;
    const r = img.getBoundingClientRect();
    return {
      x: Math.max(0, Math.round(((e.clientX - r.left) / r.width) * frame.cssWidth)),
      y: Math.max(0, Math.round(((e.clientY - r.top) / r.height) * frame.cssHeight)),
    };
  };

  const onWheel = (e: React.WheelEvent) => {
    const w = wheel.current;
    w.dx += e.deltaX;
    w.dy += e.deltaY;
    if (w.timer) return;
    w.timer = window.setTimeout(async () => {
      const { dx, dy } = w;
      w.dx = w.dy = 0;
      w.timer = 0;
      if (!(await evaluate(`() => { window.scrollBy(${Math.round(dx)}, ${Math.round(dy)}); return true; }`)) && Math.abs(dy) > 20) {
        await act({ kind: "press", key: dy > 0 ? "PageDown" : "PageUp" });
      }
    }, 100);
  };

  const onKeyDown = (e: React.KeyboardEvent) => {
    if ((e.target as HTMLElement).tagName === "INPUT" || (e.target as HTMLElement).tagName === "SELECT") return;
    const key = keyFor(e);
    if (!key) return;
    e.preventDefault();
    keyQueue.current = keyQueue.current.then(() => act({ kind: "press", key }));
  };

  const reconnect = () => {
    attempts.current = 0;
    setRestart((n) => n + 1);
  };

  const startBrowser = async () => {
    setPhase("starting");
    try {
      await req("POST", "/start");
      reconnect();
    } catch (e) {
      setPhase("error");
      setError(errorMessage(e));
    }
  };

  const switchTab = async (id: string) => {
    setTargetId(id);
    await req("POST", "/tabs/focus", { targetId: id }).catch(() => {});
    reconnect();
  };

  const navigate = async () => {
    const next = normalizeUrl(address);
    (document.activeElement as HTMLElement | null)?.blur();
    setEditing(false);
    setUrl(next);
    try {
      await req("POST", "/navigate", { url: next, targetId });
      loadTabs().catch(() => {});
    } catch (e) {
      setError(errorMessage(e));
    }
  };

  // Drag the left edge to resize.
  const startResize = (e: React.MouseEvent) => {
    e.preventDefault();
    const startX = e.clientX;
    const startW = width;
    const move = (ev: MouseEvent) => {
      const max = Math.max(MIN_WIDTH, window.innerWidth - 560);
      setWidth(Math.min(max, Math.max(MIN_WIDTH, startW + (startX - ev.clientX))));
    };
    const up = () => {
      window.removeEventListener("mousemove", move);
      window.removeEventListener("mouseup", up);
    };
    window.addEventListener("mousemove", move);
    window.addEventListener("mouseup", up);
  };

  const domainOf = (u?: string) => {
    try {
      return u ? new URL(u).hostname.replace(/^www\./, "") : "";
    } catch {
      return u ?? "";
    }
  };
  const canAct = phase === "live" || phase === "stills";
  const name = guyName(guy);
  const status =
    phase === "live"
      ? { label: "Live", tone: "live" }
      : phase === "stills"
        ? { label: "Every few seconds", tone: "ok" }
        : phase === "paused"
          ? { label: "Paused", tone: "muted" }
          : phase === "not-running"
            ? { label: "Closed", tone: "muted" }
            : phase === "error"
              ? { label: "Stopped", tone: "bad" }
              : { label: "Connecting", tone: "muted" };

  return (
    <aside
      style={{ width: Math.max(MIN_WIDTH, Math.min(width, window.innerWidth - 560)), ["--guy-h" as string]: guyHue(guy.id) }}
      className="guy-themed relative flex shrink-0 flex-col border-l border-[var(--color-line)] bg-[var(--guy-wash)]"
    >
      <div
        onMouseDown={startResize}
        className="absolute -left-1 top-0 z-10 h-full w-2 cursor-col-resize transition-colors hover:bg-[var(--guy-wash-strong)]"
        aria-hidden
      />

      <div data-tauri-drag-region className="flex h-12 shrink-0 items-center gap-2.5 pl-3.5 pr-2">
        <Avatar agent={guy} size={24} />
        <h3 data-tauri-drag-region className="min-w-0 truncate font-display text-[13.5px] font-semibold">
          {name}'s browser
        </h3>
        <span
          className={`flex shrink-0 items-center gap-1.5 rounded-full px-2 py-0.5 text-[11px] font-medium ${
            status.tone === "live"
              ? "bg-[var(--color-pane-solid)] text-[var(--color-ok)] shadow-sm"
              : status.tone === "bad"
                ? "text-[var(--color-bad)]"
                : "text-[var(--color-ink-3)]"
          }`}
        >
          {status.tone === "live" ? (
            <span className="relative flex h-1.5 w-1.5">
              <span className="absolute inline-flex h-full w-full animate-ping rounded-full bg-[var(--color-ok)] opacity-70 motion-reduce:hidden" />
              <span className="relative inline-flex h-1.5 w-1.5 rounded-full bg-[var(--color-ok)]" />
            </span>
          ) : phase === "starting" ? (
            <Spinner size={10} />
          ) : null}
          {status.label}
        </span>
        <span className="flex-1" data-tauri-drag-region />
        <IconButton label="Close browser" onClick={() => setOpen(guy.uid, false)}>
          <X size={14} />
        </IconButton>
      </div>

      <div className="flex shrink-0 items-center gap-0.5 px-3 pb-3">
        <IconButton label="Back" onClick={() => evaluate("() => { history.go(-1); return true; }")} disabled={!canAct}>
          <ArrowLeft size={14} />
        </IconButton>
        <IconButton label="Forward" onClick={() => evaluate("() => { history.go(1); return true; }")} disabled={!canAct}>
          <ArrowRight size={14} />
        </IconButton>
        <IconButton label="Reload" onClick={() => evaluate("() => { location.reload(); return true; }")} disabled={!canAct}>
          <RotateCw size={13} />
        </IconButton>
        <form
          className="ml-1 min-w-0 flex-1"
          onSubmit={(e) => {
            e.preventDefault();
            navigate();
          }}
        >
          <label className="flex h-8 items-center gap-2 rounded-full bg-[var(--color-pane-solid)] px-3 shadow-[0_1px_2px_rgb(0_0_0/0.06)] ring-1 ring-[var(--color-line)] focus-within:ring-2 focus-within:ring-[var(--color-accent)]/40">
            <Globe size={12} className="shrink-0 text-[var(--color-ink-3)]" />
            <input
              className="selectable min-w-0 flex-1 bg-transparent text-[12.5px] outline-none placeholder:text-[var(--color-ink-3)]"
              value={editing ? address : domainOf(url) || address}
              onFocus={(e) => {
                setEditing(true);
                setAddress(url);
                requestAnimationFrame(() => e.target.select());
              }}
              onBlur={() => setEditing(false)}
              onChange={(e) => setAddress(e.target.value)}
              placeholder="Search or type an address"
              disabled={phase === "not-running"}
            />
          </label>
        </form>
      </div>

      <div className="min-h-0 flex-1 overflow-y-auto px-4 pb-4">
        <div
          tabIndex={0}
          onKeyDown={onKeyDown}
          className="group relative overflow-hidden rounded-xl bg-[var(--color-pane-solid)] shadow-[0_12px_32px_-8px_rgb(0_0_0/0.18)] outline-none ring-1 ring-black/5 focus-visible:ring-2 focus-visible:ring-[var(--color-accent)]/50 dark:ring-white/10"
        >
          {frame && (phase === "live" || phase === "stills" || phase === "paused") ? (
            <>
              <img
                ref={imgRef}
                src={frame.src}
                alt={`What ${name} sees`}
                draggable={false}
                onClick={(e) => {
                  const p = toPage(e);
                  if (p) act({ kind: "clickCoords", ...p, ...(e.detail === 2 ? { doubleClick: true } : {}) });
                  (e.currentTarget.parentElement as HTMLElement | null)?.focus();
                }}
                onWheel={onWheel}
                className={`block w-full select-none ${phase === "paused" ? "opacity-60 grayscale" : ""}`}
              />
              {canAct && (
                <span className="pointer-events-none absolute bottom-2.5 left-1/2 flex -translate-x-1/2 items-center gap-1.5 whitespace-nowrap rounded-full bg-black/70 px-2.5 py-1 text-[11px] font-medium text-white opacity-0 backdrop-blur transition-opacity group-hover:opacity-100 group-focus:opacity-0">
                  <MousePointerClick size={12} /> Click and type to lend {name} a hand
                </span>
              )}
            </>
          ) : (
            <div className="flex aspect-[16/10] flex-col items-center justify-center px-6 text-center">
              {phase === "not-running" ? (
                <>
                  <Avatar agent={guy} size={44} />
                  <p className="mt-3 font-display text-[14px] font-semibold">{name}'s browser is closed</p>
                  <p className="mt-1 max-w-[240px] text-[12px] text-[var(--color-ink-3)]">It opens on its own when {name} browses. You can also open it now.</p>
                  <button
                    onClick={startBrowser}
                    className="mt-3 inline-flex items-center gap-1.5 rounded-full bg-[var(--color-ink)] px-3.5 py-1.5 text-[12px] font-medium text-[var(--color-pane-solid)] hover:opacity-90"
                  >
                    <Play size={11} /> Open browser
                  </button>
                </>
              ) : phase === "error" ? (
                <>
                  <p className="font-display text-[14px] font-semibold">The browser view stopped</p>
                  <p className="selectable mt-1 max-w-[280px] text-[12px] text-[var(--color-ink-3)]">{error}</p>
                  <button
                    onClick={reconnect}
                    className="mt-3 rounded-full bg-[var(--color-chip)] px-3.5 py-1.5 text-[12px] font-medium hover:bg-[var(--guy-wash-strong)]"
                  >
                    Reconnect
                  </button>
                </>
              ) : (
                <p className="flex items-center gap-2 text-[12px] text-[var(--color-ink-3)]">
                  <Spinner size={12} />{" "}
                  {phase === "paused"
                    ? "Paused while littleguys is hidden"
                    : phase === "stills"
                      ? `Getting a picture of ${name}'s browser`
                      : `Connecting to ${name}'s browser`}
                </p>
              )}
            </div>
          )}
        </div>
        {error && phase !== "error" && <p className="mt-2 px-1 text-[11.5px] text-[var(--color-bad)]">{error}</p>}

        {tabs.length > 0 && (
          <section className="mt-4">
            <h4 className="mb-1 flex items-center gap-1.5 px-1 text-[12px] font-semibold text-[var(--color-ink-2)]">
              Open tabs
              <span className="rounded-full bg-[var(--color-pane-solid)] px-1.5 text-[10.5px] font-medium tabular-nums text-[var(--color-ink-3)]">
                {tabs.length}
              </span>
            </h4>
            {tabs.map((t) => {
              const current = t.targetId === targetId;
              const domain = domainOf(t.url);
              return (
                <button
                  key={t.targetId}
                  onClick={() => !current && switchTab(t.targetId)}
                  className={`flex w-full items-center gap-2.5 rounded-lg px-2 py-1.5 text-left transition-colors ${
                    current ? "bg-[var(--color-pane-solid)] shadow-sm" : "hover:bg-[var(--guy-wash-strong)]"
                  }`}
                >
                  <span className="flex h-6 w-6 shrink-0 items-center justify-center rounded-md bg-[var(--guy-wash-strong)] text-[11px] font-semibold uppercase text-[var(--guy-ink)]">
                    {domain.slice(0, 1) || "·"}
                  </span>
                  <span className="min-w-0 flex-1">
                    <span className={`block truncate text-[12.5px] ${current ? "font-medium" : ""}`}>{t.title || domain || "New tab"}</span>
                    {domain && <span className="block truncate text-[11px] text-[var(--color-ink-3)]">{domain}</span>}
                  </span>
                  {current && <span className="shrink-0 text-[10.5px] font-medium text-[var(--guy-ink)]">Showing</span>}
                </button>
              );
            })}
          </section>
        )}
        {profile && <p className="mt-3 px-1 text-[11px] text-[var(--color-ink-3)]">Uses {name}'s own browser profile, “{profile}”.</p>}
      </div>
    </aside>
  );
}
