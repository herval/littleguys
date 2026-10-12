import { Camera, Check, Globe, ImagePlus } from "lucide-react";
import { useEffect, useMemo, useRef, useState } from "react";
import { photoToAvatar } from "../lib/avatarImage";
import { agents as agentsApi, models as modelsApi } from "../lib/gateway";
import { hasPicture, pictureHome, setPicture } from "../lib/guyPicture";
import { giveOwnBrowser, hasOwnBrowser, identityMarkdown, PERSONALITIES, readPersonality, writePersonality } from "../lib/guySetup";
import { findGuy, guyAvatarUrl, guyEmoji, guyName, guyUid, liveGateways, splitUid } from "../lib/guys";
import { core, errorMessage } from "../lib/ipc";
import type { AgentSummary, ModelChoice } from "../lib/types";
import { mainSessionKey, useApp } from "../stores/app";
import { Avatar } from "../ui/Avatar";
import { confirmAction } from "../ui/confirm";
import { Button, Field, inputBase, inputClass, Modal, Spinner } from "../ui/kit";

const EMOJIS = ["🦊", "🐙", "🐢", "🦉", "🐝", "🐸", "🦦", "🐼", "🦄", "🐳", "🦖", "🐧", "🐨", "🦔", "🐞", "🤖", "👾", "🧙", "🧑‍🚀", "🌵", "🍄", "⚡️", "🔮", "🎩"];

