// The OS littleguys runs on, read from the webview (WKWebView on macOS,
// WebView2 on Windows), and the words and keys that change with it.

const ua = typeof navigator === "undefined" ? "" : navigator.userAgent;

export const OS: "mac" | "windows" | "linux" = /Windows/.test(ua) ? "windows" : /Linux|X11/.test(ua) && !/Mac OS/.test(ua) ? "linux" : "mac";
export const IS_MAC = OS === "mac";

/** The machine, as in "on this Mac" / "on this PC". */
export const COMPUTER = { mac: "Mac", windows: "PC", linux: "computer" }[OS];

/** The app that shows files and folders. */
export const FILE_MANAGER = { mac: "Finder", windows: "File Explorer", linux: "Files" }[OS];

/** Where the little guy lives. */
export const TRAY = { mac: "menu bar", windows: "taskbar", linux: "panel" }[OS];

/** The key app shortcuts use: ⌘ on macOS, Ctrl elsewhere. */
export function isModKey(e: { metaKey: boolean; ctrlKey: boolean }): boolean {
  return IS_MAC ? e.metaKey : e.ctrlKey;
}

/** A shortcut as it's written here: `shortcut("L", true)` is ⇧⌘L on macOS, Ctrl+Shift+L elsewhere. */
export function shortcut(key: string, shift = false): string {
  return IS_MAC ? `${shift ? "⇧" : ""}⌘${key}` : `Ctrl+${shift ? "Shift+" : ""}${key}`;
}

if (typeof document !== "undefined") document.documentElement.dataset.os = OS;
