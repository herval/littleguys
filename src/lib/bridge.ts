// Paired phones (src-tauri/src/bridge): Settings → Phones and the pairing QR.
import { invoke, listen } from "./ipc";

export type Via = "wifi" | "tailscale" | "local";

export interface PhoneState {
  key: string;
  name: string;
  model: string;
  pairedAtMs: number;
  lastSeenAtMs: number;
  connected: boolean;
  via: Via | null;
  /** Registered for pushes (through the relay). */
  push: boolean;
}

export interface BridgeAddress {
  host: string;
  port: number;
  kind: "lan" | "tailnet" | "loopback";
}

export interface PairingState {
  uri: string;
  expiresAtMs: number;
  pairedWith: { key: string; name: string } | null;
}

export interface BridgeState {
  /** "Let paired phones connect". */
  enabled: boolean;
  listening: boolean;
  port: number | null;
  macName: string;
  macId: string;
  addresses: BridgeAddress[];
  /** Where pushes go; null when push is off. */
  relayUrl: string | null;
  phones: PhoneState[];
  pairing: PairingState | null;
}

export const bridge = {
  state: () => invoke<BridgeState>("bridge_state"),
  setEnabled: (on: boolean) => invoke<BridgeState>("bridge_set_enabled", { on }),
  /** A fresh one-time pairing link (show it as a QR code). */
  pairStart: () => invoke<{ uri: string; expiresAt: number }>("bridge_pair_start"),
  pairCancel: () => invoke<void>("bridge_pair_cancel"),
  removePhone: (key: string) => invoke<void>("bridge_phone_remove", { key }),
  renamePhone: (key: string, name: string) => invoke<void>("bridge_phone_rename", { key, name }),
  onState: (cb: (state: BridgeState) => void) => listen<BridgeState>("bridge:state", cb),
};

// ---- mock backend (`make mock`): pairing "succeeds" a few seconds after the QR shows ----

const MOCK_KEY = "bW9jay1waG9uZS1rZXktYmFzZTY0dXJsLW5vLXBhZGRpbmc";
const mock: BridgeState = {
  enabled: true,
  listening: true,
  port: 47100,
  macName: "Herval's MacBook Pro",
  macId: "0f3c9a1b2d4e5f60718293a4b5c6d7e8",
  addresses: [
    { host: "192.168.1.20", port: 47100, kind: "lan" },
    { host: "herval-mbp.tail1234.ts.net", port: 47100, kind: "tailnet" },
  ],
  relayUrl: "https://littleguys-relay.vercel.app",
  phones: [
    { key: "b2xkLXBob25l", name: "Work iPhone", model: "iPhone16,2", pairedAtMs: Date.now() - 9 * 86_400_000, lastSeenAtMs: Date.now() - 86_400_000, connected: false, via: null, push: true },
  ],
  pairing: null,
};
let mockTimer: number | undefined;

/** `bridge_*` commands for src/lib/mock.ts. */
export async function bridgeMockInvoke(cmd: string, args: Record<string, unknown> | undefined, emit: (event: string, payload: unknown) => void): Promise<unknown> {
  const publish = () => emit("bridge:state", structuredClone(mock));
  switch (cmd) {
    case "bridge_state":
      return structuredClone(mock);
    case "bridge_set_enabled":
      mock.enabled = Boolean(args?.on);
      mock.listening = mock.enabled;
      publish();
      return structuredClone(mock);
    case "bridge_pair_start": {
      await new Promise((r) => setTimeout(r, 300));
      const expiresAt = Date.now() + 10 * 60_000;
      const uri = `littleguys://pair?v=1&name=Herval%27s%20MacBook%20Pro&id=${"A".repeat(43)}&nk=${"B".repeat(43)}&addr=192.168.1.20:47100,herval-mbp.tail1234.ts.net:47100&secret=${crypto.randomUUID().replace(/-/g, "")}`;
      mock.enabled = mock.listening = true;
      mock.pairing = { uri, expiresAtMs: expiresAt, pairedWith: null };
      publish();
      clearTimeout(mockTimer);
      mockTimer = window.setTimeout(() => {
        const phone: PhoneState = { key: MOCK_KEY, name: "Herval's iPhone", model: "iPhone17,1", pairedAtMs: Date.now(), lastSeenAtMs: Date.now(), connected: true, via: "wifi", push: false };
        mock.phones = [...mock.phones.filter((p) => p.key !== MOCK_KEY), phone];
        if (mock.pairing) mock.pairing = { ...mock.pairing, pairedWith: { key: phone.key, name: phone.name } };
        publish();
      }, 4_000);
      return { uri, expiresAt };
    }
    case "bridge_pair_cancel":
      clearTimeout(mockTimer);
      mock.pairing = null;
      publish();
      return undefined;
    case "bridge_phone_remove":
      mock.phones = mock.phones.filter((p) => p.key !== args?.key);
      publish();
      return undefined;
    case "bridge_phone_rename":
      mock.phones = mock.phones.map((p) => (p.key === args?.key ? { ...p, name: String(args?.name ?? p.name) } : p));
      publish();
      return undefined;
    default:
      return undefined;
  }
}