export function GuyEditor({ uid }: { uid: string | null }) {
  const close = useApp((s) => s.closeEditor);
  const roster = useApp((s) => s.roster);
  const selectedUid = useApp((s) => s.selectedUid);
  const existing = useMemo(() => findGuy(roster, uid), [roster, uid]);
  const isNew = !existing;
  const agentId = existing?.id ?? null;
  const gateways = liveGateways(roster).filter((g) => g.status.capabilities?.manageGuys);
  const [gw, setGw] = useState<string>(
    () => existing?.gatewayId ?? (selectedUid ? splitUid(selectedUid).gatewayId : null) ?? gateways[0]?.id ?? "",
  );

  const [name, setName] = useState(existing ? guyName(existing) : "");
  const [emoji, setEmoji] = useState<string>(existing ? (guyEmoji(existing) ?? "") : EMOJIS[Math.floor(Math.random() * 12)]);
  // Their picture, separate from the emoji (which is what OpenClaw uses where only text fits).
  const [savedPhoto] = useState(() => (existing && hasPicture(roster, existing) ? guyAvatarUrl(existing) : null));
  const [photo, setPhoto] = useState<string | null>(savedPhoto);
  const [dropping, setDropping] = useState(false);
  const fileRef = useRef<HTMLInputElement>(null);
  const [personality, setPersonality] = useState(isNew ? PERSONALITIES[0].text : "");
  const [model, setModel] = useState<string>(currentModel(existing));
  const [ownBrowser, setOwnBrowser] = useState(true);
  const [hadBrowser, setHadBrowser] = useState(false);
  const [models, setModels] = useState<ModelChoice[]>([]);
  const [loadingExtras, setLoadingExtras] = useState(!isNew);
  const [step, setStep] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [deleteFiles, setDeleteFiles] = useState(false);

  useEffect(() => {
    if (!gw) return;
    modelsApi
      .list(gw, agentId ?? undefined)
      .then((r) => setModels(r.models ?? []))
      .catch(() => setModels([]));
    if (agentId) {
      Promise.all([readPersonality(gw, agentId), hasOwnBrowser(gw, agentId)])
        .then(([p, b]) => {
          setPersonality(p);
          setOwnBrowser(b);
          setHadBrowser(b);
        })
        .finally(() => setLoadingExtras(false));
    }
  }, [gw, agentId]);

  const preview = useMemo<AgentSummary>(
    () => ({ id: agentId ?? (name.trim() || "new"), name, identity: { name, emoji, avatarUrl: photo ?? undefined } }) as AgentSummary,
    [agentId, name, emoji, photo],
  );

  const choosePhoto = async (file: File) => {
    setError(null);
    try {
      setPhoto(await photoToAvatar(file));
    } catch (e) {
      setError(errorMessage(e));
    }
  };
  const hostName = gateways.find((g) => g.id === gw)?.name;

  const save = async () => {
    const trimmed = name.trim();
    if (!trimmed) {
      setError("Your little guy needs a name.");
      return;
    }
    setError(null);
    try {
      if (isNew) {
        setStep("Creating…");
        const created = await agentsApi.create(gw, { name: trimmed, ...(emoji ? { emoji } : {}), ...(model ? { model } : {}) });
        const id = created.agentId;
        setStep("Writing their identity…");
        const identity = await agentsApi.getFile(gw, id, "IDENTITY.md").catch(() => null);
        await agentsApi
          .setFile(gw, id, "IDENTITY.md", identityMarkdown(trimmed, emoji || null, null), identity?.file?.hash)
          .catch(() => {});
        if (personality.trim()) await writePersonality(gw, id, personality);
        if (ownBrowser) {
          setStep("Setting up their browser…");
          await giveOwnBrowser(gw, id, trimmed).catch((e) => console.warn("browser profile", e));
        }
        const fresh = await core.refreshRoster(gw);
        if (photo) {
          setStep("Saving their picture…");
          await setPicture(fresh, { uid: guyUid(gw, id), id, gatewayId: gw, identity: undefined }, photo);
        }
        useApp.getState().selectGuy(guyUid(gw, id), mainSessionKey(id));
      } else {
        setStep("Saving…");
        const id = existing!.id;
        const prevModel = currentModel(existing);
        await agentsApi.update(gw, {
          agentId: id,
          name: trimmed,
          ...(emoji ? { emoji } : {}),
          ...(model !== prevModel ? { model: model || null } : {}),
        });
        if (photo !== savedPhoto) await setPicture(roster, existing!, photo);
        await writePersonality(gw, id, personality);
        if (ownBrowser && !hadBrowser) await giveOwnBrowser(gw, id, trimmed).catch(() => {});
        await core.refreshRoster(gw);
      }
      close();
    } catch (e) {
      setError(errorMessage(e));
      setStep(null);
    }
  };

  const remove = async () => {
    if (!existing) return;
    const ok = await confirmAction(
      deleteFiles
        ? `Delete ${guyName(existing)} and their workspace files? This can't be undone.`
        : `Delete ${guyName(existing)}? Their workspace folder is kept.`,
    );
    if (!ok) return;
    setStep("Deleting…");
    try {
      await agentsApi.remove(gw, existing.id, deleteFiles);
      // So a new guy that gets the same id doesn't start with this one's photo.
      if (roster.faces.includes(existing.uid)) await core.setGuyFace(existing.uid, null).catch(() => {});
      await core.refreshRoster(gw);
      close();
    } catch (e) {
      setError(errorMessage(e));
      setStep(null);
    }
  };

  const busy = step !== null;

  return (
    <Modal onClose={busy ? () => {} : close} width={560}>
      <div
        className="flex gap-5 p-6"
        onDragOver={(e) => {
          if (busy || !e.dataTransfer.types.includes("Files")) return;
          e.preventDefault();
          setDropping(true);
        }}
        onDragLeave={(e) => {
          if (!e.currentTarget.contains(e.relatedTarget as Node | null)) setDropping(false);
        }}
        onDrop={(e) => {
          if (!e.dataTransfer.files.length) return;
          e.preventDefault();
          setDropping(false);
          void choosePhoto(e.dataTransfer.files[0]);
        }}
      >
        <input
          ref={fileRef}
          type="file"
          accept="image/*"
          hidden
          onChange={(e) => {
            const file = e.target.files?.[0];
            if (file) void choosePhoto(file);
            e.target.value = "";
          }}
        />
        <div className="flex w-[120px] shrink-0 flex-col items-center pt-2">
          <button
            onClick={() => fileRef.current?.click()}
            disabled={busy}
            title={photo ? "Change photo" : "Use a photo"}
            className={`group relative rounded-full transition ${dropping ? "scale-105 ring-4 ring-[var(--color-accent)]/40" : ""}`}
          >
            <Avatar agent={preview} size={96} ring="idle" />
            <span className="absolute bottom-0 right-0 flex h-7 w-7 items-center justify-center rounded-full bg-[var(--color-pane-solid)] text-[var(--color-ink-2)] opacity-0 shadow ring-1 ring-[var(--color-line)] transition group-hover:opacity-100">
              <Camera size={14} />
            </span>
          </button>
          <p className="mt-2 max-w-full truncate text-center font-display text-[14px] font-semibold">{name || "New guy"}</p>
        </div>
        <div className="min-w-0 flex-1 space-y-4">
          <h2 className="font-display text-[17px] font-semibold tracking-tight">{isNew ? "Make a little guy" : `Edit ${guyName(existing!)}`}</h2>
          <Field label="Name">
            <input className={inputClass} value={name} onChange={(e) => setName(e.target.value)} placeholder="Fox" autoFocus={isNew} disabled={busy} />
          </Field>
          {isNew && gateways.length > 1 && (
            <Field label="Lives on" hint="Which gateway (computer) runs this guy.">
              <select className={inputClass} value={gw} onChange={(e) => setGw(e.target.value)} disabled={busy}>
                {gateways.map((g) => (
                  <option key={g.id} value={g.id}>
                    {g.name}
                  </option>
                ))}
              </select>
            </Field>
          )}
          <Field label="Look">
            <div className="flex flex-wrap gap-1">
              {EMOJIS.map((e) => (
                <button
                  key={e}
                  onClick={() => setEmoji(e)}
                  disabled={busy}
                  className={`flex h-8 w-8 items-center justify-center rounded-lg text-[18px] transition ${
                    emoji === e ? "bg-[var(--color-accent-soft)] ring-2 ring-[var(--color-accent)]" : "hover:bg-[var(--color-chip)]"
                  }`}
                >
                  {e}
                </button>
              ))}
              <input
                className={`${inputBase} h-8 w-14 text-center text-[16px]`}
                value={EMOJIS.includes(emoji) ? "" : emoji}
                onChange={(e) => setEmoji(e.target.value.trim())}
                placeholder="…"
                title="Any emoji"
                disabled={busy}
              />
            </div>
          </Field>
          <Field
            label="Picture"
            hint={
              pictureHome(roster, { gatewayId: gw }) === "openclaw"
                ? "Saved in their workspace, so OpenClaw shows it too."
                : `Kept in littleguys on this Mac: ${hostName ?? "their gateway"} is on another computer.`
            }
          >
            <div className="flex items-center gap-2">
              <button
                onClick={() => fileRef.current?.click()}
                disabled={busy}
                className="flex h-8 items-center gap-1.5 rounded-lg border border-[var(--color-line)] pl-1 pr-2.5 text-[12.5px] transition hover:border-[var(--color-ink-3)]/50"
              >
                {photo ? (
                  <img src={photo} alt="" className="h-6 w-6 rounded-md object-cover" draggable={false} />
                ) : (
                  <span className="flex h-6 w-6 items-center justify-center text-[var(--color-ink-3)]">
                    <ImagePlus size={15} />
                  </span>
                )}
                {photo ? "Change…" : "Choose a photo…"}
              </button>
              {photo && (
                <button
                  onClick={() => setPhoto(null)}
                  disabled={busy}
                  className="text-[12.5px] text-[var(--color-ink-3)] transition hover:text-[var(--color-ink)]"
                >
                  Remove
                </button>
              )}
            </div>
          </Field>
          <Field label="Personality" hint={loadingExtras ? "Loading…" : "Saved to their SOUL.md. They'll grow into it."}>
            <div className="mb-2 flex flex-wrap gap-1.5">
              {PERSONALITIES.map((p) => (
                <button
                  key={p.id}
                  onClick={() => setPersonality(p.text)}
                  disabled={busy}
                  className={`rounded-full border px-2.5 py-1 text-[12px] transition ${
                    personality === p.text
                      ? "border-[var(--color-accent)] bg-[var(--color-accent-soft)] text-[var(--color-accent-strong)]"
                      : "border-[var(--color-line)] hover:border-[var(--color-ink-3)]/50"
                  }`}
                >
                  {p.emoji} {p.label}
                </button>
              ))}
            </div>
            <textarea
              className={`${inputClass} min-h-[72px] resize-y`}
              value={personality}
              onChange={(e) => setPersonality(e.target.value)}
              placeholder="Describe how they should come across…"
              disabled={busy || loadingExtras}
            />
          </Field>
          <Field label="Brain">
            <select className={inputClass} value={model} onChange={(e) => setModel(e.target.value)} disabled={busy}>
              <option value="">Default model</option>
              {models.map((m) => {
                const id = m.provider && !m.id.includes("/") ? `${m.provider}/${m.id}` : m.id;
                return (
                  <option key={id} value={id}>
                    {m.name || id}
                    {m.provider ? ` · ${m.provider}` : ""}
                  </option>
                );
              })}
              {model && !models.some((m) => m.id === model || `${m.provider}/${m.id}` === model) && <option value={model}>{model}</option>}
            </select>
          </Field>
          <label className={`flex items-start gap-2.5 rounded-xl border border-[var(--color-line)] p-3 ${hadBrowser ? "opacity-70" : ""}`}>
            <input
              type="checkbox"
              className="mt-0.5 accent-[var(--color-accent)]"
              checked={ownBrowser}
              disabled={busy || hadBrowser}
              onChange={(e) => setOwnBrowser(e.target.checked)}
            />
            <span>
              <span className="flex items-center gap-1.5 font-medium">
                <Globe size={13} /> Their own browser
                {hadBrowser && <Check size={13} className="text-[var(--color-ok)]" />}
              </span>
              <span className="block text-[12px] text-[var(--color-ink-3)]">
                A separate Chrome profile, so their logins and cookies stay theirs.
              </span>
            </span>
          </label>
          {!isNew && existing?.workspace && (
            <p className="text-[12px] text-[var(--color-ink-3)]">
              Workspace{hostName ? ` on ${hostName}` : ""}:{" "}
              <span className="selectable font-mono text-[11.5px]">{existing.workspace.replace(/^\/Users\/[^/]+/, "~")}</span>
            </p>
          )}
          {error && <p className="text-[12.5px] text-[var(--color-bad)]">{error}</p>}
          {isNew && (
            <button
              onClick={() => {
                close();
                useApp.getState().setAddGatewayOpen(true);
              }}
              className="text-[12px] text-[var(--color-accent-strong)] hover:underline"
            >
              Or add a different kind of bot — Hermes, Claude Code, Codex… →
            </button>
          )}
          <div className="flex items-center justify-between gap-2 pt-1">
            {!isNew ? (
              <div className="flex items-center gap-2">
                <Button variant="danger" size="sm" onClick={remove} disabled={busy}>
                  Delete
                </Button>
                <label className="flex items-center gap-1 text-[11.5px] text-[var(--color-ink-3)]">
                  <input type="checkbox" checked={deleteFiles} onChange={(e) => setDeleteFiles(e.target.checked)} className="accent-[var(--color-bad)]" />
                  and their files
                </label>
              </div>
            ) : (
              <span />
            )}
            <div className="flex items-center gap-2">
              {step && (
                <span className="flex items-center gap-1.5 text-[12px] text-[var(--color-ink-3)]">
                  <Spinner size={12} /> {step}
                </span>
              )}
              <Button variant="ghost" onClick={close} disabled={busy}>
                Cancel
              </Button>
              <Button variant="primary" onClick={save} disabled={busy || loadingExtras}>
                {isNew ? "Make them" : "Save"}
              </Button>
            </div>
          </div>
        </div>
      </div>
    </Modal>
  );
}

function currentModel(agent?: AgentSummary): string {
  const m = agent?.model as unknown;
  if (!m) return "";
  if (typeof m === "string") return m;
  if (typeof m === "object" && m && "primary" in m) return String((m as { primary?: string }).primary ?? "");
  return "";
}
