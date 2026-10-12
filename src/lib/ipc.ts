// Bridge to the Rust core. Every gateway call goes through `gw_call`; events
// arrive as Tauri events. Outside Tauri (plain `pnpm dev` in a browser) a mock
// backend with fixture data is used so the UI can be iterated on directly.
import type {
  AcpConfig,
  ClaudeCodeConfig,
  ClaudeCodeProbe,
  ConnStatus,
  DiscoveredGateway,
  DoctorReport,
  GatewayEntry,
  HermesConfig,
  HermesProbe,
  Navigate,
  RegistryAgent,
  Roster,
  UpdateState,
} from "./types";

export interface RpcError {
  code: string;
  message: string;
  details?: unknown;
}

type Unlisten = () => void;

export interface Backend {
  invoke<T>(cmd: string, args?: Record<string, unknown>): Promise<T>;
  listen<T>(event: string, cb: (payload: T) => void): Promise<Unlisten>;
}

const inTauri = typeof window !== "undefined" && "__TAURI_INTERNALS__" in window;

let backendPromise: Promise<Backend> | null = null;
function backend(): Promise<Backend> {
  backendPromise ??= inTauri
    ? Promise.all([import("@tauri-apps/api/core"), import("@tauri-apps/api/event")]).then(([core, event]) => ({
        invoke: <T,>(cmd: string, args?: Record<string, unknown>) => core.invoke<T>(cmd, args),
        listen: <T,>(name: string, cb: (payload: T) => void) => event.listen<T>(name, (e) => cb(e.payload)),
      }))
    : import("./mock").then((m) => m.mockBackend);
  return backendPromise;
}

export const isTauri = inTauri;

export async function invoke<T>(cmd: string, args?: Record<string, unknown>): Promise<T> {
  return (await backend()).invoke<T>(cmd, args);
}

export async function listen<T>(event: string, cb: (payload: T) => void): Promise<Unlisten> {
  return (await backend()).listen<T>(event, cb);
}

/** Generic gateway RPC against one gateway. Rejects with an RpcError. */
export function gwCall<T = unknown>(gateway: string, method: string, params: object = {}, timeoutMs?: number): Promise<T> {
  return invoke<T>("gw_call", { gateway, method, params, timeoutMs });
}

/** One message from a guy's live browser stream (see src-tauri/src/browser_stream.rs). */
export type BrowserStreamMessage =
  | { kind: "text"; data: { type?: string; targetId?: string; url?: string; title?: string; message?: string; code?: string } }
  | { kind: "frame"; header: { url?: string; cssWidth: number; cssHeight: number; scrollX?: number; scrollY?: number }; jpeg: Uint8Array }
  | { kind: "ended"; code: number; reason: string };

export function decodeStreamMessage(buf: ArrayBuffer): BrowserStreamMessage {
  const bytes = new Uint8Array(buf);
  const text = (from: number, to?: number) => new TextDecoder().decode(bytes.subarray(from, to));
  if (bytes[0] === 1) {
    const len = new DataView(buf, 1, 4).getUint32(0);
    return { kind: "frame", header: JSON.parse(text(5, 5 + len)), jpeg: bytes.subarray(5 + len) };
  }
  const data = JSON.parse(text(1));
  return bytes[0] === 0 ? { kind: "text", data } : { kind: "ended", code: data.code, reason: data.reason };
}

export interface BrowserStreamOptions {
  profile?: string;
  targetId?: string;
  maxWidth?: number;
  maxHeight?: number;
  quality?: number;
}

export function errorMessage(err: unknown): string {
  if (typeof err === "string") return err;
  if (err && typeof err === "object" && "message" in err) return String((err as { message: unknown }).message);
  return "Something went wrong";
}

