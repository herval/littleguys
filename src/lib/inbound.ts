// What OpenClaw wraps around a message that came in from a channel (Slack,
// Teams, Mattermost…) before the model sees it: queued `System: [time] …`
// event lines ahead of the text, `<@U…> (name)` mentions, and a
// `[slack message id: …]` line. None of it is what the person wrote.

export interface Inbound {
  /** What the person wrote. */
  body: string;
  /** Who sent it, from the "Slack message in #room from Name" event. */
  sender?: string;
  /** Where it came from: the room (`#general`), or the channel for DMs (`Slack DM`). */
  via?: string;
  /** Other events the guy heard about with it, e.g. "Slack message edited in #general." */
  events: string[];
}

// OpenClaw rewrites a `System:` line someone typed to `System (untrusted):`,
// so a line that starts with `System:` is always OpenClaw's own. An event
// starts with a `[time]`; untimed lines continue it, or (before any event)
// are the channel status summary a new session opens with.
const SYSTEM_LINE = /^System: (\[[^\]\n]*\] )?(.*)$/;
const ORIGIN = /^(\S[^\n]{0,40}?) (?:message in (\S+?) from|DM from) (.+?)\.?$/;
const MESSAGE_ID_LINE = /^[ \t]*\[slack message id: [^\]\n]*\][ \t]*$/gim;
const MENTION = /<@[A-Z0-9]+> \(([^()\n]+)\)/g;

export function parseInbound(text: string): Inbound {
  const raw: string[] = [];
  const body: string[] = [];
  let open = false;
  for (const line of text.split("\n")) {
    const sys = line.match(SYSTEM_LINE);
    if (!sys) {
      open = false;
      body.push(line);
    } else if (sys[1]) {
      open = true;
      raw.push(sys[2].trim());
    } else if (open) {
      raw[raw.length - 1] = `${raw[raw.length - 1]} ${sys[2].trim()}`.trim();
    }
  }

  let origin: RegExpMatchArray | null = null;
  const events: string[] = [];
  for (const event of raw) {
    const match: RegExpMatchArray | null = origin ? null : event.match(ORIGIN);
    if (match) origin = match;
    else if (event && !events.includes(event)) events.push(event);
  }
  const [, channel, room, sender] = origin ?? [];
  return {
    body: body
      .join("\n")
      .replace(MESSAGE_ID_LINE, "")
      .replace(MENTION, "@$1")
      .replace(/\n{3,}/g, "\n\n")
      .trim(),
    sender,
    via: channel && (room?.startsWith("#") ? room : room ? channel : `${channel} DM`),
    events,
  };
}
