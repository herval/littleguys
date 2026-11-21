// How the sidebar lists your chats (lib/threadGroups.ts), and the folders you file them in.
// Kept on this computer: none of it is the agents' business, so none of it goes to a runtime.
import { create } from "zustand";
import { type Folder, GROUP_BYS, type GroupBy } from "../lib/threadGroups";

const GROUP_KEY = "littleguys.groupBy";
const FOLDERS_KEY = "littleguys.folders";
const CLOSED_KEY = "littleguys.closedGroups";

function read<T>(key: string, fallback: T): T {
  try {
    const raw = localStorage.getItem(key);
    return raw ? (JSON.parse(raw) as T) : fallback;
  } catch {
    return fallback;
  }
}

function write(key: string, value: unknown) {
  try {
    localStorage.setItem(key, JSON.stringify(value));
  } catch {
    // Not kept, then.
  }
}

/** The grouping last picked (agent until you pick another). */
export function readGroupBy(): GroupBy {
  const g = read<string>(GROUP_KEY, "agent");
  return (GROUP_BYS as string[]).includes(g) ? (g as GroupBy) : "agent";
}

interface Filing {
  folders: Folder[];
  /** Thread (`gateway|sessionKey`) → the folder it's in. */
  filed: Record<string, string>;
}

interface SidebarState extends Filing {
  groupBy: GroupBy;
  /** `groupBy:groupId` → closed. */
  closed: Record<string, boolean>;
  /** The folder whose name is being typed (a new one starts that way). */
  naming: string | null;
  setNaming(id: string | null): void;
  setGroupBy(groupBy: GroupBy): void;
  /** A new folder (its id). */
  addFolder(name: string): string;
  renameFolder(id: string, name: string): void;
  /** The folder goes; its threads go back to everything else. */
  removeFolder(id: string): void;
  /** Put a thread in a folder (null: out of the one it's in). */
  file(threadId: string, folderId: string | null): void;
  toggle(key: string): void;
}

export const useSidebar = create<SidebarState>((set, get) => {
  const saveFiling = (next: Filing) => {
    write(FOLDERS_KEY, next);
    set(next);
  };
  const filing = read<Filing>(FOLDERS_KEY, { folders: [], filed: {} });
  return {
    groupBy: readGroupBy(),
    folders: filing.folders ?? [],
    filed: filing.filed ?? {},
    closed: read<Record<string, boolean>>(CLOSED_KEY, {}),
    naming: null,
    setNaming: (naming) => set({ naming }),
    setGroupBy: (groupBy) => {
      write(GROUP_KEY, groupBy);
      set({ groupBy });
    },
    addFolder: (name) => {
      const id = `f${Date.now().toString(36)}${Math.random().toString(36).slice(2, 6)}`;
      saveFiling({ folders: [...get().folders, { id, name }], filed: get().filed });
      return id;
    },
    renameFolder: (id, name) =>
      saveFiling({ folders: get().folders.map((f) => (f.id === id ? { ...f, name } : f)), filed: get().filed }),
    removeFolder: (id) =>
      saveFiling({
        folders: get().folders.filter((f) => f.id !== id),
        filed: Object.fromEntries(Object.entries(get().filed).filter(([, folder]) => folder !== id)),
      }),
    file: (threadId, folderId) => {
      const filed = { ...get().filed };
      if (folderId) filed[threadId] = folderId;
      else delete filed[threadId];
      saveFiling({ folders: get().folders, filed });
    },
    toggle: (key) => {
      const closed = { ...get().closed, [key]: !get().closed[key] };
      write(CLOSED_KEY, closed);
      set({ closed });
    },
  };
});
