import { Archive, ArrowDown, Globe, RotateCw } from "lucide-react";
import { useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState } from "react";
import { sessions as sessionsApi } from "../lib/gateway";
import { channelName, gatewayOf, guyName, hasMainChat, isOnThisMac, isRunning, phaseLabel, sessionRow, threadKind, threadTitle } from "../lib/guys";
import { core } from "../lib/ipc";
import { presentTense, toolKind } from "../lib/toolPhrases";
import { firstUnread } from "../lib/transcript";
import type { Guy, SessionRow } from "../lib/types";
import { useApp } from "../stores/app";
import { useArchive, useArchivedRow } from "../stores/archive";
import { threadId, useChat, waitingCount } from "../stores/chat";
import { Avatar, GuyMark } from "../ui/Avatar";
import { Button, IconButton, Spinner } from "../ui/kit";
import { Approvals } from "./Approvals";
import { Composer } from "./Composer";
import { FolderPicker, threadFolder, tildify, usePendingFolder } from "./FolderPicker";
import { phaseLabel as livePhaseLabel, toItems, Transcript } from "./Transcript";

/** How far down the pane an unread thread opens its "New" divider: under the day chip, with a bit of what you'd read. */
const NEW_DIVIDER_TOP = 64;

export function ChatView({ guy, sessionKey }: { guy: Guy; sessionKey: string }) {
  const agent = guy;
  const gw = guy.gatewayId;
  const tid = threadId(gw, sessionKey);
  const rosterRow = useApp((s) => sessionRow(s.roster, gw, sessionKey));
  // Archived threads aren't in the roster; look in the archive if it hasn't been yet.
  const archivedRow = useArchivedRow(guy, sessionKey);
  const row = rosterRow ?? archivedRow;
  const archiveLoaded = useArchive((s) => guy.uid in s.byGuy);
  const inRoster = Boolean(rosterRow);
  useEffect(() => {
    if (!inRoster && !archiveLoaded) useArchive.getState().load({ uid: guy.uid, gatewayId: gw, id: guy.id });
  }, [inRoster, archiveLoaded, guy.uid, gw, guy.id]);
  const rosterRunning = useApp((s) => isRunning(s.roster, gw, sessionKey));
  const thread = useChat((s) => s.threads[tid]);
  const { open, send, abort, reload } = useChat.getState();
  const scrollRef = useRef<HTMLDivElement>(null);
  const contentRef = useRef<HTMLDivElement>(null);
  // Follow mode: keep the newest message in view while anything grows (streaming
  // text, images and code blocks laying out). Only scrolling up turns it off;
  // scrolling back down to the bottom turns it on again.
  const follow = useRef(true);
  const lastTop = useRef(0);
  // An unread thread opens at its "New" divider instead, and keeps it there as
  // things above it lay out, until you scroll or send. While everything new fits
  // on screen that's the bottom anyway, so replies still come into view.
  const atNew = useRef(false);
  const [showJump, setShowJump] = useState(false);
  const setFollow = useCallback((on: boolean) => {
    follow.current = on;
    setShowJump(!on);
  }, []);
  const pin = useCallback(() => {
    const el = scrollRef.current;
    if (!el) return;
    const divider = atNew.current ? el.querySelector<HTMLElement>("[data-new-divider]") : null;
    if (divider) {
      const bottom = el.scrollHeight - el.clientHeight;
      const top = el.scrollTop + divider.getBoundingClientRect().top - el.getBoundingClientRect().top - NEW_DIVIDER_TOP;
      el.scrollTop = Math.max(0, Math.min(top, bottom));
      lastTop.current = el.scrollTop;
      setFollow(el.scrollTop >= bottom - 1);
      return;
    }
    if (!follow.current) return;
    el.scrollTop = el.scrollHeight;
    lastTop.current = el.scrollTop;
  }, [setFollow]);

  useEffect(() => {
    open(tid);
  }, [open, tid]);

  // Where you left off, as it was when you opened the thread (opening it reads it).
  const [readTo, setReadTo] = useState<number | null>();
  useLayoutEffect(() => {
    if (readTo === undefined && row) setReadTo(row.unread ? (row.lastReadAt ?? 0) : null);
  }, [readTo, row]);

  // Opening a thread reads it, and so does a reply landing while it's open,
  // unless you marked it unread here: then it waits until you open it again.
  const keptUnread = useApp((s) => s.keptUnread === tid);
  useEffect(() => {
    if (row?.unread && !keptUnread) {
      sessionsApi.patch(gw, sessionKey, { unread: false }).then(() => core.refreshRoster(gw)).catch(() => {});
    }
  }, [row?.unread, keptUnread, gw, sessionKey]);

  const pendingMessages = useMemo(() => thread?.pending.map((p) => p.message) ?? [], [thread?.pending]);
  const items = useMemo(() => toItems(thread?.messages ?? [], pendingMessages), [thread?.messages, pendingMessages]);
  const live = thread?.live ?? null;
  const running = Boolean(live) || rosterRunning;
  const newFrom = useMemo(() => (readTo == null ? -1 : firstUnread(items, readTo || undefined)), [items, readTo]);

  const openedAtNew = useRef(false);
  useLayoutEffect(() => {
    if (newFrom > 0 && !openedAtNew.current) openedAtNew.current = atNew.current = true;
  }, [newFrom]);
  useLayoutEffect(pin, [pin, items.length, live?.text, live?.phase, thread?.loaded, newFrom]);

  // Images, videos and code blocks finish laying out after the first render, and
  // the pane shrinks when approvals or a taller composer show up.
  useEffect(() => {
    const el = scrollRef.current;
    const content = contentRef.current;
    if (!el || !content) return;
    const ro = new ResizeObserver(pin);
    ro.observe(content);
    ro.observe(el);
    return () => ro.disconnect();
  }, [pin]);

  const kind = threadKind(sessionKey);
  const mainChat = useApp((s) => hasMainChat(s.roster, gw));
  const isMain = kind === "main" && mainChat;
  const title = row ? threadTitle(row, mainChat) : isMain ? "Main chat" : kind === "main" ? "New chat" : "Chat";
  // The tool call in flight, if any: drives the header's status line and the browser button.
  const activeTool = useMemo(() => {
    if (!running) return null;
    for (let i = items.length - 1; i >= 0; i--) {
      const it = items[i];
      if (it.kind === "tool" && !it.result && thread?.toolStatus[it.call.id] === "running") return it.call;
    }
    return null;
  }, [items, running, thread?.toolStatus]);
  const browsing = Boolean(activeTool && toolKind(activeTool.name) === "browse");
  // ACP and Claude Code threads can start in any folder on this Mac; it's fixed once they have messages.
  const [pendingFolder, setPendingFolder] = usePendingFolder(tid);
  const folder = threadFolder(guy, row);
  const canPickFolder = useApp((s) => {
    const g = gatewayOf(s.roster, gw);
    return ((g?.status.kind === "acp" && isOnThisMac(g)) || g?.status.kind === "claudecode") && kind !== "routine";
  });
  // A title or preview means it has history somewhere, even if none loaded.
  const unstarted = Boolean(thread?.loaded) && items.length === 0 && !running && !row?.derivedTitle && !row?.lastMessagePreview;
  const activity = activeTool
    ? presentTense(activeTool)
    : live
      ? live.text
        ? "typing"
        : live.phase
          ? livePhaseLabel(live.phase).replace(/…$/, "")
          : "thinking"
      : running
        ? "working"
        : null;

  return (
    <div className="flex h-full min-h-0 flex-col">
      <ChatHeader
        guy={guy}
        title={title}
        threadKey={tid}
        sessionKey={sessionKey}
        folder={canPickFolder && unstarted ? null : folder}
        activity={activity}
        browsing={browsing}
      />
      <div
        ref={scrollRef}
        onScroll={(e) => {
          const el = e.currentTarget;
          if (atNew.current) {
            if (Math.abs(el.scrollTop - lastTop.current) < 2) return; // pin's own scroll
            atNew.current = false;
          }
          const gap = el.scrollHeight - el.scrollTop - el.clientHeight;
          // Content growing below never moves scrollTop, so being far from the
          // bottom alone doesn't mean the user left it. Moving up does (unless the
          // content shrank and clamped us, which leaves us at the bottom).
          if (el.scrollTop < lastTop.current && gap > 1) setFollow(false);
          else if (gap < 80) setFollow(true);
          lastTop.current = el.scrollTop;
        }}
        // Catches a scroll up even when new content lands in the same frame.
        onWheel={(e) => {
          atNew.current = false;
          if (e.deltaY < 0 && e.currentTarget.scrollTop > 0) setFollow(false);
        }}
        className="relative min-h-0 flex-1 overflow-y-auto px-5 pb-4 pt-2"
      >
        {!thread?.loaded && thread?.loading && (
          <div className="flex h-full items-center justify-center">
            <Spinner size={18} />
          </div>
        )}
        {thread?.error && (
          <div className="mx-auto mt-10 max-w-sm text-center">
            <p className="text-[var(--color-bad)]">{thread.error}</p>
            <Button className="mt-3" onClick={() => reload(tid)}>
              <RotateCw size={13} /> Try again
            </Button>
          </div>
        )}
        {thread?.loaded && items.length === 0 && !live && (
          <div className="flex h-full flex-col items-center justify-center pb-10 text-center">
            <Avatar agent={agent} size={72} ring="idle" />
            <p className="mt-3 font-display text-[15px] font-semibold">
              {kind === "routine" ? "Nothing from this run yet" : `Say hi to ${guyName(agent)}`}
            </p>
            <p className="mt-1 max-w-xs text-[12.5px] text-[var(--color-ink-3)]">
              {isMain
                ? "This is your main chat. Ask for anything — research, errands on the web, files, reminders."
                : kind === "routine"
                  ? `${guyName(agent)}'s messages for this run show up here as they work.`
                  : "A fresh thread. Good for keeping one topic in one place."}
            </p>
          </div>
        )}
        <div ref={contentRef} className="mx-auto max-w-[860px]">
          <Transcript
            gatewayId={gw}
            sessionKey={sessionKey}
            items={items}
            agent={agent}
            live={live}
            waiting={waitingCount(thread)}
            toolStatus={thread?.toolStatus ?? {}}
            newFrom={newFrom}
          />
          {thread?.sendError && (
            <p className="mt-3 text-center text-[12px] text-[var(--color-bad)]">{thread.sendError}</p>
          )}
        </div>
      </div>
      {showJump && (
        <div className="relative">
          <button
            onClick={() => {
              atNew.current = false;
              setFollow(true);
              const el = scrollRef.current;
              if (el) el.scrollTo({ top: el.scrollHeight, behavior: "smooth" });
            }}
            className="absolute -top-12 left-1/2 flex h-8 w-8 -translate-x-1/2 items-center justify-center rounded-full border border-[var(--color-line)] bg-[var(--color-pane-solid)] shadow-md"
            aria-label="Jump to latest"
          >
            <ArrowDown size={15} />
          </button>
        </div>
      )}
      <Approvals guy={guy} sessionKey={sessionKey} />
      <div className="mx-auto w-full max-w-[900px]">
        {canPickFolder && unstarted && (pendingFolder ?? folder) && (
          <FolderPicker guy={guy} value={(pendingFolder ?? folder) as string} onChange={setPendingFolder} />
        )}
        {row?.archived && <ArchivedNote guy={guy} row={row} />}
        <Composer
          gatewayId={gw}
          agentId={guy.id}
          sessionKey={sessionKey}
          draftKey={tid}
          placeholder={kind === "channel" ? `Reply in ${channelName(sessionKey) ?? "channel"} as ${guyName(agent)}…` : `Message ${guyName(agent)}…`}
          running={running}
          onSend={async (text, attachments) => {
            // Replying means you've caught up.
            atNew.current = false;
            setReadTo(null);
            // Writing in an archived thread brings it back (OpenClaw won't start work in one that's archived).
            if (row?.archived) await useArchive.getState().unarchive(guy, row);
            setFollow(true);
            const cwd = canPickFolder && unstarted && pendingFolder !== folder ? (pendingFolder ?? undefined) : undefined;
            await send(tid, text, attachments, cwd);
            setPendingFolder(null);
          }}
          onStop={() => abort(tid)}
        />
      </div>
    </div>
  );
}

