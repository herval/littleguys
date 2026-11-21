import { errorMessage, isTauri } from "../lib/ipc";

/** Mutation errors must remain visible after the menu that started them closes. */
export async function reportError(error: unknown, title = "Couldn't save the change", kind: "error" | "info" = "error"): Promise<void> {
  const text = errorMessage(error);
  if (isTauri) {
    const { message } = await import("@tauri-apps/plugin-dialog");
    await message(text, { title, kind });
  } else {
    window.alert(text);
  }
}
