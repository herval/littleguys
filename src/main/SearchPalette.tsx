// Cmd+K: jump to any guy, thread or message, on every gateway.
import { AlarmClock, Archive, Search } from "lucide-react";
import { Fragment, useEffect, useMemo, useRef, useState } from "react";
import { activityFor, allGuys, channelName, guyName, isMultiGateway, shortTime, threadKind } from "../lib/guys";
import { sessions as sessionsApi } from "../lib/gateway";
import { findInRoster, findMessages, highlightParts, plainSnippet, queryWords, type GuyResult, type MessageResult, type ThreadResult } from "../lib/search";
import { useApp } from "../stores/app";
import { useArchive } from "../stores/archive";
import { Avatar } from "../ui/Avatar";
import { Spinner } from "../ui/kit";

type Item =
  | { kind: "guy"; id: string; r: GuyResult }
  | { kind: "thread"; id: string; r: ThreadResult }
  | { kind: "message"; id: string; r: MessageResult };

/** Messages are asked for once typing stops for this long. */
const MESSAGE_DELAY_MS = 200;

export function SearchPalette() {
  const roster = useApp((s) => s.roster);
  const archived = useArchive((s) => s.byGuy);
  const [query, setQuery] = useState("");
  const [active, setActive] = useState(0);
  const [messages, setMessages] = useState<{ query: string; results: MessageResult[]; indexing: boolean } | null>(null);
  const listRef = useRef<HTMLDivElement>(null);
  const inputRef = useRef<HTMLInputElement>(null);

  const guys = useMemo(() => allGuys(roster), [roster]);
  const words = useMemo(() => queryWords(query), [query]);
  const found = useMemo(() => findInRoster(roster, guys, archived, query), [roster, guys, archived, query]);
  const q = query.trim();

  // Archived threads load while a guy's threads are on screen; search needs everyone's.
  useEffect(() => {
    const { byGuy, load } = useArchive.getState();
    for (const guy of allGuys(useApp.getState().roster)) if (!byGuy[guy.uid]) void load(guy);
  }, []);

  useEffect(() => {
    if (q.length < 2) {
      setMessages(null);
      return;
    }
    let stale = false;
    const t = setTimeout(() => {
      const { roster } = useApp.getState();
      findMessages(roster, allGuys(roster), useArchive.getState().byGuy, q)
        .then((res) => !stale && setMessages({ query: q, ...res }))
        .catch(() => !stale && setMessages({ query: q, results: [], indexing: false }));
    }, MESSAGE_DELAY_MS);
    return () => {
      stale = true;
      clearTimeout(t);
    };
  }, [q]);

  // The last answer stays up (faded) while the next one is on its way.
  const searching = q.length >= 2 && messages?.query !== q;
  const items = useMemo<Item[]>(
    () => [
      ...found.guys.map((r): Item => ({ kind: "guy", id: `g|${r.guy.uid}`, r })),
      ...found.threads.map((r): Item => ({ kind: "thread", id: `t|${r.guy.uid}|${r.row.key}`, r })),
      ...(q.length >= 2 ? (messages?.results ?? []) : []).map(
        (r): Item => ({ kind: "message", id: `m|${r.guy.uid}|${r.key}|${r.hit.messageId}`, r }),
      ),
    ],
    [found, messages, q],
  );
  const current = Math.min(active, items.length - 1);

  useEffect(() => {
    const list = listRef.current;
    if (!list) return;
    if (current <= 0) list.scrollTop = 0;
    else list.querySelector(`[data-i="${current}"]`)?.scrollIntoView({ block: "nearest" });
  }, [current]);

  const close = () => useApp.getState().setSearchOpen(false);
  const open = async (item: Item) => {
    close();
    const s = useApp.getState();
    const { guy } = item.r;
    if (item.kind === "guy") return s.selectGuy(guy.uid);
    let sessionKey = item.kind === "thread" ? item.r.row.key : item.r.key;
    // A session from before the list goes back: back in the list, as when it's resumed.
    if (item.kind === "message" && item.r.past) {
      sessionKey = await sessionsApi.open(guy.gatewayId, item.r.past).then(
        (r) => r.key,
        () => sessionKey,
      );
    }
    // `navigate` puts routine runs in the Routines tab and the rest in the chat.
    s.navigate({ gatewayId: guy.gatewayId, agentId: guy.id, sessionKey });
  };

  const onKeyDown = (e: React.KeyboardEvent) => {
    if (e.nativeEvent.isComposing) return;
    if (e.key === "ArrowDown") {
      e.preventDefault();
      setActive(Math.min(current + 1, items.length - 1));
    } else if (e.key === "ArrowUp") {
      e.preventDefault();
      setActive(Math.max(current - 1, 0));
    } else if (e.key === "Enter") {
      e.preventDefault();
      if (items[current]) void open(items[current]);
    } else if (e.key === "Escape") {
      e.preventDefault();
      close();
    }
  };

  const heading = (item: Item, i: number) => {
    if (i > 0 && items[i - 1].kind === item.kind) return null;
    const label = item.kind === "guy" ? "Little guys" : item.kind === "message" ? "Messages" : q ? "Threads" : "Recent";
    return <p className="px-3 pb-1 pt-2.5 text-[11px] font-semibold uppercase tracking-wide text-[var(--color-ink-3)]">{label}</p>;
  };
  const multi = isMultiGateway(roster);

  return (
    <div
      className="fixed inset-0 z-50 flex justify-center bg-black/20 px-6 pt-[12vh] backdrop-blur-[2px] dark:bg-black/40"
      onMouseDown={(e) => {
        if (e.target === e.currentTarget) close();
      }}
    >
      <div
        role="dialog"
        aria-label="Search"
        onKeyDown={onKeyDown}
        // Typing, arrows and Esc keep working wherever you click in here.
        onMouseDown={(e) => {
          if (e.target !== inputRef.current) e.preventDefault();
        }}
        className="animate-pop flex max-h-[min(560px,76vh)] w-full max-w-[620px] flex-col self-start overflow-hidden rounded-2xl border border-[var(--color-line)] bg-[var(--color-pane-solid)] shadow-2xl"
      >
        <div className="flex shrink-0 items-center gap-2.5 border-b border-[var(--color-line)] px-4">
          <Search size={17} className="shrink-0 text-[var(--color-ink-3)]" />
          <input
            ref={inputRef}
            autoFocus
            value={query}
            onChange={(e) => {
              setQuery(e.target.value);
              setActive(0);
            }}
            placeholder="Search threads, guys and messages"
            spellCheck={false}
            autoComplete="off"
            role="combobox"
            aria-expanded
            aria-controls="search-results"
            aria-activedescendant={items[current] ? `search-${current}` : undefined}
            className="selectable h-[52px] min-w-0 flex-1 bg-transparent text-[15px] text-[var(--color-ink)] outline-none placeholder:text-[var(--color-ink-3)]"
          />
          {searching && <Spinner size={14} />}
        </div>

        <div ref={listRef} id="search-results" role="listbox" className="min-h-0 flex-1 overflow-y-auto px-1.5 pb-1.5">
          {items.map((item, i) => (
            <Fragment key={item.id}>
              {heading(item, i)}
              <div
                id={`search-${i}`}
                data-i={i}
                role="option"
                aria-selected={i === current}
                onMouseMove={() => i !== current && setActive(i)}
                onClick={() => void open(item)}
                className={`flex cursor-default items-center gap-3 rounded-xl px-2.5 py-2 ${i === current ? "bg-[var(--color-chip)]" : ""} ${
                  item.kind === "message" && searching ? "opacity-55" : ""
                }`}
              >
                <Avatar agent={item.r.guy} size={34} />
                <div className="min-w-0 flex-1">
                  {item.kind === "guy" ? (
                    <GuyLine r={item.r} words={words} gateway={multi} />
                  ) : item.kind === "thread" ? (
                    <ThreadLine r={item.r} words={words} />
                  ) : (
                    <MessageLine r={item.r} words={words} />
                  )}
                </div>
              </div>
            </Fragment>
          ))}
          {!items.length && (
            <p className="px-3 py-6 text-center text-[13px] text-[var(--color-ink-3)]">
              {!q ? "No threads yet." : searching ? "Looking…" : `Nothing matches “${q}”.`}
            </p>
          )}
        </div>

        <div className="flex shrink-0 items-center gap-3 border-t border-[var(--color-line)] px-4 py-1.5 text-[11px] text-[var(--color-ink-3)]">
          <span>↑↓ choose · ↵ open · esc close</span>
          {messages?.query === q && messages.indexing && <span className="ml-auto">Some messages are still being indexed</span>}
        </div>
      </div>
    </div>
  );
}

