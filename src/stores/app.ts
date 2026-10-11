import { useMemo } from "react";
import { create } from "zustand";
import { activityFor, allGuys, defaultThread, findGuy, guyName, guyUid, liveGateways, threadKind } from "../lib/guys";
import { core, listen } from "../lib/ipc";
import type { ConnStatus, Guy, Navigate, Roster } from "../lib/types";

const ORDER_KEY = "littleguys.order";
const LAST_THREAD_KEY = "littleguys.lastThread";
const LAST_GUY_KEY = "littleguys.lastGuy";
/** How long launch waits for the last guy's gateway before showing someone else. */
const RESTORE_WAIT_MS = 8_000;
const BROWSER_KEY = "littleguys.browser";

function readJson<T>(key: string, fallback: T): T {
  try {
    const raw = localStorage.getItem(key);
    return raw ? (JSON.parse(raw) as T) : fallback;
  } catch {
    return fallback;
  }
}

/** What the main pane shows; "past" is the resume picker (ACP agents' sessions from every folder). */
export type MainView = "chat" | "routines" | "files" | "skills" | "past";

/** The browser panel beside the chat: open per guy, one shared width. */
interface BrowserPanelPrefs {
  open: Record<string, boolean>;
  width: number;
}

interface AppState {
  roster: Roster;
  /** User-chosen order of guys (uids) in the sidebar. */
  order: string[];
  /** Selected guy, as `${gatewayId}/${agentId}`. */
  selectedUid: string | null;
  selectedSessionKey: string | null;
  lastThreadByGuy: Record<string, string>;
  /** The guy you were on last time, reopened at launch once their gateway is up. */
  restoreUid: string | null;
  view: MainView;
  /**
   * The guy whose threads the sidebar shows (null = every guy). Browsing doesn't
   * change what's open; clicking a thread, run, file or skill there does.
   */
  browseUid: string | null;
  /** The sidebar's tab for the browsed guy (the main pane follows `view`). */
  sidebarTab: MainView;
  /** Open file in the Files tab, per guy (workspace-relative path). */
  fileByGuy: Record<string, string>;
  /** Routine run open in the Routines tab, per guy (null = the routines list). */
  routineRunByGuy: Record<string, string | null>;
  /** Selected item in the Skills tab, per guy (see lib/abilities.ts ids). */
  abilityByGuy: Record<string, string>;
  browser: BrowserPanelPrefs;
  editor: { open: boolean; uid: string | null };
  settingsOpen: boolean;
  addGatewayOpen: boolean;
  setStatus(status: ConnStatus): void;
  setRoster(roster: Roster): void;
  selectGuy(uid: string, sessionKey?: string | null): void;
  selectSession(key: string | null): void;
  setView(view: MainView): void;
  browse(uid: string | null): void;
  setSidebarTab(tab: MainView): void;
  /** Pick a guy's file (and show it, unless `open` is false). */
  selectFile(uid: string, path: string, open?: boolean): void;
  /** Open a routine run's transcript in the Routines tab (null = back to the list). */
  selectRoutineRun(key: string | null, uid?: string): void;
  /** Pick a guy's skill or tool (and show it, unless `open` is false). */
  selectAbility(uid: string, id: string, open?: boolean): void;
  /** Show the resume picker for a guy's past sessions. */
  openPast(uid: string): void;
  setBrowserOpen(uid: string, open: boolean): void;
  setBrowserWidth(width: number): void;
  setOrder(order: string[]): void;
  openEditor(uid: string | null): void;
  closeEditor(): void;
  setSettingsOpen(open: boolean): void;
  setAddGatewayOpen(open: boolean): void;
  navigate(nav: Navigate): void;
  /** Make a guy on a gateway that can, else add another kind of bot. */
  startNewGuy(): void;
}

const emptyRoster: Roster = { gateways: [], hidden: [], updatedAt: 0 };

