// Each guy's archived threads. The roster only carries the threads that aren't
// archived, so these come from their own `sessions.list {archived: true}`,
// fetched while a guy's threads are on screen.
import { create } from "zustand";
import { sessions as sessionsApi } from "../lib/gateway";
import { core } from "../lib/ipc";
import type { Guy, SessionRow } from "../lib/types";
import { agentIdFromKey } from "./app";

type GuyRef = Pick<Guy, "uid" | "gatewayId" | "id">;

interface ArchiveState {
  /** Archived threads by guy uid, newest first; missing until loaded. */
  byGuy: Record<string, SessionRow[]>;
  load(guy: GuyRef): Promise<void>;
  /** Put a thread back in the guy's threads. */
  unarchive(guy: GuyRef, row: SessionRow): Promise<void>;
}

/** The newest load per guy; an older one finishing late doesn't win. */
const latest: Record<string, number> = {};

export const useArchive = create<ArchiveState>((set, get) => ({
  byGuy: {},
  load: async (guy) => {
    const n = (latest[guy.uid] = (latest[guy.uid] ?? 0) + 1);
    let rows: SessionRow[];
    try {
      const res = await sessionsApi.archived(guy.gatewayId, guy.id);
      // A runtime that ignores `archived` answers with the other threads: none of them count.
      rows = res.sessions
        .filter((r) => r.archived && (r.agentId ?? agentIdFromKey(r.key)) === guy.id)
        .sort((a, b) => (b.updatedAt ?? 0) - (a.updatedAt ?? 0));
    } catch {
      // A gateway that can't list them has none to show.
      rows = get().byGuy[guy.uid] ?? [];
    }
    if (latest[guy.uid] === n) set((s) => ({ byGuy: { ...s.byGuy, [guy.uid]: rows } }));
  },
  unarchive: async (guy, row) => {
    await sessionsApi.patch(guy.gatewayId, row.key, { archived: false }, row.sessionId ?? undefined);
    // The roster has it again before it leaves the archive, so an open chat keeps its title.
    await core.refreshRoster(guy.gatewayId).catch(() => {});
    await get().load(guy);
  },
}));

/** An archived thread of the guy's, if `key` is one. */
export function useArchivedRow(guy: GuyRef, key: string): SessionRow | undefined {
  return useArchive((s) => s.byGuy[guy.uid]?.find((r) => r.key === key));
}