function Hl({ text, words }: { text: string; words: string[] }) {
  return (
    <>
      {highlightParts(text, words).map((p, i) =>
        p.match ? (
          <mark key={i} className="rounded-[3px] bg-[var(--color-accent-soft)] text-[var(--color-ink)]">
            {p.text}
          </mark>
        ) : (
          p.text
        ),
      )}
    </>
  );
}

function When({ ms }: { ms: number | null | undefined }) {
  return ms ? <span className="ml-auto shrink-0 pl-2 text-[11px] text-[var(--color-ink-3)]">{shortTime(ms)}</span> : null;
}

function GuyLine({ r, words, gateway }: { r: GuyResult; words: string[]; gateway: boolean }) {
  const roster = useApp((s) => s.roster);
  const { guy } = r;
  const status = activityFor(roster, guy).status;
  return (
    <>
      <div className="flex items-baseline">
        <span className="truncate font-display text-[14px] font-semibold">
          <Hl text={guyName(guy)} words={words} />
        </span>
      </div>
      <p className="truncate text-[12.5px] text-[var(--color-ink-3)]">
        {gateway && guy.gatewayName !== guyName(guy) && (
          <>
            <Hl text={guy.gatewayName} words={words} /> ·{" "}
          </>
        )}
        {status}
      </p>
    </>
  );
}

