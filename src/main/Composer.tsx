import { ArrowUp, Clock3, Paperclip, Pencil, Square, X } from "lucide-react";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { findCommand, loadCommands, matchSlash } from "../lib/commands";
import type { Delivery } from "../lib/gateway";
import { errorMessage } from "../lib/ipc";
import { isModKey, shortcut } from "../lib/platform";
import type { Attachment, CommandEntry, QueuedMessage } from "../lib/types";
import { capsOf, gatewayOf } from "../lib/guys";
import { useApp } from "../stores/app";
import { SlashMenu } from "./SlashMenu";

const DEFAULT_MAX_IMAGE = 6 * 1024 * 1024;
const DEFAULT_MAX_FILE = 20 * 1024 * 1024;

export function Composer({
  gatewayId,
  agentId,
  sessionKey,
  placeholder,
  name,
  running,
  queued,
  onSend,
  onSendQueued,
  onUnqueue,
  onStop,
  draftKey,
}: {
  gatewayId: string;
  agentId: string;
  sessionKey: string;
  placeholder: string;
  /** The guy's name, for "when … is done". */
  name: string;
  running: boolean;
  /** Written while the guy worked, waiting for its turn to end. */
  queued: QueuedMessage[];
  /** `delivery`: only while the guy works (`queue` for after its turn, `steer` for now). */
  onSend: (text: string, attachments: Attachment[], delivery?: Delivery) => Promise<void>;
  onSendQueued: (id: string, delivery?: Delivery) => void;
  onUnqueue: (id: string) => void;
  onStop: () => void;
  draftKey: string;
}) {
  const [text, setText] = useState(() => sessionStorage.getItem(`draft:${draftKey}`) ?? "");
  const [attachments, setAttachments] = useState<Attachment[]>([]);
  const [error, setError] = useState<string | null>(null);
  const [dragging, setDragging] = useState(false);
  const taRef = useRef<HTMLTextAreaElement>(null);
  const fileRef = useRef<HTMLInputElement>(null);
  const policy = useApp((s) => gatewayOf(s.roster, gatewayId)?.status.hello?.policy?.attachments);
  const canAttach = useApp((s) => capsOf(s.roster, gatewayId).attachments);
  const hasCommands = useApp((s) => capsOf(s.roster, gatewayId).commands);
  const canSteer = useApp((s) => Boolean(capsOf(s.roster, gatewayId).steer));
  const [commands, setCommands] = useState<CommandEntry[] | null>(null);
  const [active, setActive] = useState(0);
  const [menuOff, setMenuOff] = useState(false);
  const typingCommand = hasCommands && text.startsWith("/");

  // Fetched the first time a "/" is typed in this thread.
  useEffect(() => {
    if (!typingCommand || commands) return;
    let live = true;
    loadCommands(gatewayId, agentId, sessionKey).then((list) => live && setCommands(list));
    return () => {
      live = false;
    };
  }, [typingCommand, commands, gatewayId, agentId, sessionKey]);

  const match = useMemo(
    () => (typingCommand && commands && !menuOff ? matchSlash(text, commands) : null),
    [typingCommand, commands, menuOff, text],
  );
  const options = match?.stage === "command" || match?.stage === "choice" ? match.items.length : 0;
  const matchKey = match ? `${match.stage}:${match.stage === "command" ? match.query : match.command.name}` : "";
  useEffect(() => setActive(0), [matchKey]);

  useEffect(() => {
    sessionStorage.setItem(`draft:${draftKey}`, text);
  }, [text, draftKey]);

  useEffect(() => {
    taRef.current?.focus();
  }, [draftKey]);

  // Text handed over from elsewhere (a line quoted from the Changes panel, a next-step chip).
  useEffect(() => {
    const onCompose = (e: Event) => {
      const { draftKey: key, text: add } = (e as CustomEvent<{ draftKey: string; text: string }>).detail;
      if (key !== draftKey) return;
      setText((t) => (t.trim() ? `${t.replace(/\s+$/, "")}\n\n${add}` : add));
      requestAnimationFrame(() => {
        const ta = taRef.current;
        if (!ta) return;
        ta.focus();
        ta.setSelectionRange(ta.value.length, ta.value.length);
      });
    };
    document.addEventListener("lg:compose", onCompose);
    return () => document.removeEventListener("lg:compose", onCompose);
  }, [draftKey]);

  // Auto-grow
  useEffect(() => {
    const ta = taRef.current;
    if (!ta) return;
    ta.style.height = "0px";
    ta.style.height = `${Math.min(ta.scrollHeight, 220)}px`;
  }, [text]);

  const addFiles = useCallback(
    async (files: FileList | File[]) => {
      setError(null);
      if (!canAttach) {
        setError("This bot can't take attachments.");
        return;
      }
      const maxImage = policy?.maxImageBytes ?? DEFAULT_MAX_IMAGE;
      const maxFile = policy?.maxBytes ?? DEFAULT_MAX_FILE;
      for (const file of Array.from(files)) {
        const isImage = file.type.startsWith("image/");
        const limit = isImage ? maxImage : maxFile;
        if (file.size > limit) {
          setError(`${file.name} is too big (max ${Math.round(limit / 1024 / 1024)} MB).`);
          continue;
        }
        const dataUrl = await readAsDataUrl(file);
        const content = dataUrl.slice(dataUrl.indexOf(",") + 1);
        setAttachments((a) => [
          ...a,
          {
            type: isImage ? "image" : "file",
            mimeType: file.type || "application/octet-stream",
            fileName: file.name || (isImage ? "pasted-image.png" : "file"),
            content,
            sizeBytes: file.size,
            previewUrl: isImage ? dataUrl : undefined,
          },
        ]);
      }
    },
    [policy, canAttach],
  );

  /** Enter runs (or, for a command that takes arguments, fills it in); Tab only fills. */
  const pick = (i: number, run: boolean) => {
    if (!match || match.stage === "args") return;
    if (match.stage === "command") {
      const c = match.items[i];
      if (!c) return;
      const typed = text.slice(1);
      if (run && (c.acceptsArgs === false || findCommand([c], typed))) {
        submit(c.acceptsArgs === false ? `/${c.name}` : text);
        return;
      }
      setText(`/${c.name}${c.acceptsArgs === false ? "" : " "}`);
      return;
    }
    const choice = match.items[i];
    if (!choice) return;
    const last = (match.command.args?.length ?? 0) <= 1;
    const filled = `/${match.command.name} ${choice.value}${last ? "" : " "}`;
    if (run && last) submit(filled);
    else setText(filled);
  };

  /**
   * While the guy works, a message waits for its turn to end; `now` (⌘↵) puts it into the
   * turn under way instead, when the guy can take that. Commands always go at once.
   */
  const submit = async (override?: string, now = false) => {
    const message = (override ?? text).trim();
    if (!message && attachments.length === 0) return;
    const sending = attachments;
    const command = hasCommands && message.startsWith("/");
    const delivery: Delivery | undefined = !running || command ? undefined : now && canSteer ? "steer" : "queue";
    setText("");
    setAttachments([]);
    setError(null);
    try {
      await onSend(message, sending, delivery);
    } catch (e) {
      setText(message);
      setAttachments(sending);
      setError(errorMessage(e));
    }
  };

  return (
    <div
      className="relative px-4 pb-4 pt-1"
      onDragOver={(e) => {
        if (e.dataTransfer.types.includes("Files")) {
          e.preventDefault();
          setDragging(true);
        }
      }}
      onDragLeave={() => setDragging(false)}
      onDrop={(e) => {
        e.preventDefault();
        setDragging(false);
        if (e.dataTransfer.files.length) addFiles(e.dataTransfer.files);
      }}
    >
      {queued.length > 0 && (
        <div className="mb-1.5 space-y-1">
          <p className="px-1 text-[11.5px] text-[var(--color-ink-3)]">
            {running ? `Sends when ${name} is done` : `Waiting for you: ${name} was stopped`}
          </p>
          {queued.map((q) => {
            const files = q.attachments.length;
            return (
              <div
                key={q.id}
                className="flex items-center gap-2 rounded-xl border border-dashed border-[var(--color-line)] bg-[var(--color-pane-solid)]/70 py-1 pl-2.5 pr-1 text-[12.5px]"
              >
                <Clock3 size={13} className="shrink-0 text-[var(--color-ink-3)]" />
                <span className="selectable min-w-0 flex-1 truncate">{q.text || (files === 1 ? q.attachments[0].fileName : `${files} files`)}</span>
                {files > 0 && q.text && (
                  <span className="flex shrink-0 items-center gap-0.5 text-[11px] text-[var(--color-ink-3)]">
                    <Paperclip size={11} />
                    {files}
                  </span>
                )}
                {(!running || canSteer) && (
                  <button
                    onClick={() => onSendQueued(q.id, running ? "steer" : undefined)}
                    className="shrink-0 rounded-full px-2 py-0.5 text-[11.5px] font-medium text-[var(--color-accent)] transition hover:bg-[var(--color-chip)]"
                    title={running ? `Into what ${name} is doing now` : undefined}
                  >
                    {running ? "Send now" : "Send"}
                  </button>
                )}
                {files === 0 && (
                  <button
                    onClick={() => {
                      onUnqueue(q.id);
                      setText((t) => (t.trim() ? `${q.text}\n\n${t}` : q.text));
                      taRef.current?.focus();
                    }}
                    className="flex h-6 w-6 shrink-0 items-center justify-center rounded-full text-[var(--color-ink-3)] transition hover:bg-[var(--color-chip)] hover:text-[var(--color-ink)]"
                    title="Edit"
                    aria-label="Edit"
                  >
                    <Pencil size={12} />
                  </button>
                )}
                <button
                  onClick={() => onUnqueue(q.id)}
                  className="flex h-6 w-6 shrink-0 items-center justify-center rounded-full text-[var(--color-ink-3)] transition hover:bg-[var(--color-chip)] hover:text-[var(--color-ink)]"
                  title="Don't send"
                  aria-label="Don't send"
                >
                  <X size={13} />
                </button>
              </div>
            );
          })}
        </div>
      )}
      {error && <p className="mb-1.5 px-1 text-[12px] text-[var(--color-bad)]">{error}</p>}
      <div
        className={`relative rounded-2xl border bg-[var(--color-pane-solid)] shadow-[0_1px_6px_rgba(0,0,0,0.06)] transition ${
          dragging ? "border-[var(--color-accent)] ring-4 ring-[var(--color-accent)]/15" : "border-[var(--color-line)] focus-within:border-[var(--color-ink-3)]/50"
        }`}
      >
        {match && <SlashMenu match={match} active={active} onHover={setActive} onPick={(i) => pick(i, true)} />}
        {attachments.length > 0 && (
          <div className="flex flex-wrap gap-2 px-3 pt-3">
            {attachments.map((a, i) => (
              <div key={i} className="group relative">
                {a.previewUrl ? (
                  <img src={a.previewUrl} alt="" className="h-16 w-16 rounded-lg border border-[var(--color-line)] object-cover" />
                ) : (
                  <div className="flex h-16 w-28 flex-col justify-center rounded-lg border border-[var(--color-line)] bg-[var(--color-chip)] px-2 text-[11px]">
                    <span className="truncate font-medium">{a.fileName}</span>
                    <span className="text-[var(--color-ink-3)]">{Math.max(1, Math.round(a.sizeBytes / 1024))} KB</span>
                  </div>
                )}
                <button
                  onClick={() => setAttachments((list) => list.filter((_, j) => j !== i))}
                  className="absolute -right-1.5 -top-1.5 flex h-5 w-5 items-center justify-center rounded-full bg-[var(--color-ink)] text-[var(--color-pane-solid)] opacity-0 shadow transition group-hover:opacity-100"
                  aria-label="Remove attachment"
                >
                  <X size={11} />
                </button>
              </div>
            ))}
          </div>
        )}
        {/* The buttons sit on the last line of text: its middle is 18px up from the bottom (8px padding + half the 20px line). */}
        <div className="flex items-end gap-1.5 p-1.5 pl-2">
          <button
            hidden={!canAttach}
            onClick={() => fileRef.current?.click()}
            className="mb-[calc(18px_-_1rem)] flex h-8 w-8 shrink-0 items-center justify-center rounded-full text-[var(--color-ink-3)] transition hover:bg-[var(--color-chip)] hover:text-[var(--color-ink)]"
            title="Attach files"
          >
            <Paperclip size={16} />
          </button>
          <input
            ref={fileRef}
            type="file"
            multiple
            hidden
            onChange={(e) => {
              if (e.target.files) addFiles(e.target.files);
              e.target.value = "";
            }}
          />
          <textarea
            ref={taRef}
            rows={1}
            value={text}
            placeholder={placeholder}
            onChange={(e) => {
              setText(e.target.value);
              if (!e.target.value.startsWith("/") || e.target.value === "/") setMenuOff(false);
            }}
            onPaste={(e) => {
              const files = Array.from(e.clipboardData.files);
              if (files.length) {
                e.preventDefault();
                addFiles(files);
              }
            }}
            role="combobox"
            aria-expanded={options > 0}
            aria-controls={options > 0 ? "slash-menu" : undefined}
            aria-activedescendant={options > 0 ? `slash-${active}` : undefined}
            onKeyDown={(e) => {
              if (match && e.key === "Escape") {
                e.preventDefault();
                setMenuOff(true);
                return;
              }
              if (options > 0 && !e.nativeEvent.isComposing) {
                if (e.key === "ArrowDown" || e.key === "ArrowUp") {
                  e.preventDefault();
                  setActive((a) => (a + (e.key === "ArrowDown" ? 1 : options - 1)) % options);
                  return;
                }
                if (e.key === "Tab" || (e.key === "Enter" && !e.shiftKey)) {
                  e.preventDefault();
                  pick(active, e.key === "Enter");
                  return;
                }
              }
              if (e.key === "Enter" && !e.shiftKey && !e.nativeEvent.isComposing) {
                e.preventDefault();
                submit(undefined, isModKey(e));
              }
            }}
            className="selectable max-h-[220px] min-h-[36px] flex-1 resize-none bg-transparent py-[8px] text-[13.5px] leading-[20px] outline-none placeholder:text-[var(--color-ink-3)]"
          />
          {running && !text.trim() && attachments.length === 0 ? (
            <button
              onClick={onStop}
              title="Stop"
              className="mb-[calc(18px_-_1rem)] flex h-8 w-8 shrink-0 items-center justify-center rounded-full bg-[var(--color-ink)] text-[var(--color-pane-solid)] transition active:scale-95"
            >
              <Square size={12} fill="currentColor" />
            </button>
          ) : (
            <button
              onClick={() => submit()}
              disabled={!text.trim() && attachments.length === 0}
              title={running ? `Send when ${name} is done (↵)${canSteer ? `, or now (${shortcut("↵")})` : ""}` : "Send (↵)"}
              className="mb-[calc(18px_-_1rem)] flex h-8 w-8 shrink-0 items-center justify-center rounded-full bg-[var(--color-accent)] text-white shadow-sm transition hover:bg-[var(--color-accent-strong)] active:scale-95 disabled:bg-[var(--color-chip)] disabled:text-[var(--color-ink-3)] disabled:shadow-none"
            >
              <ArrowUp size={17} strokeWidth={2.4} />
            </button>
          )}
        </div>
      </div>
      {running && (text.trim() !== "" || attachments.length > 0) && !(hasCommands && text.startsWith("/")) && (
        <p className="px-2 pt-1 text-right text-[11px] text-[var(--color-ink-3)]">
          ↵ sends when {name} is done{canSteer && <> · {shortcut("↵")} sends now</>}
        </p>
      )}
    </div>
  );
}

function readAsDataUrl(file: File): Promise<string> {
  return new Promise((resolve, reject) => {
    const reader = new FileReader();
    reader.onload = () => resolve(String(reader.result));
    reader.onerror = () => reject(reader.error);
    reader.readAsDataURL(file);
  });
}
