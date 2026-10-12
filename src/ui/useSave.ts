// Saving a chat attachment to ~/Downloads (the webview can't download on its
// own), with brief feedback on whatever button triggered it. Once saved, the
// same button shows the file in Finder, as it does from the start for a file
// that's already on this Mac.
import { Check, Download, FolderOpen, type LucideIcon, TriangleAlert } from "lucide-react";
import { useEffect, useRef, useState } from "react";
import { core, errorMessage } from "../lib/ipc";

type SaveStatus =
  | { status: "idle" | "saving" }
  /** `saved` flashes a checkmark, then settles into `done` ("Show in Finder"). */
  | { status: "saved" | "done"; path: string; local?: boolean }
  | { status: "failed"; error: string };

export type Save = SaveStatus & { run: () => void };

export function useSave(
  resolveUrl: () => Promise<string | null>,
  fileName: string | undefined,
  gatewayId: string,
  /** Where the file already is on this Mac: nothing to download. */
  localPath?: string,
): Save {
  const [progress, setState] = useState<SaveStatus>({ status: "idle" });
  const state: SaveStatus = localPath && progress.status === "idle" ? { status: "done", path: localPath, local: true } : progress;
  const timer = useRef<ReturnType<typeof setTimeout>>(undefined);
  useEffect(() => () => clearTimeout(timer.current), []);

  const later = (next: SaveStatus, ms: number) => {
    clearTimeout(timer.current);
    timer.current = setTimeout(() => setState(next), ms);
  };

  const run = () => {
    if (state.status === "saving") return;
    if (state.status === "saved" || state.status === "done") {
      const path = state.path;
      import("@tauri-apps/plugin-opener")
        .then(({ revealItemInDir }) => revealItemInDir(path))
        // Moved or deleted since: save it again next click.
        .catch(() => setState({ status: "idle" }));
      return;
    }
    setState({ status: "saving" });
    resolveUrl()
      .then((url) => {
        if (!url) throw new Error("The file isn't available yet.");
        return core.saveMedia(originalUrl(url), fileName, gatewayId);
      })
      .then(
        (path) => {
          setState({ status: "saved", path });
          later({ status: "done", path }, 1800);
        },
        (e) => {
          setState({ status: "failed", error: errorMessage(e) });
          later({ status: "idle" }, 4000);
        },
      );
  };

  return { ...state, run };
}

export function saveLabel(save: Save): string {
  switch (save.status) {
    case "saving":
      return "Saving…";
    case "saved":
      return "Saved";
    case "done":
      return "Show in Finder";
    case "failed":
      return "Couldn't save";
    default:
      return "Download";
  }
}

export function saveIcon(save: Save): LucideIcon {
  if (save.status === "saved") return Check;
  if (save.status === "done") return FolderOpen;
  if (save.status === "failed") return TriangleAlert;
  return Download;
}

export function saveTitle(save: Save): string | undefined {
  if (save.status === "saved" || save.status === "done") return save.local ? save.path : `Saved to ${save.path}`;
  if (save.status === "failed") return save.error;
  return undefined;
}

/** The file itself, not the gateway's transcode for playback (`gw_media` adds `playback=1`). */
function originalUrl(url: string): string {
  if (!/^https?:/i.test(url)) return url;
  try {
    const u = new URL(url);
    u.searchParams.delete("playback");
    return u.toString();
  } catch {
    return url;
  }
}
