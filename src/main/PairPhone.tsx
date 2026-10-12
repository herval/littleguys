import { Check, Copy, RotateCw, Smartphone } from "lucide-react";
import QRCode from "qrcode";
import { useEffect, useState } from "react";
import { bridge, type BridgeState } from "../lib/bridge";
import { errorMessage } from "../lib/ipc";
import { Button, Modal, Spinner } from "../ui/kit";

/** The pairing QR code: a one-time link the iPhone app scans (valid 10 minutes). */
export function PairPhone({ state, onClose }: { state: BridgeState; onClose: () => void }) {
  const [start, setStart] = useState<{ uri: string; expiresAt: number } | null>(null);
  const [svg, setSvg] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [now, setNow] = useState(Date.now());
  const [copied, setCopied] = useState(false);

  const newCode = () => {
    setError(null);
    setSvg(null);
    bridge.pairStart().then(setStart).catch((e) => setError(errorMessage(e)));
  };

  useEffect(() => {
    newCode();
    const tick = window.setInterval(() => setNow(Date.now()), 1000);
    return () => window.clearInterval(tick);
  }, []);

  useEffect(() => {
    if (!start) return;
    QRCode.toString(start.uri, { type: "svg", margin: 1, errorCorrectionLevel: "M" })
      .then(setSvg)
      .catch((e) => setError(errorMessage(e)));
  }, [start]);

  const close = () => {
    bridge.pairCancel().catch(() => {});
    onClose();
  };

  const paired = start && state.pairing?.uri === start.uri ? state.pairing.pairedWith : null;
  const left = start ? Math.max(0, Math.round((start.expiresAt - now) / 1000)) : 0;
  const expired = Boolean(start) && left === 0 && !paired;
  const lan = state.addresses.filter((a) => a.kind === "lan").map((a) => a.host);
  const tailnet = state.addresses.find((a) => a.kind === "tailnet" && /[a-z]/i.test(a.host))?.host ?? state.addresses.find((a) => a.kind === "tailnet")?.host;

  return (
    <Modal onClose={close} width={460}>
      <div className="p-6">
        <h2 className="font-display text-[17px] font-semibold tracking-tight">Pair mobile app</h2>
        {paired ? (
          <div className="mt-6 flex flex-col items-center gap-3 pb-2 text-center">
            <span className="flex h-12 w-12 items-center justify-center rounded-full bg-[var(--color-ok)]/15 text-[var(--color-ok)]">
              <Check size={24} />
            </span>
            <p className="text-[14px] font-medium">Paired with {paired.name}</p>
            <p className="text-[12px] text-[var(--color-ink-3)]">Your guys on this Mac are on your phone now.</p>
          </div>
        ) : (
          <>
            <ol className="mt-3 space-y-1 text-[12.5px] text-[var(--color-ink-2)]">
              <li>1. Get littleguys on your iPhone.</li>
              <li>
                2. Tap <span className="font-medium text-[var(--color-ink)]">Pair with your Mac</span>.
              </li>
              <li>3. Scan this.</li>
            </ol>
            <div className="mt-4 flex justify-center">
              <div className={`relative flex h-[220px] w-[220px] items-center justify-center rounded-xl bg-white p-2 ${expired ? "opacity-25" : ""}`}>
                {svg ? <div className="h-full w-full [&>svg]:h-full [&>svg]:w-full" dangerouslySetInnerHTML={{ __html: svg }} /> : !error && <Spinner size={20} />}
              </div>
            </div>
            <div className="mt-2 flex items-center justify-center gap-2 text-[11.5px] text-[var(--color-ink-3)]">
              {expired ? (
                <span>This code expired.</span>
              ) : start ? (
                <span>
                  Works once, for {Math.floor(left / 60)}:{String(left % 60).padStart(2, "0")}
                </span>
              ) : null}
              <Button size="sm" variant="ghost" onClick={newCode}>
                <RotateCw size={11} /> New code
              </Button>
              {start && (
                <Button
                  size="sm"
                  variant="ghost"
                  onClick={() => {
                    navigator.clipboard.writeText(start.uri);
                    setCopied(true);
                    window.setTimeout(() => setCopied(false), 1500);
                  }}
                >
                  {copied ? <Check size={11} /> : <Copy size={11} />} {copied ? "Copied" : "Copy link"}
                </Button>
              )}
            </div>
            <p className="mt-4 flex items-start gap-2 text-[11.5px] text-[var(--color-ink-3)]">
              <Smartphone size={13} className="mt-[1px] shrink-0" />
              <span>
                {lan.length || tailnet ? (
                  <>
                    Your phone reaches this Mac
                    {lan.length ? <> on Wi-Fi ({lan[0]})</> : null}
                    {lan.length && tailnet ? " and" : null}
                    {tailnet ? <> over Tailscale ({tailnet.split(".")[0]})</> : null}.
                  </>
                ) : (
                  "Looking for this Mac's addresses…"
                )}
                {!tailnet && " Without Tailscale, only on the same Wi-Fi. Install Tailscale on both to reach it from anywhere."}
              </span>
            </p>
            <p className="mt-2 text-[11.5px] text-[var(--color-ink-3)]">
              A paired phone can do everything littleguys can here: chat, OK commands, and edit guys and routines.
            </p>
          </>
        )}
        {error && <p className="mt-3 text-[11.5px] text-[var(--color-bad)]">{error}</p>}
        <div className="mt-5 flex justify-end">
          <Button variant={paired ? "primary" : "secondary"} onClick={close}>
            {paired ? "Done" : "Cancel"}
          </Button>
        </div>
      </div>
    </Modal>
  );
}
