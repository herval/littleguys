import { test } from "node:test";
import assert from "node:assert/strict";
import { chipLabel, effortName, effortsFor, modelName, shown } from "../src/lib/models.ts";

const claude = {
  models: [{ id: "opus", name: "Opus" }, { id: "sonnet", name: "Sonnet" }],
  efforts: ["low", "medium", "high", "xhigh", "max"],
  default: { model: "sonnet", effort: "high" },
};
const codex = {
  models: [
    { id: "gpt-test-large", name: "GPT-Test-Large", efforts: ["low", "medium", "high", "ultra"], defaultEffort: "low" },
    { id: "gpt-test-small", name: "GPT-Test-Small", efforts: ["low", "medium"], defaultEffort: "medium" },
  ],
  default: { model: "gpt-test-large" },
};

test("models are named the way people say them", () => {
  assert.equal(modelName("claude-opus-5-5"), "Opus 5.5");
  assert.equal(modelName("claude-sonnet-5-20260101"), "Sonnet 5");
  assert.equal(modelName("claude-fable-5-2[1m]"), "Fable 5.2 (1M)");
  assert.equal(modelName("haiku"), "Haiku");
  assert.equal(modelName("gpt-test-large", codex.models), "GPT-Test-Large");
  assert.equal(modelName("some/other-model"), "some/other-model");
  assert.equal(effortName("xhigh"), "Extra high");
  assert.equal(effortName("turbo"), "Turbo");
});

test("a model's efforts are its own, else every model's", () => {
  assert.deepEqual(effortsFor(codex, "gpt-test-small"), ["low", "medium"]);
  assert.deepEqual(effortsFor(codex, null), ["low", "medium", "high", "ultra"], "the default model's");
  assert.deepEqual(effortsFor(claude, "opus"), claude.efforts);
  assert.deepEqual(effortsFor(null, "opus"), []);
});

test("the chip shows your pick, else what the thread ran on, else the default", () => {
  assert.equal(chipLabel(shown({}, undefined, claude), claude), "Sonnet · High");
  assert.equal(chipLabel(shown({ model: "opus" }, undefined, claude), claude), "Opus · High");
  const ran = { model: "claude-opus-5-5", effort: "max" };
  assert.equal(chipLabel(shown({}, ran, claude), claude), "Opus 5.5 · Max");
  assert.equal(chipLabel(shown({ effort: "low" }, ran, claude), claude), "Opus 5.5 · Low");
  assert.equal(chipLabel(shown({ model: "gpt-test-small" }, undefined, codex), codex), "GPT-Test-Small · Medium", "its own default effort");
  assert.equal(chipLabel(shown({}, undefined, { models: [] }), null), "Default model");
});
