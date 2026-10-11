import { CornerDownLeft, MessageCircleReply, Plus, Power, Settings } from "lucide-react";
import { useEffect, useLayoutEffect, useMemo, useRef, useState } from "react";
import { chat as chatApi } from "../lib/gateway";
import { activityFor, cleanPreview, defaultThread, guyName, isMultiGateway, liveGateways, needsAction, needsProxyFix, statusLabel, threadKind } from "../lib/guys";
import { core, errorMessage, listen } from "../lib/ipc";
import type { GatewayRoster, Guy, Roster } from "../lib/types";
import { sortGuys, useApp } from "../stores/app";
import { Avatar, GuyMark } from "../ui/Avatar";
import { Spinner, TypingDots } from "../ui/kit";

export function Popover() {
  const roster = useApp((s) => s.roster);
  const order = useApp((s) => s.order);
  const rootRef = useRef<HTMLDivElement>(null);
  const [replyTo, setReplyTo] = useState<string | null>(null);

  const guys = useMemo(() => sortGuys(roster, order), [roster, order]);
  const multi = isMultiGateway(roster);
  const live = liveGateways(roster);
  const enabled = roster.gateways.filter((g) => g.enabled);
  const troubled = enabled.filter((g) => g.status.phase !== "connected");

  // Keep the window exactly as tall as its content.
  useLayoutEffect(() => {
    const el = rootRef.current;
    if (!el) return;
    const fit = () => core.fitPopover(Math.ceil(el.getBoundingClientRect().height)).catch(() => {});
    fit();
    const ro = new ResizeObserver(fit);
    ro.observe(el);
    return () => ro.disconnect();
  }, []);

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") core.hidePopover();
    };
    window.addEventListener("keydown", onKey);
    const unlisten = listen("lg:popover-shown", () => {
      setReplyTo(null);
      // Pick up order changes made in the main window.
      try {
        const raw = localStorage.getItem("littleguys.order");
        if (raw) useApp.setState({ order: JSON.parse(raw) });
      } catch {
        /* ignore */
      }
    });
    return () => {
      window.removeEventListener("keydown", onKey);
      unlisten.then((u) => u());
    };
  }, []);

  const row = (guy: Guy) => (
    <GuyRow
      key={guy.uid}
      guy={guy}
      roster={roster}
      replying={replyTo === guy.uid}
      onReply={() => setReplyTo(replyTo === guy.uid ? null : guy.uid)}
    />
  );

  return (
    <div ref={rootRef} className="flex max-h-[640px] flex-col text-[13px]">
      <Header roster={roster} />
      {live.length === 0 ? (
        <Disconnected gateway={enabled[0]} />
      ) : (
        <div className="no-scrollbar max-h-[480px] overflow-y-auto px-1.5 pb-1">
          {live.every((g) => !g.loaded) && (
            <div className="flex items-center gap-2 px-3 py-4 text-[var(--color-ink-3)]">
              <Spinner /> Rounding up your little guys…
            </div>
          )}
          {live.some((g) => g.loaded) && guys.length === 0 && <EmptyRoster />}
          {multi
            ? live.map((g) => {
                const theirs = guys.filter((guy) => guy.gatewayId === g.id);
                if (!theirs.length) return null;
                return (
                  <div key={g.id}>
                    <GatewayHeader gateway={g} />
                    {theirs.map(row)}
                  </div>
                );
              })
            : guys.map(row)}
          {troubled.length > 0 && (
            <div className="mt-1 space-y-1 px-1 pb-1">
              {troubled.map((g) => (
                <TroubleRow key={g.id} gateway={g} />
              ))}
            </div>
          )}
        </div>
      )}
      <Footer />
    </div>
  );
}

function Header({ roster }: { roster: Roster }) {
  const enabled = roster.gateways.filter((g) => g.enabled);
  const live = enabled.filter((g) => g.status.phase === "connected");
  const connecting = enabled.some((g) => g.status.phase === "connecting" || g.status.phase === "starting");
  const dot = live.length
    ? live.length === enabled.length
      ? "bg-[var(--color-ok)]"
      : "bg-[var(--color-warn)]"
    : connecting
      ? "bg-[var(--color-warn)] animate-pulse"
      : "bg-[var(--color-bad)]";
  const label =
    enabled.length > 1
      ? `${live.length} of ${enabled.length} online`
      : live.length
        ? live[0].name
        : statusLabel(enabled[0]?.status);
  return (
    <div className="flex items-center justify-between px-3.5 pb-1.5 pt-3">
      <span className="font-display text-[13px] font-semibold tracking-tight">littleguys</span>
      <span className="flex items-center gap-1.5 text-[11px] text-[var(--color-ink-3)]">
        <span className={`h-1.5 w-1.5 rounded-full ${dot}`} />
        {label}
      </span>
    </div>
  );
}

