import type { AgentSummary, RoomInfo } from "../lib/types";
import { guyAvatarUrl, guyEmoji, guyHue, guyName } from "../lib/guys";
import { seatFace, seatsOf } from "../lib/rooms";
import { useApp } from "../stores/app";
import ghost from "./guy.svg";
import sleepyGhost from "./guy-sleepy.svg";

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
    <img
      src={mood === "sleepy" ? sleepyGhost : ghost}
      width={size}
      height={size}
      alt=""
      draggable={false}
      className={mood === "happy" ? "origin-bottom animate-breathe" : undefined}
    />
  );
}
