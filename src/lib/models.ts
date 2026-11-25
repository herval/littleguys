// The models and efforts a thread can run on (Claude Code, Codex: `threadModels`): fetched
// with `models.list`, cached briefly, and named the way people say them.
import { models as modelsApi } from "./gateway";
import type { ModelOption, ModelPick, RowModel, ThreadModels } from "./types";

const TTL = 5 * 60_000;
const cache = new Map<string, { at: number; list: Promise<ThreadModels> }>();

export function loadThreadModels(gw: string, agentId: string): Promise<ThreadModels> {
  const key = `${gw}|${agentId}`;
  const hit = cache.get(key);
  if (hit && Date.now() - hit.at < TTL) return hit.list;
  const list = modelsApi.forThreads(gw, agentId).catch(() => {
    cache.delete(key);
    return { models: [] } as ThreadModels;
  });
  cache.set(key, { at: Date.now(), list });
  return list;
}

const EFFORTS: Record<string, [name: string, short: string]> = {
  none: ["None", "None"],
  minimal: ["Minimal", "Min"],
  low: ["Low", "Low"],
  medium: ["Medium", "Medium"],
  high: ["High", "High"],
  xhigh: ["Extra high", "Extra"],
  max: ["Max", "Max"],
  ultra: ["Ultra", "Ultra"],
};

const capital = (s: string) => s.charAt(0).toUpperCase() + s.slice(1);

/** `xhigh` → "Extra high". */
export function effortName(effort: string): string {
  return EFFORTS[effort]?.[0] ?? capital(effort);
}

/** For the row of effort buttons: `xhigh` → "Extra". */
export function effortShort(effort: string): string {
  return EFFORTS[effort]?.[1] ?? capital(effort);
}

/**
 * A model as people say it: its name in the list, else a Claude model id read out
 * (`claude-opus-5-5` → "Opus 5.5"), else an alias capitalized (`opus` → "Opus").
 */
export function modelName(id: string, list?: ModelOption[]): string {
  const known = list?.find((m) => m.id === id);
  if (known?.name) return known.name;
  const claude = /^claude-([a-z]+)-(\d+)(?:-(\d{1,2}))?(?:-\d{8})?(\[1m\])?$/.exec(id);
  if (claude) return `${capital(claude[1])} ${claude[2]}${claude[3] ? `.${claude[3]}` : ""}${claude[4] ? " (1M)" : ""}`;
  if (/^[a-z]+$/.test(id)) return capital(id);
  return id;
}

/** The efforts `model` takes: its own, else the ones every model takes, else the default model's. */
export function effortsFor(list: ThreadModels | null, model: string | null | undefined): string[] {
  if (!list) return [];
  const own = list.models.find((m) => m.id === model)?.efforts;
  if (own?.length) return own;
  if (list.efforts?.length) return list.efforts;
  return list.models.find((m) => m.id === list.default?.model)?.efforts ?? [];
}

/** What the chip says a thread runs on. */
export interface Shown {
  /** What you picked (`null`: the default). */
  picked: Required<ModelPick>;
  model: string | null;
  effort: string | null;
}

/**
 * What a thread runs on: what you picked, else (a started thread) what its last turn ran on,
 * else the engine's default.
 */
export function shown(picked: ModelPick, row: RowModel | undefined, list: ThreadModels | null): Shown {
  const model = picked.model ?? row?.model ?? list?.default?.model ?? null;
  const modelDefault = list?.models.find((m) => m.id === model)?.defaultEffort ?? null;
  const effort = picked.effort ?? row?.effort ?? list?.default?.effort ?? modelDefault;
  return { picked: { model: picked.model ?? null, effort: picked.effort ?? null }, model, effort: effort ?? null };
}

/** `Opus 5.5 · High`, or "Default" when nothing says. */
export function chipLabel(s: Shown, list: ThreadModels | null): string {
  if (!s.model && !s.effort) return "Default model";
  const name = s.model ? modelName(s.model, list?.models) : "Default";
  return s.effort ? `${name} · ${effortName(s.effort)}` : name;
}