export const useApp = create<AppState>((set, get) => ({
  roster: emptyRoster,
  order: readJson<string[]>(ORDER_KEY, []),
  selectedUid: null,
  selectedSessionKey: null,
  lastThreadByGuy: readJson<Record<string, string>>(LAST_THREAD_KEY, {}),
  restoreUid: readJson<string | null>(LAST_GUY_KEY, null),
  view: "chat",
  // At launch, the sidebar opens on the guy you were last on (if they show up).
  browseUid: readJson<string | null>(LAST_GUY_KEY, null),
  sidebarTab: "chat",
  fileByGuy: {},
  routineRunByGuy: {},
  abilityByGuy: {},
  browser: readJson<BrowserPanelPrefs>(BROWSER_KEY, { open: {}, width: 520 }),
  editor: { open: false, uid: null },
  settingsOpen: false,
  addGatewayOpen: false,

  setStatus: (status) =>
    set((s) => ({
      roster: {
        ...s.roster,
        gateways: s.roster.gateways.map((g) => (g.id === status.gatewayId ? { ...g, status } : g)),
      },
    })),
  setRoster: (roster) => {
    set({ roster });
    const guys = sortGuys(roster, get().order);
    const { selectedUid, restoreUid } = get();
    if (restoreUid) {
      if (guys.some((g) => g.uid === restoreUid)) {
        set({ restoreUid: null });
        return showGuy(restoreUid);
      }
      if (stillConnecting(roster, restoreUid)) return;
      set({ restoreUid: null });
    }
    if ((!selectedUid || !guys.some((g) => g.uid === selectedUid)) && guys.length) {
      showGuy(guys[0].uid);
    }
  },
  selectGuy: (uid, sessionKey) => {
    activate(uid);
    set({ browseUid: uid, sidebarTab: "chat" });
    showGuy(uid, sessionKey);
  },
  selectSession: (key) => {
    if (key && threadKind(key) === "routine") return get().selectRoutineRun(key);
    const uid = get().selectedUid;
    if (key && uid) {
      const lastThreadByGuy = { ...get().lastThreadByGuy, [uid]: key };
      localStorage.setItem(LAST_THREAD_KEY, JSON.stringify(lastThreadByGuy));
      set({ lastThreadByGuy });
    }
    set({ selectedSessionKey: key, view: "chat" });
  },
  setView: (view) => set({ view }),
  // Back in a guy you're on, the sidebar shows what's open; anyone else starts at their chats.
  browse: (uid) => set({ browseUid: uid, sidebarTab: uid && uid === get().selectedUid ? get().view : "chat" }),
  setSidebarTab: (sidebarTab) => set({ sidebarTab }),
  selectRoutineRun: (key, uid = get().selectedUid ?? undefined) => {
    if (!uid) return;
    activate(uid);
    set({ routineRunByGuy: { ...get().routineRunByGuy, [uid]: key }, view: "routines" });
  },
  selectFile: (uid, path, open = true) => {
    set({ fileByGuy: { ...get().fileByGuy, [uid]: path } });
    if (open) {
      activate(uid);
      set({ view: "files" });
    }
  },
  selectAbility: (uid, id, open = true) => {
    set({ abilityByGuy: { ...get().abilityByGuy, [uid]: id } });
    if (open) {
      activate(uid);
      set({ view: "skills" });
    }
  },
  openPast: (uid) => {
    activate(uid);
    set({ view: "past" });
  },
  setBrowserOpen: (uid, open) => {
    const browser = { ...get().browser, open: { ...get().browser.open, [uid]: open } };
    localStorage.setItem(BROWSER_KEY, JSON.stringify(browser));
    set({ browser });
  },
  setBrowserWidth: (width) => {
    const browser = { ...get().browser, width: Math.round(width) };
    localStorage.setItem(BROWSER_KEY, JSON.stringify(browser));
    set({ browser });
  },
  setOrder: (order) => {
    localStorage.setItem(ORDER_KEY, JSON.stringify(order));
    set({ order });
  },
  openEditor: (uid) => set({ editor: { open: true, uid } }),
  closeEditor: () => set({ editor: { open: false, uid: null } }),
  setSettingsOpen: (settingsOpen) => set({ settingsOpen }),
  setAddGatewayOpen: (addGatewayOpen) => set({ addGatewayOpen }),
  startNewGuy: () => {
    const canMake = liveGateways(get().roster).some((g) => g.status.capabilities?.manageGuys);
    set(canMake ? { editor: { open: true, uid: null } } : { addGatewayOpen: true });
  },
  navigate: (nav) => {
    if (nav.view === "settings") return set({ settingsOpen: true });
    if (nav.view === "new-guy") return get().startNewGuy();
    if (nav.view === "add-gateway") return set({ addGatewayOpen: true });
    const agentId = nav.agentId ?? (nav.sessionKey ? agentIdFromKey(nav.sessionKey) : null);
    // Routine runs open in the Routines tab, not as a chat.
    if (nav.gatewayId && agentId && nav.sessionKey && threadKind(nav.sessionKey) === "routine") {
      const uid = guyUid(nav.gatewayId, agentId);
      get().selectGuy(uid);
      get().selectRoutineRun(nav.sessionKey, uid);
      return set({ sidebarTab: "routines" });
    }
    if (nav.gatewayId && agentId) get().selectGuy(guyUid(nav.gatewayId, agentId), nav.sessionKey ?? undefined);
    if (nav.view === "routines" || nav.view === "files" || nav.view === "skills") set({ view: nav.view, sidebarTab: nav.view });
  },
}));