/** Over an archived thread's composer: writing in it brings it back. */
function ArchivedNote({ guy, row }: { guy: Guy; row: SessionRow }) {
  const [busy, setBusy] = useState(false);
  return (
    <div className="flex items-center gap-1.5 px-7 pb-0.5 text-[12px] text-[var(--color-ink-3)]">
      <Archive size={12} className="shrink-0" />
      <span className="truncate">Archived. Sending a message brings it back.</span>
      <button
        disabled={busy}
        onClick={async () => {
          setBusy(true);
          await useArchive.getState().unarchive(guy, row).catch(() => {});
          setBusy(false);
        }}
        className="shrink-0 rounded-full px-2 py-0.5 font-medium text-[var(--color-accent-strong)] transition hover:bg-[var(--color-chip)] disabled:opacity-50"
      >
        Unarchive
      </button>
    </div>
  );
}

/** Who you're talking to and what they're up to, like a messaging app's header. */
function ChatHeader({
  guy,
  title,
  threadKey,
  sessionKey,
  folder,
  activity,
  browsing,
}: {
  guy: Guy;
  title: string;
  threadKey: string;
  sessionKey: string;
  /** Shown for ACP and Claude Code threads, which each work in a folder. */
  folder: string | null;
  activity: string | null;
  browsing: boolean;
}) {
  const kind = threadKind(sessionKey);
  const name = guyName(guy);
  const gateway = useApp((s) => gatewayOf(s.roster, guy.gatewayId));
  const hasBrowser = Boolean(gateway?.status.capabilities?.browser);
  const browserOpen = useApp((s) => Boolean(s.browser.open[guy.uid]));
  const online = gateway?.status.phase === "connected";
  const channel = kind === "channel" ? channelName(sessionKey) : null;
  const channelLabel = channel ? channel.charAt(0).toUpperCase() + channel.slice(1) : null;
  // The main chat is just "talking to Jefe"; other threads say which one this is.
  const isMain = kind === "main" && gateway?.status.kind !== "acp" && gateway?.status.kind !== "claudecode";
  const showFolder = (gateway?.status.kind === "acp" || gateway?.status.kind === "claudecode") && folder;
  const threadLabel = isMain ? null : kind === "channel" ? `${title} on ${channelLabel}` : title;

  return (
    <div data-tauri-drag-region className="flex h-14 shrink-0 items-center gap-3 border-b border-[var(--color-line)] px-5">
      <Avatar agent={guy} size={34} ring={activity ? "working" : "none"} />
      <div data-tauri-drag-region className="min-w-0 flex-1">
        <div data-tauri-drag-region className="flex min-w-0 items-baseline gap-2">
          <h3 className="shrink-0 font-display text-[14.5px] font-semibold leading-tight">{name}</h3>
          {threadLabel && <span className="truncate text-[12px] text-[var(--color-ink-3)]">{threadLabel}</span>}
        </div>
        <p className="mt-0.5 truncate text-[11.5px] leading-tight">
          {activity ? (
            <span className="text-[var(--color-accent-strong)]">{activity}…</span>
          ) : !online ? (
            <span className="text-[var(--color-ink-3)]">{phaseLabel(gateway?.status.phase)}</span>
          ) : channelLabel ? (
            <span className="text-[var(--color-ink-3)]">
              Replies go out on {channelLabel} as {name}
            </span>
          ) : kind === "routine" ? (
            <span className="text-[var(--color-ink-3)]">Routine run</span>
          ) : (
            <span className="flex items-center gap-1 text-[var(--color-ink-3)]">
              <span className="h-1.5 w-1.5 shrink-0 rounded-full bg-[var(--color-ok)]" /> online
              {showFolder && (
                <span className="truncate" title={folder}>
                  · in <span className="font-mono text-[11px]">{tildify(folder)}</span>
                </span>
              )}
            </span>
          )}
        </p>
      </div>
      {hasBrowser && (
        <IconButton
          label={browserOpen ? "Hide browser" : `Show ${name}'s browser`}
          onClick={() => useApp.getState().setBrowserOpen(guy.uid, !browserOpen)}
          className={browserOpen ? "bg-[var(--color-chip)] text-[var(--color-ink)]" : browsing ? "animate-pulse text-[var(--color-accent-strong)]" : ""}
        >
          <Globe size={13} />
        </IconButton>
      )}
      <IconButton label="Reload thread" onClick={() => useChat.getState().reload(threadKey)}>
        <RotateCw size={13} />
      </IconButton>
    </div>
  );
}

export function NoThread() {
  return (
    <div className="flex h-full flex-col items-center justify-center text-[var(--color-ink-3)]">
      <GuyMark size={80} />
    </div>
  );
}
