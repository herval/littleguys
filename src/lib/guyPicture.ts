// A guy's picture, separate from their emoji. OpenClaw has its own: for guys on
// a gateway on this Mac, the image goes into their workspace and OpenClaw's
// `identity.avatar` points at it (a short path in IDENTITY.md, never image data),
// so OpenClaw's own apps show it too. Everyone else keeps the picture in
// littleguys on this Mac (`set_guy_face`): a remote gateway can't receive files,
// and other bots have nowhere to put one.
import { agents, config } from "./gateway";
import { gatewayOf, isOnThisMac } from "./guys";
import { core } from "./ipc";
import type { Guy, Roster } from "./types";

type Who = Pick<Guy, "uid" | "id" | "gatewayId" | "identity">;

/** Where a new picture for this guy goes: OpenClaw's own avatar, or littleguys on this Mac. */
export function pictureHome(roster: Roster, guy: Pick<Guy, "gatewayId">): "openclaw" | "this-mac" {
  const g = gatewayOf(roster, guy.gatewayId);
  return g?.status.kind === "openclaw" && g.status.capabilities?.manageGuys && isOnThisMac(g) ? "openclaw" : "this-mac";
}

/** A picture littleguys can take away: one kept on this Mac, or OpenClaw's own (not a bot's built-in face). */
export function hasPicture(roster: Roster, guy: Who): boolean {
  return roster.faces.includes(guy.uid) || Boolean(openclawAvatar(roster, guy));
}

/** Give a guy a picture (a `data:image/…` URL), or `null` to take theirs away. */
export async function setPicture(roster: Roster, guy: Who, picture: string | null): Promise<void> {
  const kept = roster.faces.includes(guy.uid);
  if (!picture) {
    if (kept) await core.setGuyFace(guy.uid, null);
    else {
      const avatar = openclawAvatar(roster, guy);
      if (avatar) await removeOpenclawAvatar(roster, guy, avatar);
    }
  } else if (pictureHome(roster, guy) === "openclaw") {
    const avatar = await core.setWorkspaceAvatar(guy.gatewayId, guy.id, picture);
    if (avatar) await agents.update(guy.gatewayId, { agentId: guy.id, avatar });
    if (kept) await core.setGuyFace(guy.uid, null);
  } else {
    await core.setGuyFace(guy.uid, picture);
  }
  await core.refreshRoster(guy.gatewayId);
}

function openclawAvatar(roster: Roster, guy: Who): string | null {
  const isOpenclaw = gatewayOf(roster, guy.gatewayId)?.status.kind === "openclaw";
  return (isOpenclaw && guy.identity?.avatar?.trim()) || null;
}

/**
 * OpenClaw's `agents.update` can set an avatar but not unset one, so take it out
 * of the agent's config entry and their IDENTITY.md (and delete our file, if it's ours).
 */
async function removeOpenclawAvatar(roster: Roster, guy: Who, avatar: string): Promise<void> {
  const gw = guy.gatewayId;
  const snapshot = await config.get(gw);
  const parsed = (snapshot.parsed ?? snapshot.config ?? {}) as {
    agents?: {
      entries?: Record<string, { identity?: { avatar?: string } }>;
      list?: Array<{ id?: string; identity?: { avatar?: string } }>;
    };
  };
  // Newer OpenClaw keys agents by id under `entries`; older ones keep a `list` (patched by id).
  const patch = parsed.agents?.entries?.[guy.id]?.identity?.avatar
    ? { agents: { entries: { [guy.id]: { identity: { avatar: null } } } } }
    : parsed.agents?.list?.some((a) => a.id === guy.id && a.identity?.avatar)
      ? { agents: { list: [{ id: guy.id, identity: { avatar: null } }] } }
      : null;
  if (patch) await config.patch(gw, patch, snapshot.hash, `littleguys: no picture for ${guy.id}`);
  const file = await agents.getFile(gw, guy.id, "IDENTITY.md").catch(() => null);
  const current = file?.file?.content;
  const next = current && withoutAvatarLine(current, avatar);
  if (current && next !== current) await agents.setFile(gw, guy.id, "IDENTITY.md", next!, file?.file?.hash);
  if (isOnThisMac(gatewayOf(roster, gw))) await core.setWorkspaceAvatar(gw, guy.id, null).catch(() => {});
}

/** Drops the `- Avatar: …` line (however it's bolded) that holds `avatar`; leaves the rest alone. */
function withoutAvatarLine(doc: string, avatar: string): string {
  return doc
    .split("\n")
    .filter((line) => {
      const m = /^\s*-\s*[*_`]*avatar[*_`]*\s*:(.*)$/i.exec(line);
      return !m || m[1].replace(/^[\s*_`]+|[\s*_`]+$/g, "") !== avatar;
    })
    .join("\n");
}
