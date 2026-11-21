import { test } from "node:test";
import assert from "node:assert/strict";
import { groupByFolder, groupByState, groupByWorkspace, listThreads } from "../src/lib/threadGroups.ts";

const now = Date.now();
const min = 60_000;
const gateway = (id, kind, agents, sessions, extra = {}) => ({
  id,
  name: id,
  enabled: true,
  loaded: true,
  status: { gatewayId: id, phase: "connected", kind },
  agents,
  sessions,
  running: [],
  approvals: [],
  ...extra,
});

// Fictional guys: an OpenClaw pair on "den", and a Claude Code that works in three folders.
const roster = {
  hidden: [],
  gateways: [
    gateway(
      "den",
      "openclaw",
      [{ id: "pip", name: "Pip" }, { id: "moss", name: "Moss" }],
      [
        { key: "agent:pip:main", agentId: "pip", updatedAt: now - 5 * min, unread: true },
        { key: "agent:pip:dashboard:a", agentId: "pip", label: "Trip", updatedAt: now - 60 * min },
        { key: "agent:pip:dashboard:old", agentId: "pip", label: "Old", updatedAt: now - 90 * min, pinned: true },
        { key: "agent:pip:dashboard:gone", agentId: "pip", label: "Gone", updatedAt: now, archived: true },
        { key: "agent:pip:cron:j:run:r", agentId: "pip", updatedAt: now },
      ],
      { approvals: [{ agentId: "pip", sessionKey: "agent:pip:dashboard:a" }] },
    ),
    gateway(
      "cc",
      "claudecode",
      [{ id: "claude", name: "Claude Code" }],
      [
        { key: "agent:claude:cc:1", agentId: "claude", workspaceDir: "/w/shop", updatedAt: now - 1 * min },
        { key: "agent:claude:cc:2", agentId: "claude", workspaceDir: "/w/shop/.claude/worktrees/fix/", updatedAt: now - 2 * min },
        { key: "agent:claude:cc:3", agentId: "claude", workspaceDir: "/w/shop/.claude/worktrees/fix", updatedAt: now - 3 * min },
        { key: "agent:claude:cc:4", agentId: "claude", workspaceDir: "/w/notes", updatedAt: now - 4 * min },
      ],
      { running: ["agent:claude:cc:1"] },
    ),
  ],
};

const keys = (threads) => threads.map((t) => t.row.key);

test("every guy's chats, pinned first then newest, with a main chat for guys you haven't talked to", () => {
  const threads = listThreads(roster);
  assert.deepEqual(keys(threads), [
    "agent:pip:dashboard:old",
    "agent:claude:cc:1",
    "agent:claude:cc:2",
    "agent:claude:cc:3",
    "agent:claude:cc:4",
    "agent:pip:main",
    "agent:pip:dashboard:a",
    "agent:moss:main",
  ]);
  const state = Object.fromEntries(threads.map((t) => [t.row.key, t.state]));
  assert.equal(state["agent:pip:dashboard:a"], "needs-you");
  assert.equal(state["agent:claude:cc:1"], "working");
  assert.equal(state["agent:pip:main"], "unread");
  assert.equal(state["agent:moss:main"], "idle");
  assert.equal(threads[0].id, "den|agent:pip:dashboard:old");
});

test("by state: waiting on you, working, unread, idle, leaving out empty ones", () => {
  const groups = groupByState(listThreads(roster));
  assert.deepEqual(
    groups.map((g) => [g.title, g.threads.length]),
    [
      ["Needs your OK", 1],
      ["Working", 1],
      ["Unread", 1],
      ["Idle", 5],
    ],
  );
});

test("by workspace: a worktree goes under its repo, one status speaks for its folder, the rest go last", () => {
  const threads = listThreads(roster);
  // Only cc:2's status is in: it's a worktree of /w/shop. cc:3 is in the same folder.
  const statuses = {
    "cc|agent:claude:cc:1": { repo: { root: "/w/shop", name: "shop", mainRoot: null } },
    "cc|agent:claude:cc:2": { repo: { root: "/w/shop/.claude/worktrees/fix", name: "shop", mainRoot: "/w/shop" } },
    "cc|agent:claude:cc:4": null,
  };
  const groups = groupByWorkspace(threads, (id) => statuses[id]);
  assert.deepEqual(
    groups.map((g) => [g.title, keys(g.threads)]),
    [
      ["shop", ["agent:claude:cc:1", "agent:claude:cc:2", "agent:claude:cc:3"]],
      ["notes", ["agent:claude:cc:4"]],
      ["Other chats", ["agent:pip:dashboard:old", "agent:pip:main", "agent:pip:dashboard:a", "agent:moss:main"]],
    ],
  );
  // Before any status is in, threads go by their folder.
  assert.deepEqual(
    groupByWorkspace(threads, () => undefined).map((g) => g.title),
    ["shop", "fix", "notes", "Other chats"],
  );
});

test("by folder: your folders in order, empty ones too, then everything else", () => {
  const folders = [
    { id: "a", name: "Travel" },
    { id: "b", name: "Empty" },
  ];
  const filed = { "den|agent:pip:dashboard:a": "a", "cc|agent:claude:cc:4": "a", "den|agent:moss:main": "deleted-folder" };
  const groups = groupByFolder(listThreads(roster), folders, filed);
  assert.deepEqual(
    groups.map((g) => [g.title, g.threads.length, g.folderId ?? null, Boolean(g.rest)]),
    [
      ["Travel", 2, "a", false],
      ["Empty", 0, "b", false],
      ["Everything else", 6, null, true],
    ],
  );
  assert.deepEqual(keys(groups[0].threads), ["agent:claude:cc:4", "agent:pip:dashboard:a"]);
});