function GatewayHeader({ gateway }: { gateway: GatewayRoster }) {
  return (
    <div className="px-2.5 pb-0.5 pt-2 text-[10.5px] font-semibold uppercase tracking-wide text-[var(--color-ink-3)]">{gateway.name}</div>
  );
}

function TroubleRow({ gateway: g }: { gateway: GatewayRoster }) {
  const urgent = needsAction(g.status);
  return (
    <button
      onClick={() => core.openMain({ view: "settings" })}
      className="flex w-full items-center gap-2 rounded-lg bg-[var(--color-chip)] px-2.5 py-1.5 text-left text-[12px] transition hover:bg-black/8 dark:hover:bg-white/10"
    >
      <span className={`h-1.5 w-1.5 rounded-full ${urgent ? "bg-[var(--color-warn)]" : "bg-[var(--color-bad)]"}`} />
      <span className="font-medium">{g.name}</span>
      <span className="text-[var(--color-ink-3)]">{statusLabel(g.status).toLowerCase()}</span>
      <span className="ml-auto text-[11px] text-[var(--color-accent-strong)]">{urgent ? "Fix →" : ""}</span>
    </button>
  );
}

function GuyRow({ guy, roster, replying, onReply }: { guy: Guy; roster: Roster; replying: boolean; onReply: () => void }) {
  const act = activityFor(roster, guy);
  const ring = act.needsYou ? "needs-you" : act.running ? "working" : act.unread ? "unread" : "idle";
  const preview = cleanPreview(act.latest?.lastMessagePreview);
  const target = act.latest && threadKind(act.latest.key) !== "routine" ? act.latest.key : defaultThread(roster, guy);

  return (
    <div className={`group rounded-xl transition ${replying ? "bg-[var(--color-chip)]" : "hover:bg-[var(--color-chip)]"}`}>
      <div
        role="button"
        tabIndex={0}
        onClick={() =>
          core.openMain({ gatewayId: guy.gatewayId, agentId: guy.id, sessionKey: act.unread || act.running ? target : null })
        }
        onKeyDown={(e) => {
          if (e.key === "Enter") core.openMain({ gatewayId: guy.gatewayId, agentId: guy.id });
        }}
        className="flex items-center gap-3 px-2 py-2 outline-none"
      >
        <Avatar agent={guy} size={40} ring={ring} />
        <div className="min-w-0 flex-1">
          <div className="flex items-baseline justify-between gap-2">
            <span className={`truncate ${act.unread ? "font-semibold" : "font-medium"}`}>{guyName(guy)}</span>
            <span
              className={`shrink-0 text-[11px] ${
                act.needsYou
                  ? "font-medium text-[var(--color-bad)]"
                  : act.running
                    ? "text-[var(--color-accent-strong)]"
                    : act.unread
                      ? "font-medium text-[var(--color-accent-strong)]"
                      : "text-[var(--color-ink-3)]"
              }`}
            >
              {act.running && !act.needsYou ? <TypingDots /> : act.status}
            </span>
          </div>
          <div className="flex items-center gap-1">
            <p className={`min-w-0 flex-1 truncate text-[12px] ${act.unread ? "text-[var(--color-ink)]" : "text-[var(--color-ink-3)]"}`}>
              {preview || (act.running ? "Working on it…" : "No messages yet")}
            </p>
            <button
              onClick={(e) => {
                e.stopPropagation();
                onReply();
              }}
              title={`Message ${guyName(guy)}`}
              className={`shrink-0 rounded-md p-1 text-[var(--color-ink-3)] transition hover:bg-black/5 hover:text-[var(--color-ink)] dark:hover:bg-white/10 ${
                replying ? "opacity-100" : "opacity-0 group-hover:opacity-100"
              }`}
            >
              <MessageCircleReply size={14} />
            </button>
          </div>
        </div>
      </div>
      {replying && <QuickReply guy={guy} sessionKey={target} />}
    </div>
  );
}

