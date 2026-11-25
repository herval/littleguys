import type { AgentSummary, RoomInfo } from "../lib/types";
import { guyAvatarUrl, guyEmoji, guyHue, guyName } from "../lib/guys";
import { seatFace, seatsOf } from "../lib/rooms";
import { useApp } from "../stores/app";

export type RingState = "idle" | "working" | "unread" | "needs-you" | "none";

interface AvatarProps {
  /** A room (`room` set) shows as its members' faces together. */
  agent: Pick<AgentSummary, "id" | "name" | "identity"> & { room?: RoomInfo };
  size?: number;
  ring?: RingState;
  selected?: boolean;
  className?: string;
}

/**
 * A little guy's face: their emoji (or avatar image) on a soft per-guy color,
 * wrapped in a stories-style ring that shows what they're up to.
 */
export function Avatar({ agent, size = 44, ring = "none", selected = false, className = "" }: AvatarProps) {
  const hue = guyHue(agent.id);
  const emoji = guyEmoji(agent);
  const img = guyAvatarUrl(agent);
  const name = guyName(agent);
  const ringWidth = ring === "none" ? 0 : Math.max(2, Math.round(size / 22));
  const gap = ring === "none" ? 0 : Math.max(2, Math.round(size / 26));
  const inner = size - (ringWidth + gap) * 2;
  if (agent.room) return <RoomFace room={agent.room} name={name} size={size} working={ring === "working"} className={className} />;

  const ringBackground =
    ring === "working"
      ? `conic-gradient(from 0deg, #ff7a45, #ffc24b, #ff5fa2, #ff7a45)`
      : ring === "unread"
        ? `linear-gradient(135deg, #ff7a45, #ff5fa2)`
        : ring === "needs-you"
          ? `linear-gradient(135deg, #ff453a, #ff9f0a)`
          : ring === "idle"
            ? selected
              ? "var(--color-ink-3)"
              : "var(--color-line)"
            : "transparent";

  return (
    <div className={`relative shrink-0 ${className}`} style={{ width: size, height: size }} title={name}>
      {ring !== "none" && (
        <div
          className={`absolute inset-0 rounded-full ${ring === "working" ? "animate-spin-slow" : ""}`}
          style={{ background: ringBackground }}
        />
      )}
      <div
        className="absolute rounded-full bg-[var(--color-pane-solid)]"
        style={{ inset: ringWidth }}
      />
      <div
        className={`absolute flex items-center justify-center overflow-hidden rounded-full ${ring === "working" ? "animate-breathe" : ""}`}
        style={{
          inset: ringWidth + gap,
          width: inner,
          height: inner,
          background: img ? undefined : `linear-gradient(160deg, hsl(${hue} 90% 88%), hsl(${(hue + 30) % 360} 85% 76%))`,
        }}
      >
        {img ? (
          <img src={img} alt="" className="h-full w-full object-cover" draggable={false} />
        ) : emoji ? (
          <span style={{ fontSize: inner * 0.52, lineHeight: 1 }}>{emoji}</span>
        ) : (
          <span className="font-semibold" style={{ fontSize: inner * 0.42, color: `hsl(${hue} 45% 32%)` }}>
            {name.slice(0, 1).toUpperCase()}
          </span>
        )}
      </div>
      {ring === "needs-you" && (
        <span className="absolute -right-0.5 -top-0.5 flex h-4 w-4 items-center justify-center rounded-full bg-[var(--color-bad)] text-[10px] font-bold text-white ring-2 ring-[var(--color-pane-solid)]">
          !
        </span>
      )}
    </div>
  );
}

/** A room's face: up to three of its members', overlapping. */
function RoomFace({ room, name, size, working, className }: { room: RoomInfo; name: string; size: number; working: boolean; className: string }) {
  const roster = useApp((s) => s.roster);
  const faces = seatsOf(roster, room).slice(0, 3);
  const each = Math.round(size * (faces.length > 2 ? 0.58 : 0.66));
  // Top left, right, bottom middle: a little pile.
  const spots = faces.length > 2 ? [[0, 0], [size - each, size * 0.1], [(size - each) * 0.4, size - each]] : [[0, 0], [size - each, size - each]];
  return (
    <div className={`relative shrink-0 ${working ? "animate-breathe" : ""} ${className}`} style={{ width: size, height: size }} title={name}>
      {faces.map((seat, i) => (
        <div key={seat.uid} className="absolute rounded-full ring-2 ring-[var(--color-pane-solid)]" style={{ left: spots[i][0], top: spots[i][1] }}>
          <Avatar agent={seatFace(seat)} size={each} />
        </div>
      ))}
    </div>
  );
}

/** The littleguys mascot, in color, for empty states and onboarding. */
export function GuyMark({ size = 96, mood = "happy" }: { size?: number; mood?: "happy" | "sleepy" | "wave" }) {
  return (
    <svg width={size} height={size} viewBox="0 0 120 120" aria-hidden>
      <defs>
        <linearGradient id="gm-body" x1="0" y1="0" x2="0" y2="1">
          <stop offset="0" stopColor="#fff" />
          <stop offset="1" stopColor="#ffeadb" />
        </linearGradient>
      </defs>
      <g className={mood === "happy" ? "animate-breathe" : ""} style={{ transformOrigin: "60px 100px" }}>
        <path
          d="M 28 54 A 32 32 0 0 1 92 54 L 92 88 Q 92 96 84 96 L 36 96 Q 28 96 28 88 Z"
          fill="url(#gm-body)"
          stroke="#f0602a"
          strokeOpacity="0.25"
          strokeWidth="1.5"
        />
        <rect x="38" y="92" width="16" height="12" rx="6" fill="url(#gm-body)" stroke="#f0602a" strokeOpacity="0.25" strokeWidth="1.5" />
        <rect x="66" y="92" width="16" height="12" rx="6" fill="url(#gm-body)" stroke="#f0602a" strokeOpacity="0.25" strokeWidth="1.5" />
        {mood === "sleepy" ? (
          <>
            <path d="M 44 60 Q 50 66 56 60" stroke="#2b1b14" strokeWidth="3" fill="none" strokeLinecap="round" />
            <path d="M 64 60 Q 70 66 76 60" stroke="#2b1b14" strokeWidth="3" fill="none" strokeLinecap="round" />
            <text x="88" y="34" fontSize="16" fontWeight="700" fill="#f0602a">z</text>
            <text x="98" y="22" fontSize="12" fontWeight="700" fill="#f0602a" opacity="0.7">z</text>
          </>
        ) : (
          <>
            <ellipse cx="50" cy="58" rx="5" ry="7" fill="#2b1b14" />
            <ellipse cx="70" cy="58" rx="5" ry="7" fill="#2b1b14" />
            <circle cx="51.6" cy="55.5" r="1.7" fill="#fff" />
            <circle cx="71.6" cy="55.5" r="1.7" fill="#fff" />
          </>
        )}
        <ellipse cx="40" cy="70" rx="5" ry="3" fill="#ff8a8a" opacity="0.5" />
        <ellipse cx="80" cy="70" rx="5" ry="3" fill="#ff8a8a" opacity="0.5" />
        {mood !== "sleepy" && <path d="M 55 70 Q 60 75 65 70" stroke="#2b1b14" strokeWidth="2.5" fill="none" strokeLinecap="round" />}
        {mood === "wave" && <path d="M 92 66 L 106 50" stroke="#f0602a" strokeOpacity="0.35" strokeWidth="7" strokeLinecap="round" />}
      </g>
    </svg>
  );
}