/**
 * Make a guy the one on screen (and the one to reopen at launch), keeping the
 * main pane's view. Their last thread becomes the open chat.
 */
function activate(uid: string) {
  localStorage.setItem(LAST_GUY_KEY, JSON.stringify(uid));
  useApp.setState({ restoreUid: null });
  if (useApp.getState().selectedUid === uid) return;
  const { lastThreadByGuy, roster } = useApp.getState();
  const last = lastThreadByGuy[uid];
  const gatewayId = uid.slice(0, uid.indexOf("/"));
  const agentId = uid.slice(uid.indexOf("/") + 1);
  const key = last && threadKind(last) !== "routine" ? last : defaultThread(roster, { gatewayId, id: agentId });
  useApp.setState({ selectedUid: uid, selectedSessionKey: key });
}

/** Show a guy (and their last thread) without making them the one to reopen. */
function showGuy(uid: string, sessionKey?: string | null) {
  const { lastThreadByGuy, roster, selectSession } = useApp.getState();
  // Routine runs live in the Routines tab, not in the chats.
  const last = lastThreadByGuy[uid];
  const remembered = last && threadKind(last) !== "routine" ? last : undefined;
  const gatewayId = uid.slice(0, uid.indexOf("/"));
  const agentId = uid.slice(uid.indexOf("/") + 1);
  const key = sessionKey ?? remembered ?? defaultThread(roster, { gatewayId, id: agentId });
  useApp.setState({ selectedUid: uid, selectedSessionKey: key });
  selectSession(key);
}

/** Could this guy still show up (their gateway is on its way up)? */
function stillConnecting(roster: Roster, uid: string): boolean {
  const g = roster.gateways.find((g) => g.id === uid.slice(0, uid.indexOf("/")));
  if (!g?.enabled || roster.hidden.includes(uid)) return false;
  const { phase } = g.status;
  return phase === "starting" || phase === "connecting" || (phase === "connected" && !g.loaded);
}

export function mainSessionKey(agentId: string): string {
  return `agent:${agentId}:main`;
}

export function agentIdFromKey(key: string): string | null {
  const parts = key.split(":");
  return parts[0] === "agent" && parts[1] ? parts[1] : null;
}

/** Guys with the newest message first, like a chat list; quiet ones keep display order. */
export function recentGuys(roster: Roster, order: string[]): Guy[] {
  return sortGuys(roster, order)
    .map((guy) => ({ guy, at: activityFor(roster, guy).latest?.updatedAt ?? 0 }))
    .sort((a, b) => b.at - a.at)
    .map(({ guy }) => guy);
}

/** Guys in display order: user order, then gateway order, default guy first, then by name. */
export function sortGuys(roster: Roster, order: string[]): Guy[] {
  const gwIndex = new Map(roster.gateways.map((g, i) => [g.id, i]));
  const defaults = new Set(roster.gateways.map((g) => g.defaultAgentId && guyUid(g.id, g.defaultAgentId)));
  const rank = (uid: string) => {
    const i = order.indexOf(uid);
    return i === -1 ? Number.MAX_SAFE_INTEGER : i;
  };
  return allGuys(roster).sort(
    (a, b) =>
      rank(a.uid) - rank(b.uid) ||
      (gwIndex.get(a.gatewayId) ?? 0) - (gwIndex.get(b.gatewayId) ?? 0) ||
      Number(defaults.has(b.uid)) - Number(defaults.has(a.uid)) ||
      guyName(a).localeCompare(guyName(b)),
  );
}

/** The selected guy (or undefined while loading). */
export function useSelectedGuy(): Guy | undefined {
  const roster = useApp((s) => s.roster);
  const uid = useApp((s) => s.selectedUid);
  return useMemo(() => findGuy(roster, uid), [roster, uid]);
}

/** Wire the store to the Rust core. Call once per window. */
export async function connectAppStore() {
  const { setStatus, setRoster } = useApp.getState();
  await Promise.all([listen<ConnStatus>("gw:status", setStatus), listen<Roster>("lg:roster", setRoster)]);
  setRoster(await core.roster());
  // Don't wait forever on a gateway that's slow to come up.
  setTimeout(() => {
    if (!useApp.getState().restoreUid) return;
    useApp.setState({ restoreUid: null });
    setRoster(useApp.getState().roster);
  }, RESTORE_WAIT_MS);
}