function ThreadLine({ r, words }: { r: ThreadResult; words: string[] }) {
  const kind = threadKind(r.row.key);
  const channel = kind === "channel" ? channelName(r.row.key) : null;
  return (
    <>
      <div className="flex items-center gap-1.5">
        {kind === "routine" && <AlarmClock size={12} className="shrink-0 text-[var(--color-ink-3)]" />}
        <span className="truncate text-[13.5px] font-medium">
          <Hl text={r.main ? guyName(r.guy) : r.title} words={words} />
        </span>
        {channel && <Badge>{channel}</Badge>}
        {r.archived && (
          <Badge>
            <Archive size={9} className="-mt-px mr-0.5 inline" />
            archived
          </Badge>
        )}
        <When ms={r.row.updatedAt} />
      </div>
      <p className="truncate text-[12.5px] text-[var(--color-ink-3)]">
        <span className="text-[var(--color-ink-2)]">
          <Hl text={r.main ? r.title : guyName(r.guy)} words={words} />
        </span>
        {r.folder && (
          <>
            {" · "}
            <Hl text={r.folder} words={words} />
          </>
        )}
        {r.preview && (
          <>
            {" · "}
            <Hl text={r.preview} words={words} />
          </>
        )}
      </p>
    </>
  );
}

function MessageLine({ r, words }: { r: MessageResult; words: string[] }) {
  return (
    <>
      <div className="flex items-center gap-1.5">
        <span className="truncate text-[13.5px] font-medium">{r.title}</span>
        <span className="shrink-0 text-[12px] text-[var(--color-ink-3)]">· {guyName(r.guy)}</span>
        {r.archived && <Badge>archived</Badge>}
        <When ms={r.hit.timestamp} />
      </div>
      <p className="line-clamp-2 text-[12.5px] leading-snug text-[var(--color-ink-3)]">
        {r.hit.role === "user" && <span className="text-[var(--color-ink-2)]">You: </span>}
        <Hl text={plainSnippet(r.hit.snippet)} words={words} />
      </p>
    </>
  );
}

function Badge({ children }: { children: React.ReactNode }) {
  return (
    <span className="shrink-0 rounded bg-[var(--color-chip)] px-1 text-[9.5px] font-semibold uppercase tracking-wide text-[var(--color-ink-3)]">
      {children}
    </span>
  );
}