function QuickReply({ guy, sessionKey }: { guy: Guy; sessionKey: string }) {
  const [text, setText] = useState("");
  const [state, setState] = useState<"idle" | "sending" | "sent" | "error">("idle");
  const [error, setError] = useState<string | null>(null);
  const inputRef = useRef<HTMLInputElement>(null);
  useEffect(() => inputRef.current?.focus(), []);

  const send = async () => {
    const message = text.trim();
    if (!message || state === "sending") return;
    setState("sending");
    try {
      await chatApi.send(guy.gatewayId, sessionKey, message);
      setText("");
      setState("sent");
      setTimeout(() => setState("idle"), 1600);
    } catch (e) {
      setError(errorMessage(e));
      setState("error");
    }
  };

  return (
    <div className="px-2 pb-2">
      <div className="flex items-center gap-1.5 rounded-lg border border-[var(--color-line)] bg-[var(--color-pane-solid)] px-2 py-1 focus-within:border-[var(--color-accent)]">
        <input
          ref={inputRef}
          value={text}
          onChange={(e) => setText(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === "Enter") send();
          }}
          placeholder={state === "sent" ? "Sent ✓" : `Message ${guyName(guy)}…`}
          className="selectable min-w-0 flex-1 bg-transparent py-0.5 text-[12.5px] outline-none placeholder:text-[var(--color-ink-3)]"
        />
        {state === "sending" ? <Spinner size={12} /> : <CornerDownLeft size={13} className="text-[var(--color-ink-3)]" />}
      </div>
      {state === "error" && <p className="mt-1 px-1 text-[11px] text-[var(--color-bad)]">{error}</p>}
    </div>
  );
}

function EmptyRoster() {
  return (
    <div className="flex flex-col items-center px-6 pb-4 pt-2 text-center">
      <GuyMark size={64} mood="wave" />
      <p className="mt-1 font-medium">No little guys yet</p>
      <p className="mt-0.5 text-[12px] text-[var(--color-ink-3)]">Make your first one — it takes a few seconds.</p>
    </div>
  );
}

function Disconnected({ gateway }: { gateway?: GatewayRoster }) {
  const status = gateway?.status;
  const phase = status?.phase ?? "starting";
  const copy: Record<string, { title: string; body: string }> = {
    starting: { title: "Waking up…", body: "Connecting to OpenClaw." },
    connecting: { title: "Connecting…", body: status?.url ?? "" },
    pairing: { title: "Approve littleguys", body: `${gateway?.name ?? "Your gateway"} needs to approve this Mac once.` },
    "auth-failed": { title: "Can't sign in", body: status?.message ?? "The gateway rejected littleguys' credentials." },
    offline: { title: "Gateway is offline", body: status?.message ?? "littleguys will keep retrying." },
    "no-config": { title: "Let's get you set up", body: "OpenClaw isn't set up on this Mac yet." },
    error: needsProxyFix(status)
      ? { title: "One setting on your gateway", body: `${gateway?.name ?? "Your gateway"} won't take connections through its Tailscale Serve route yet.` }
      : { title: "Something's off", body: status?.message ?? "" },
  };
  const c = gateway ? (copy[phase] ?? copy.starting) : { title: "All gateways are off", body: "Turn one on in Settings." };
  const actionable = !gateway || (phase !== "starting" && phase !== "connecting");
  return (
    <div className="mx-2 mb-1 flex items-center gap-3 rounded-xl bg-[var(--color-chip)] px-3 py-3">
      <GuyMark size={44} mood={phase === "offline" ? "sleepy" : "happy"} />
      <div className="min-w-0 flex-1">
        <p className="font-medium">{c.title}</p>
        <p className="line-clamp-2 text-[12px] text-[var(--color-ink-3)]">{c.body}</p>
        {actionable && (
          <button
            onClick={() => core.openMain(gateway ? undefined : { view: "settings" })}
            className="mt-1.5 text-[12px] font-medium text-[var(--color-accent-strong)] hover:underline"
          >
            {!gateway ? "Open Settings →" : phase === "no-config" ? "Set up littleguys →" : "Fix it →"}
          </button>
        )}
      </div>
    </div>
  );
}

function Footer() {
  return (
    <div className="mt-1 border-t border-[var(--color-line)] px-1.5 py-1.5">
      <MenuItem icon={<Plus size={14} />} label="New little guy" onClick={() => core.openMain({ view: "new-guy" })} />
      <MenuItem
        icon={<span className="flex w-3.5 justify-center text-[13px] leading-none">◐</span>}
        label="Open littleguys"
        shortcut="⇧⌘L"
        onClick={() => core.openMain()}
      />
      <MenuItem icon={<Settings size={14} />} label="Settings…" shortcut="⌘," onClick={() => core.openMain({ view: "settings" })} />
      <MenuItem icon={<Power size={14} />} label="Quit littleguys" shortcut="⌘Q" onClick={() => core.quit()} />
    </div>
  );
}

function MenuItem({ icon, label, shortcut, onClick }: { icon: React.ReactNode; label: string; shortcut?: string; onClick: () => void }) {
  return (
    <button
      onClick={onClick}
      className="flex w-full items-center gap-2.5 rounded-lg px-2.5 py-1.5 text-left text-[13px] text-[var(--color-ink)] transition hover:bg-[var(--color-accent)] hover:text-white"
    >
      <span className="opacity-70">{icon}</span>
      <span className="flex-1">{label}</span>
      {shortcut && <span className="text-[11px] opacity-50">{shortcut}</span>}
    </button>
  );
}
