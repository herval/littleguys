import { test, afterEach } from "node:test";
import assert from "node:assert/strict";
import { agents, config } from "../src/lib/gateway.ts";
import { writePersonality, giveOwnBrowser } from "../src/lib/guySetup.ts";
const original = { get: agents.getFile, set: agents.setFile, config: config.get, patch: config.patch };
afterEach(() => Object.assign(agents, { getFile: original.get, setFile: original.set }) && Object.assign(config, { get: original.config, patch: original.patch }));
for (const [name, edit] of [
  ["personality", () => writePersonality("gw", "main", "Kind")],
  ["browser", () => giveOwnBrowser("gw", "main", "Main")],
]) {
  test(name + " never writes after a failed read", async () => {
    let writes = 0;
    agents.getFile = async () => { throw new Error("offline"); };
    agents.setFile = config.patch = async () => { writes++; };
    config.get = async () => ({ config: {} });
    await assert.rejects(edit, /offline/);
    assert.equal(writes, 0);
  });
}
test("personality preserves other sections and sends the read version", async () => {
  agents.getFile = async () => ({ file: { content: "# Soul\nOriginal instructions\n", hash: "v1" } });
  let saved;
  agents.setFile = async (...args) => { saved = args; };
  await writePersonality("gw", "main", "Kind");
  assert.match(saved[3], /Original instructions/);
  assert.match(saved[3], /Kind/);
  assert.equal(saved[4], "v1");
});
test("a conflict is returned without a retry or unversioned write", async () => {
  agents.getFile = async () => ({ file: { content: "Original", hash: "v1" } });
  let writes = 0;
  agents.setFile = async () => { writes++; throw new Error("conflict"); };
  await assert.rejects(() => writePersonality("gw", "main", "Kind"), /conflict/);
  assert.equal(writes, 1);
});
test("only explicit absence permits creating a file", async () => {
  agents.getFile = async () => ({ file: {} });
  agents.setFile = async () => { assert.fail("unreadable file was overwritten"); };
  await assert.rejects(() => writePersonality("gw", "main", "Kind"), /safely read/);
  agents.getFile = async () => ({ file: { missing: true } });
  let saved = false;
  agents.setFile = async () => { saved = true; };
  await writePersonality("gw", "main", "Kind");
  assert.equal(saved, true);
});

test("creating a missing file uses the gateway's atomic missing-file precondition",async()=>{
 agents.getFile=async()=>({file:{missing:true}});
 let args; agents.setFile=async(...a)=>{args=a};
 await writePersonality("gw","main","Kind");
 assert.equal(args[4],undefined);assert.equal(args[5],true);
});
test("browser configuration is not patched without a version",async()=>{
 agents.getFile=async()=>({file:{content:"original",hash:"v1"}});
 config.get=async()=>({config:{}});
 config.patch=agents.setFile=async()=>{assert.fail("unversioned mutation")};
 await assert.rejects(()=>giveOwnBrowser("gw","main","Main"),/configuration safely/);
});
