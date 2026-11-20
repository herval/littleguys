import { agents } from "./gateway";

export interface EditableFile {
  content: string;
  hash?: string;
  missing?: boolean;
}

/** An unreadable file is never treated as an absent file. */
export async function readEditableFile(gateway: string, agentId: string, name: string, initial = ""): Promise<EditableFile> {
  const { file } = await agents.getFile(gateway, agentId, name);
  if (file?.missing === true) return { content: initial, missing: true };
  if (typeof file?.content !== "string" || !file.hash) {
    throw new Error(`Couldn't safely read ${name}. Refresh and try again.`);
  }
  return { content: file.content, hash: file.hash };
}

export async function writeEditedFile(gateway: string, agentId: string, name: string, before: EditableFile, content: string): Promise<void> {
  if (content !== before.content) await agents.setFile(gateway, agentId, name, content, before.hash, before.missing);
}