export const core = {
  statuses: () => invoke<ConnStatus[]>("gw_statuses"),
  reconnect: (gateway?: string) => invoke<void>("gw_reconnect", { gateway: gateway ?? null }),
  setCredential: (gateway: string, kind: "token" | "password", value: string | null) =>
    invoke<void>("gw_set_credential", { gateway, kind, value }),
  roster: () => invoke<Roster>("get_roster"),
  refreshRoster: (gateway?: string) => invoke<Roster>("refresh_roster", { gateway: gateway ?? null }),
  setGuyHidden: (uid: string, hidden: boolean) => invoke<void>("set_guy_hidden", { uid, hidden }),
  /** A photo for a guy's face (a `data:image/…` URL), or `null` for the bot's own; kept on this Mac only. */
  setGuyFace: (uid: string, face: string | null) => invoke<void>("set_guy_face", { uid, face }),
  /** Save an OpenClaw guy's picture into their workspace (gateway on this Mac); resolves to the path for `avatar`. */
  setWorkspaceAvatar: (gateway: string, agentId: string, face: string | null) =>
    invoke<string | null>("set_workspace_avatar", { gateway, agentId, face }),
  setFocusedSession: (gateway: string | null, key: string | null) =>
    invoke<void>("set_focused_session", { gateway, key }),
  openMain: (nav?: Navigate) =>
    invoke<void>("open_main", {
      gateway: nav?.gatewayId ?? null,
      agentId: nav?.agentId ?? null,
      sessionKey: nav?.sessionKey ?? null,
      view: nav?.view ?? null,
    }),
  gateways: {
    add: (url: string, name?: string, kind?: "token" | "password", secret?: string) =>
      invoke<GatewayEntry>("gateway_add", { url, name: name ?? null, kind: kind ?? null, secret: secret ?? null }),
    update: (id: string, patch: { name?: string; enabled?: boolean }) =>
      invoke<void>("gateway_update", { id, name: patch.name ?? null, enabled: patch.enabled ?? null }),
    remove: (id: string) => invoke<void>("gateway_remove", { id }),
    discover: () => invoke<DiscoveredGateway[]>("gateways_discover"),
    /** Connect with an OpenClaw setup code; resolves to the gateway id. */
    useSetupCode: (code: string) => invoke<string>("gateway_setup_code", { code }),
  },
  bots: {
    addAcp: (acp: AcpConfig, name?: string, secret?: string) =>
      invoke<GatewayEntry>("bot_add", { kind: "acp", name: name ?? null, acp, hermes: null, claude: null, secretKind: secret ? "token" : null, secret: secret ?? null }),
    addHermes: (hermes: HermesConfig, name?: string, apiKey?: string) =>
      invoke<GatewayEntry>("bot_add", { kind: "hermes", name: name ?? null, acp: null, hermes, claude: null, secretKind: apiKey ? "token" : null, secret: apiKey ?? null }),
    acpRegistry: () => invoke<RegistryAgent[]>("acp_registry"),
    hermesDetect: () => invoke<HermesProbe[]>("hermes_detect"),
    addClaudeCode: (claude: ClaudeCodeConfig) =>
      invoke<GatewayEntry>("bot_add", { kind: "claudecode", name: null, acp: null, hermes: null, claude, secretKind: null, secret: null }),
    claudeDetect: () => invoke<ClaudeCodeProbe>("claude_code_detect"),
    /** Installs (or repairs) the littleguys plugin in Claude Code. */
    claudeInstallPlugin: () => invoke<void>("claude_code_install_plugin"),
    claudeConfigure: (id: string, claude: ClaudeCodeConfig) => invoke<void>("claude_code_configure", { id, claude }),
  },
  hidePopover: () => invoke<void>("hide_popover"),
  fitPopover: (height: number) => invoke<void>("fit_popover", { height }),
  revealPath: (path: string) => invoke<void>("reveal_path", { path }),
  /**
   * A file a guy referenced by path (`MEDIA:/…`, or one it mentions), as a
   * short-lived URL the webview can load, or (a local guy's) its `path` on this Mac.
   */
  media: (gateway: string, source: string, sessionKey?: string, agentId?: string) =>
    invoke<{ available: boolean; url?: string; path?: string; mimeType?: string; sizeBytes?: number; expiresAt?: string; code?: string; reason?: string }>(
      "gw_media",
      { gateway, source, sessionKey: sessionKey ?? null, agentId: agentId ?? null },
    ),
  /** A file or folder a guy on this Mac mentions, if it's there; null for guys whose files are elsewhere. */
  path: (gateway: string, source: string, sessionKey?: string, agentId?: string) =>
    invoke<{ path: string; isDir: boolean; folder: string } | null>("gw_path", {
      gateway,
      source,
      sessionKey: sessionKey ?? null,
      agentId: agentId ?? null,
    }),
  /** The URL the webview loads a file on this Mac from, once `media` has allowed it. */
  fileSrc: async (path: string) => (await import("@tauri-apps/api/core")).convertFileSrc(path),
  /** Save a chat attachment (http(s) or data: URL) to ~/Downloads; resolves to its path. */
  saveMedia: (url: string, fileName?: string, gateway?: string) =>
    invoke<string>("save_media", { url, fileName: fileName ?? null, gateway: gateway ?? null }),
  browser: {
    /** Relay a guy's browser tab; resolves once streaming, with a `stop()`. */
    stream: async (gateway: string, options: BrowserStreamOptions, onMessage: (m: BrowserStreamMessage) => void) => {
      const channel: { onmessage: (buf: ArrayBuffer) => void } = inTauri
        ? new (await import("@tauri-apps/api/core")).Channel<ArrayBuffer>()
        : { onmessage: () => {} };
      channel.onmessage = (buf) => {
        try {
          onMessage(decodeStreamMessage(buf));
        } catch {
          // a malformed frame; skip it
        }
      };
      const started = await invoke<{ id: number; targetId?: string; url?: string }>("browser_stream_start", {
        gateway,
        options,
        onMessage: channel,
      });
      return { ...started, stop: () => invoke<void>("browser_stream_stop", { id: started.id }).catch(() => {}) };
    },
    /** A still of a tab, for browsers that can't stream. */
    screenshot: (gateway: string, profile?: string, targetId?: string) =>
      invoke<{ src: string; targetId?: string; url?: string }>("browser_screenshot", {
        gateway,
        profile: profile ?? null,
        targetId: targetId ?? null,
      }),
  },
  quit: () => invoke<void>("quit"),
  updates: {
    state: () => invoke<UpdateState>("update_state"),
    /** Check now; downloads and installs a newer release if there is one. */
    check: () => invoke<UpdateState>("update_check"),
    restart: () => invoke<void>("update_restart"),
  },
  doctor: {
    check: () => invoke<DoctorReport>("doctor_check"),
    install: (version?: string) => invoke<void>("doctor_install_openclaw", { version: version ?? null }),
    update: () => invoke<void>("doctor_update_openclaw"),
    startGateway: () => invoke<void>("doctor_start_gateway"),
    onboard: (authChoice: string, apiKey: string | null, agentName: string | null) =>
      invoke<void>("doctor_onboard", { authChoice, apiKey, agentName }),
    gatewayLog: (lines = 80) => invoke<string | null>("doctor_gateway_log", { lines }),
  },
};
