// Fixture backend for running the UI in a plain browser (`pnpm dev`).
// Shapes mirror real gateway responses. Query params:
//   ?phase=pairing|offline|no-config   previews the Doctor (no gateway connected)
//   ?single=1                          one gateway only
//   ?empty=1                           connected, but no guys yet
//   ?stills=1                          the browser tab isn't in front, so its stream sends no frames
import claudeFace from "../../src-tauri/assets/claude-code.svg";
import type { Backend } from "./ipc";
import type { ChatMessage, ConnStatus, DiscoveredGateway, DoctorReport, GatewayRoster, Roster } from "./types";

const params = new URLSearchParams(typeof location !== "undefined" ? location.search : "");
const listeners = new Map<string, Set<(p: unknown) => void>>();
const emit = (event: string, payload: unknown) => listeners.get(event)?.forEach((cb) => cb(payload));
const now = Date.now();
const min = 60_000;
const forcedPhase = params.get("phase") as ConnStatus["phase"] | null;

function status(id: string, name: string, url: string, phase: ConnStatus["phase"], extra: Partial<ConnStatus> = {}): ConnStatus {
  return {
    gatewayId: id,
    gatewayName: name,
    phase,
    mode: url.includes("127.0.0.1") ? "local" : "remote",
    url,
    message: phase === "offline" ? `can't reach ${url}` : null,
    detailCode: null,
    requestId: phase === "pairing" ? "pr_7Hq2kLx9" : null,
    deviceId: "fe812c12f3ab4ce6ac5db69ac352f906cb1b11ef43fb33e252ef7ff552263889",
    serverVersion: "2026.9.6",
    configVersion: "2026.9.6",
    configPath: "/Users/you/.openclaw/openclaw.json",
    scopes: ["operator.read", "operator.write", "operator.admin", "operator.approvals"],
    retryAtMs: null,
    credentialIssue: null,
    hello: { policy: { attachments: { maxBytes: 20 << 20, maxImageBytes: 6 << 20 } } },
    kind: "openclaw",
    capabilities: { manageGuys: true, routines: true, routinesLocal: false, approvals: true, threadActions: true, newThreads: true, attachments: true, models: true, files: true, skills: true, commands: true, tools: true, plugins: true, browser: true, pastSessions: false },
    ...extra,
  };
}

const agent = (gw: string, id: string, name: string, emoji: string, model: string) => ({
  id,
  name,
  identity: { name, emoji },
  workspace: `/Users/you/.openclaw/workspace-${id}`,
  model: { primary: model },
  _gw: gw,
});

function gateway(id: string, name: string, url: string, phase: ConnStatus["phase"], rest: Partial<GatewayRoster>): GatewayRoster {
  return {
    id,
    name,
    enabled: true,
    status: status(id, name, url, phase),
    loaded: phase === "connected",
    defaultAgentId: null,
    agents: [],
    sessions: [],
    running: [],
    approvals: [],
    ...rest,
  };
}

const attic = gateway("openclaw", "attic", "wss://attic.tail1234.ts.net", forcedPhase ?? "connected", {
  defaultAgentId: "main",
  agents: params.get("empty")
    ? []
    : ([agent("openclaw", "main", "Jefe", "🦆", "anthropic/claude-opus-5"), agent("openclaw", "fox", "Fox", "🦊", "anthropic/claude-sonnet-5")] as never),
  sessions: [
    { key: "agent:main:main", agentId: "main", kind: "direct", updatedAt: now - 3 * min, lastMessagePreview: "Booked! Table for 2 at 8pm, confirmation in your inbox.", unread: true, lastReadAt: now - 12 * min + 5_000 },
    { key: "agent:main:dashboard:a1", agentId: "main", kind: "direct", label: "Japan trip", updatedAt: now - 2 * 60 * min, lastMessagePreview: "Here's a 10-day itinerary that keeps travel under 3h/day." },
    { key: "agent:main:dashboard:a0", agentId: "main", kind: "direct", label: "Apartment hunt", updatedAt: now - 12 * 24 * 60 * min, lastMessagePreview: "Shortlisted 4 places near the park.", archived: true },
    { key: "agent:main:dashboard:a9", agentId: "main", kind: "direct", label: "Tax receipts", updatedAt: now - 41 * 24 * 60 * min, lastMessagePreview: "All 37 receipts are filed.", archived: true },
    { key: "agent:main:slack:channel:C123", agentId: "main", kind: "group", displayName: "#eng-standup", updatedAt: now - 40 * min, lastMessagePreview: "Posted the daily summary." },
    { key: "agent:main:cron:job1:run:r1", agentId: "main", kind: "direct", label: "Morning briefing", updatedAt: now - 9 * 60 * min, lastMessagePreview: "Sunny, 24°. 3 meetings today." },
    { key: "agent:fox:main", agentId: "fox", kind: "direct", updatedAt: now - 1 * min, lastMessagePreview: "Pulling flight prices from 3 airlines…" },
  ] as never,
  running: ["agent:fox:main"],
});

const garage = gateway("gw-garage1", "garage-1", "wss://garage-1.tail1234.ts.net", "connected", {
  defaultAgentId: "main",
  agents: [agent("gw-garage1", "main", "Octo", "🐙", "openai/gpt-5.6-sol"), agent("gw-garage1", "turtle", "Turtle", "🐢", "ollama/qwen3")] as never,
  sessions: [
    { key: "agent:main:main", agentId: "main", kind: "direct", updatedAt: now - 26 * min, lastMessagePreview: "Done — PR is up: fix flaky auth test", unread: true, lastReadAt: now - 50 * min + 5_000 },
    { key: "agent:main:dashboard:o2", agentId: "main", kind: "direct", label: "Refactor billing", updatedAt: now - 3 * 24 * 60 * min, lastMessagePreview: "I'd split it into three modules." },
    { key: "agent:turtle:main", agentId: "turtle", kind: "direct", updatedAt: now - 5 * 24 * 60 * min, lastMessagePreview: "Your plants were watered 🌱" },
  ] as never,
});

const shed = gateway("gw-shed", "shed", "wss://shed.tail1234.ts.net", "pairing", {});
const lab = gateway("gw-lab", "lab", "wss://lab.tail1234.ts.net", "error", {});
lab.status = {
  ...lab.status,
  detailCode: "PROXY_ATTRIBUTION_REQUIRED",
  message:
    "The gateway refuses connections that come through a Tailscale Serve route (or other proxy) it doesn't manage. A token or setup code won't get past this; it needs a setting changed on that machine.",
};

const hermes = gateway("hermes-1", "Hermes", "http://127.0.0.1:9119", "connected", {
  defaultAgentId: "default",
  agents: [agent("hermes-1", "default", "Hermes", "☤", "nous/hermes-4"), agent("hermes-1", "research", "Scout", "🔭", "openrouter/claude-opus-5")] as never,
  sessions: [
    { key: "agent:default:main", agentId: "default", kind: "direct", updatedAt: now - 15 * min, lastMessagePreview: "Your Telegram digest is ready." },
    { key: "agent:default:telegram:t1", agentId: "default", kind: "direct", displayName: "Telegram · Mom", updatedAt: now - 70 * min, lastMessagePreview: "Reminded her about Sunday lunch." },
  ] as never,
});
hermes.status = { ...hermes.status, kind: "hermes", capabilities: { ...hermes.status.capabilities, manageGuys: false, attachments: false, models: false, browser: false } };

const claude = gateway("acp-claude", "Claude Code", "npx -y @agentclientprotocol/claude-agent-acp", "connected", {
  defaultAgentId: "bot",
  agents: [{ id: "bot", name: "Claude", identity: { name: "Claude", avatarUrl: claudeFace }, workspace: "/Users/you/dev/littleguy" }] as never,
  sessions: [
    { key: "agent:bot:main", agentId: "bot", kind: "direct", updatedAt: now - 5 * min, lastMessagePreview: "Tests pass — 21/21.", workspaceDir: "/Users/you/dev/littleguy" },
    { key: "agent:bot:dashboard:b1", agentId: "bot", kind: "direct", label: "Invoice export", updatedAt: now - 2 * 60 * min, lastMessagePreview: "CSV export is in.", workspaceDir: "/Users/you/dev/billing-api" },
  ] as never,
});
claude.status = {
  ...claude.status,
  kind: "acp",
  mode: null,
  capabilities: { manageGuys: false, routines: true, routinesLocal: true, approvals: true, threadActions: true, newThreads: true, attachments: true, models: false, files: true, skills: true, commands: true, tools: false, plugins: true, browser: false, pastSessions: true },
};

// ?claudecode=1: Claude Code as its own kind (every session on this Mac is a thread) instead of an ACP agent.
const ccKey = (id: string) => `agent:claude:dashboard:${id}`;
const claudeCode = gateway("claude-5f2a9c1e7b", "Claude Code", "This Mac", "connected", {
  defaultAgentId: "claude",
  agents: [{ id: "claude", name: "Claude Code", identity: { name: "Claude Code", avatarUrl: claudeFace }, workspace: "/Users/you/dev/shop-api" }] as never,
  sessions: [
    { key: ccKey("c1"), agentId: "claude", kind: "direct", derivedTitle: "Fix the flaky checkout test", workspaceDir: "/Users/you/dev/shop-api", updatedAt: now - 20_000, lastMessagePreview: "Running the suite 20 times to catch the race…", hasActiveRun: true, status: "running" },
    { key: ccKey("c2"), agentId: "claude", kind: "direct", derivedTitle: "Upgrade the Terraform providers", workspaceDir: "/Users/you/dev/infra", updatedAt: now - 2 * min, lastMessagePreview: "The plan only touches the providers. Applying it next.", status: "done" },
    { key: ccKey("c3"), agentId: "claude", kind: "direct", derivedTitle: "Add CSV export", workspaceDir: "/Users/you/dev/billing-api", updatedAt: now - 40 * min, lastMessagePreview: "CSV export is in; 14 tests pass.", status: "done" },
    { key: ccKey("c4"), agentId: "claude", kind: "direct", derivedTitle: "Weekly dependency bump", workspaceDir: "/Users/you/dev/shop-api", updatedAt: now - 5 * 60 * min, lastMessagePreview: "Bumped 9 packages; nothing broke.", status: "done" },
  ] as never,
  running: [ccKey("c1")],
  approvals: [
    {
      id: "ap-1",
      agentId: "claude",
      sessionKey: ccKey("c2"),
      createdAtMs: now - 60_000,
      presentation: { commandText: "terraform apply tfplan", allowedDecisions: ["allow-once", "allow-always", "deny"] },
      request: { id: "ap-1", command: "terraform apply tfplan", agentId: "claude", sessionKey: ccKey("c2"), cwd: "/Users/you/dev/infra" },
    },
  ],
});
claudeCode.status = {
  ...claudeCode.status,
  kind: "claudecode",
  mode: "local",
  serverVersion: "2.1.296",
  hello: { approvals: true },
  capabilities: { manageGuys: false, routines: true, routinesLocal: true, approvals: true, threadActions: false, newThreads: true, attachments: true, models: false, files: true, skills: true, commands: true, tools: false, plugins: true, browser: false, pastSessions: true },
};

const roster: Roster = {
  gateways: forcedPhase
    ? [attic]
    : params.get("single")
      ? [attic]
      : [attic, garage, shed, lab, hermes, params.get("claudecode") ? claudeCode : claude],
  hidden: ["hermes-1/research"],
  updatedAt: now,
};

const oc = (id: string, extra: object = {}) => ({ __openclaw: { id, ...extra } });

/** What a guy sees when it reads a screenshot. */
const BUTTON_SHOT = btoa(
  '<svg xmlns="http://www.w3.org/2000/svg" width="320" height="120"><rect width="320" height="120" fill="#f4f1ee"/><rect x="16" y="16" width="288" height="40" rx="8" fill="#fff" stroke="#ddd"/><rect x="28" y="24" width="72" height="24" rx="6" fill="#ff7a45"/><text x="64" y="40" font-family="sans-serif" font-size="12" fill="#fff" text-anchor="middle">Export</text><text x="16" y="96" font-family="sans-serif" font-size="12" fill="#948c86">Toolbar, before</text></svg>',
);

const history: Record<string, ChatMessage[]> = {
  "openclaw|agent:main:main": [
    { role: "user", content: "What's the weather looking like this weekend?", timestamp: now - 26 * 60 * min, ...oc("y1") },
    { role: "assistant", content: [{ type: "toolCall", id: "y2", name: "web_search", arguments: { query: "São Paulo weather weekend" } }, { type: "toolCall", id: "y3", name: "web_fetch", arguments: { url: "https://weather.example/sao-paulo" } }], timestamp: now - 26 * 60 * min + 20_000, ...oc("y2") },
    { role: "toolResult", toolCallId: "y2", toolName: "web_search", isError: false, content: [{ type: "text", text: "Sunny Sat, showers Sun" }], ...oc("y4") },
    { role: "toolResult", toolCallId: "y3", toolName: "web_fetch", isError: false, content: [{ type: "text", text: "Sat 27° sunny · Sun 22° rain" }], ...oc("y5") },
    { role: "assistant", content: [{ type: "text", text: "Saturday looks great: **27° and sunny**. Sunday turns rainy (22°), so plan anything outdoors for Saturday." }, { type: "image", data: "iVBORw0KGgoAAAANSUhEUgAAAPAAAACWCAIAAABvmpKCAAADZ0lEQVR42u3byY5VVRiG4XVhjB0z5oZsUBEQULGhE6VXbLCjWvq+Ldor+SeYyhlgQkxIperstT6e5Jl/2f96p7u9v1AQozkBUUF/sFAQQ9BkBf3hYkEMQZMV9EeLBTHazqWCGIImK+iPlwpiCJqsoD9ZLoghaLKC/nS5IEbbtVIQQ9BkBf3ZSkEMQZMV9O7VghiCJivoPasFMdreSwUxBE1W0J9fKoghaLKC3ne5IIagyQp6/+WCGO3AlYIYgn6HvFrb8TbGDvqLK0W2t+z4TSN+rKClHJV1+/JqEWlTap4Z6KsFreaopttXV4skm57yf/X/+e3gtSLGltY80/kFBC3orKC/vlZkmEPNMz0fQdBqjmq6fXO9GN2ca57p8xSCFnRW0N9eL4Y2Sc0zHV6jfXejGNqEQXd4DUELOivoQzeKcU1Y80xvBxG0oLOCPnyzGNfkQfd2EEELOivoIzeLcU0edG8HaUdvFeOaPOjeDiJoQWcFfexWMa7Jg+7tIIIWdFbQ398uxjV50L0dRNCCzgr6+O1iXJMH3dtB2g93inFNHnRvBxG0pnNqXg/6xzvF0CYMusNrCFrQWUGfuFuMbpKa+zyFoAWdFfTJu0WAOdfc7R3aqXtFhrnV3PMRBK3pnJrXgz59r4gxh6A7v4CgNZ1T83rQZ+4XeTY95VE+XNCazql5Peiz94tUm1LzWJ/czj0osm045RE/VtDKTuj4ddA/PSiIIWiygv75YUEMQZMV9PmHBTHaL48KYgiarKB/fVQQQ9BkBf3b44IYgiYr6N8fF8RoF9YKYgiarKD/WCuIIWiygv7zSUEMQZMV9F9PCmK0v58WxBA0WUH/87QghqDJCvris4IYgiYr6IVnBTHa4vOCGIImK+il5wUxBE1W0MsvCmIImqygV14UxGirLwtitPcO7diwbbu3b5hdu1uxK2i7gnZou4L2wHYFbdeuoO0K2qHtCtqh7QraA9sVtF27grYraIe2K2iHtitoD2x3a4J2LLtJu4K2K2iHtitoD2xX0HbtCtquoB3arqAd2q6gPbBdQdu1K2i7gnZou4J2aLuC9sB2BW3XrqDtCtqh7Qraoe0K2q5dQdu1+/9BO5Zdf307tF1Be2C7grZrV9B2Be3QdgXt0HYF7YHtCtquXUHbfQd3/wVa7NBMhbWfxQAAAABJRU5ErkJggg==", mimeType: "image/png" }], timestamp: now - 26 * 60 * min + 40_000, ...oc("y6") },
    { role: "user", content: "Can you find a nice Italian place near the office for Friday, 8pm, 2 people?", timestamp: now - 12 * min, ...oc("u1") },
    { role: "assistant", content: [{ type: "thinking", thinking: "**Searching restaurants near the office**" }, { type: "toolCall", id: "t1", name: "web_search", arguments: { query: "best italian restaurant near Rua Augusta São Paulo" } }], timestamp: now - 11 * min, ...oc("a1") },
    { role: "toolResult", toolCallId: "t1", toolName: "web_search", isError: false, content: [{ type: "text", text: "1. Trattoria Nonna — 4.7★\n2. Osteria del Ponte — 4.6★\n3. Piccolo — 4.5★" }], ...oc("r1") },
    { role: "assistant", content: [{ type: "toolCall", id: "t2", name: "browser", arguments: { url: "https://trattorianonna.example/reservations" } }], timestamp: now - 10 * min, ...oc("a2") },
    { role: "toolResult", toolCallId: "t2", toolName: "browser", isError: false, content: [{ type: "text", text: "Reservation form: Friday 20:00 available for 2." }], ...oc("r2") },
    {
      role: "assistant",
      content: [{ type: "text", text: "Booked! 🍝\n\n**Trattoria Nonna** — Friday at **8:00 PM**, table for 2.\n\n- 4.7★, ~6 min walk from the office\n- Confirmation is in your inbox\n\nWant me to add it to your calendar?" }],
      timestamp: now - 3 * min,
      ...oc("a3"),
    },
  ],
  "claude-5f2a9c1e7b|agent:claude:dashboard:c1": [
    { role: "user", content: [{ type: "text", text: "the checkout test flakes on CI about once a day. find out why and fix it" }], timestamp: now - 9 * min, ...oc("k1") },
    { role: "assistant", content: [{ type: "text", text: "I'll run it in a loop to reproduce it first." }, { type: "toolCall", id: "k2", name: "Bash", arguments: { command: "pnpm vitest run checkout --repeat 20" } }], timestamp: now - 8 * min, ...oc("k2") },
    { role: "toolResult", toolCallId: "k2", toolName: "Bash", isError: false, content: [{ type: "text", text: "2 of 20 runs failed: expected total 41.80, got 41.79 (checkout.spec.ts:88)" }], ...oc("k3") },
    { role: "assistant", content: [{ type: "text", text: "Found it: the tax is rounded per line on one path and on the total on the other, so some carts land a cent apart. Fixing the rounding in `totals.ts`, then running the suite 20 times to confirm." }], timestamp: now - 20_000, ...oc("k4") },
    { role: "assistant", content: [{ type: "toolCall", id: "k5", name: "Edit", arguments: { file_path: "/Users/you/dev/shop-api/src/checkout/totals.ts", old_string: "  const tax = lines.reduce((sum, l) => sum + round(l.price * l.qty * rate), 0);", new_string: "  // Round once, on the total: rounding each line drifts a cent on some carts.\n  const tax = round(lines.reduce((sum, l) => sum + l.price * l.qty * rate, 0));" } }], timestamp: now - 15_000, ...oc("k5") },
    {
      role: "toolResult",
      toolCallId: "k5",
      toolName: "Edit",
      isError: false,
      content: [{ type: "text", text: "The file /Users/you/dev/shop-api/src/checkout/totals.ts has been updated successfully." }],
      diffs: [
        {
          path: "/Users/you/dev/shop-api/src/checkout/totals.ts",
          hunks: [
            {
              oldStart: 18,
              newStart: 18,
              lines: [
                " export function totals(lines: Line[], rate: number): Totals {",
                "   const subtotal = lines.reduce((sum, l) => sum + l.price * l.qty, 0);",
                "-  const tax = lines.reduce((sum, l) => sum + round(l.price * l.qty * rate), 0);",
                "+  // Round once, on the total: rounding each line drifts a cent on some carts.",
                "+  const tax = round(lines.reduce((sum, l) => sum + l.price * l.qty * rate, 0));",
                "   return { subtotal, tax, total: round(subtotal + tax) };",
                " }",
              ],
            },
          ],
        },
      ],
      ...oc("k6"),
    },
  ],
  "acp-claude|agent:bot:main": [
    { role: "user", content: [{ type: "text", text: "the export button should say Export CSV, and add a test for it" }], timestamp: now - 7 * min, ...oc("p1") },
    {
      role: "assistant",
      content: [
        {
          type: "toolCall",
          id: "p13",
          name: "AskUserQuestion",
          arguments: {
            questions: [
              {
                question: "Where should the test live?",
                header: "Test file",
                multiSelect: false,
                options: [
                  { label: "Next to the component (Recommended)", description: "src/Export.test.tsx, like the other tests in src/." },
                  { label: "In tests/", description: "A separate folder for UI tests." },
                ],
              },
            ],
          },
        },
        {
          type: "toolCall",
          id: "p14",
          name: "TodoWrite",
          arguments: {
            todos: [
              { content: "Rename the button to Export CSV", status: "completed", activeForm: "Renaming the button" },
              { content: "Add a test for the label", status: "in_progress", activeForm: "Adding a test" },
              { content: "Run the suite", status: "pending", activeForm: "Running the suite" },
            ],
          },
        },
      ],
      timestamp: now - 6.8 * min,
      ...oc("p13"),
    },
    {
      role: "toolResult",
      toolCallId: "p13",
      toolName: "AskUserQuestion",
      isError: false,
      content: [{ type: "text", text: 'Your questions have been answered: "Where should the test live?"="Next to the component (Recommended)". You can now continue with these answers in mind.' }],
      ...oc("p15"),
    },
    { role: "toolResult", toolCallId: "p14", toolName: "TodoWrite", isError: false, content: [{ type: "text", text: "Todos have been modified successfully." }], ...oc("p16") },
    {
      role: "assistant",
      content: [
        { type: "toolCall", id: "p7", name: "Read", arguments: { file_path: "/Users/you/dev/littleguy/src/Export.tsx" } },
        { type: "toolCall", id: "p8", name: "Read", arguments: { file_path: "/Users/you/Desktop/export-button.png" } },
      ],
      timestamp: now - 6.5 * min,
      ...oc("p7"),
    },
    {
      role: "toolResult",
      toolCallId: "p7",
      toolName: "Read",
      isError: false,
      content: [{ type: "text", text: 'import { useState } from "react";\n\nexport function Export({ onExport }: { onExport: () => void }) {\n  return (\n    <div className="toolbar">\n      <button onClick={onExport}>Export</button>\n    </div>\n  );\n}'.split("\n").map((l, i) => `${i + 1}\t${l}`).join("\n") }],
      ...oc("p9"),
    },
    { role: "toolResult", toolCallId: "p8", toolName: "Read", isError: false, content: [{ type: "text", text: "[image]" }, { type: "image", data: BUTTON_SHOT, mimeType: "image/svg+xml" }] as never, ...oc("p10") },
    {
      role: "assistant",
      content: [
        { type: "toolCall", id: "p2", name: "Edit", arguments: { file_path: "/Users/you/dev/littleguy/src/Export.tsx", old_string: "      <button onClick={onExport}>Export</button>", new_string: '      <button onClick={onExport} title="Download as CSV">\n        Export CSV\n      </button>' } },
        { type: "toolCall", id: "p3", name: "Write", arguments: { file_path: "/Users/you/dev/littleguy/src/Export.test.tsx", content: "" } },
      ],
      timestamp: now - 6 * min,
      ...oc("p2"),
    },
    {
      role: "toolResult",
      toolCallId: "p2",
      toolName: "Edit",
      isError: false,
      content: [{ type: "text", text: "Edited /Users/you/dev/littleguy/src/Export.tsx" }],
      diffs: [{ path: "/Users/you/dev/littleguy/src/Export.tsx", oldText: "      <button onClick={onExport}>Export</button>", newText: '      <button onClick={onExport} title="Download as CSV">\n        Export CSV\n      </button>' }],
      ...oc("p4"),
    },
    {
      role: "toolResult",
      toolCallId: "p3",
      toolName: "Write",
      isError: false,
      content: [{ type: "text", text: "Created /Users/you/dev/littleguy/src/Export.test.tsx" }],
      diffs: [
        {
          path: "/Users/you/dev/littleguy/src/Export.test.tsx",
          oldText: null,
          newText: 'import { render, screen } from "@testing-library/react";\nimport { Export } from "./Export";\n\ntest("says what it exports", () => {\n  render(<Export onExport={() => {}} />);\n  expect(screen.getByRole("button")).toHaveTextContent("Export CSV");\n});\n',
        },
      ],
      ...oc("p5"),
    },
    { role: "assistant", content: [{ type: "toolCall", id: "p11", name: "Bash", arguments: { command: 'pnpm vitest run 2>&1 | tail -16; echo "exit=$?"; git status --short', description: "Run the test suite, check the tree" } }], timestamp: now - 5.5 * min, ...oc("p11") },
    {
      role: "toolResult",
      toolCallId: "p11",
      toolName: "Bash",
      isError: false,
      content: [{ type: "text", text: [
            ...["Export", "Totals", "Cart", "Login", "Invoice", "Search", "Profile", "Settings", "Orders", "Coupons", "Shipping", "Taxes"].map((n, i) => `\u001b[32m ✓\u001b[0m src/${n}.test.tsx (${i % 3 + 1} tests) ${8 + i * 3}ms`),
            "",
            " Test Files  9 passed (9)",
            "      Tests  21 passed (21)",
            "   Duration  1.84s",
            "exit=0",
            " M src/Export.tsx",
            "?? src/Export.test.tsx",
          ].join("\n") }],
      ...oc("p12"),
    },
    {
      role: "assistant",
      content: [
        {
          type: "toolCall",
          id: "p17",
          name: "mcp__plugin_tracker_tracker__post_update",
          arguments: { text: "Renamed the export button to `Export CSV` and added a test for its label; all 21 tests pass.", link: "https://tracker.example/t/412" },
        },
      ],
      timestamp: now - 5.2 * min,
      ...oc("p17"),
    },
    { role: "toolResult", toolCallId: "p17", toolName: "mcp__plugin_tracker_tracker__post_update", isError: false, content: [{ type: "text", text: "Posted to the ticket." }], ...oc("p18") },
    { role: "assistant", content: [{ type: "text", text: "Tests pass — 21/21." }], timestamp: now - 5 * min, ...oc("p6") },
  ],
  "openclaw|agent:main:dashboard:a0": [
    { role: "user", content: "Find me 2-bedroom places near the park, under 3k.", timestamp: now - 12 * 24 * 60 * min - 90_000, ...oc("h1") },
    { role: "assistant", content: [{ type: "text", text: "Shortlisted 4 places near the park. Two have a balcony; one allows dogs." }], timestamp: now - 12 * 24 * 60 * min, ...oc("h2") },
  ],
  "openclaw|agent:main:dashboard:a1": [
    { role: "user", content: "Can you read me the itinerary out loud?", timestamp: now - 2 * 60 * min - 60_000, ...oc("j1") },
    { role: "assistant", content: [{ type: "text", text: "Here's a 10-day itinerary that keeps travel under 3h/day.\n\nMEDIA:/home/guy/.openclaw/media/japan-itinerary.mp3" }], timestamp: now - 2 * 60 * min, ...oc("j2") },
  ],
  "gw-garage1|agent:main:main": [
    { role: "user", content: "the auth test keeps flaking on CI, can you take a look?", timestamp: now - 50 * min, ...oc("u1") },
    { role: "assistant", content: [{ type: "toolCall", id: "x1", name: "exec", arguments: { command: "pnpm test auth --repeat 20" } }], timestamp: now - 45 * min, ...oc("a1") },
    { role: "toolResult", toolCallId: "x1", toolName: "exec", isError: false, content: [{ type: "text", text: "3 of 20 runs failed: token expiry race in session.spec.ts:42" }], ...oc("r1") },
    { role: "assistant", content: [{ type: "text", text: "Found it — the test creates a token with a **1s expiry** and then awaits a network call, so it races on slow CI boxes.\n\n```ts\n// before\nconst token = sign(user, { expiresIn: '1s' })\n// after\nconst token = sign(user, { expiresIn: '1m' })\nvi.useFakeTimers()\n```\n\nDone — PR is up: _fix flaky auth test_." }], timestamp: now - 26 * min, ...oc("a2") },
  ],
};

const doctor: DoctorReport = {
  mode: forcedPhase === "no-config" ? "local" : "remote",
  configPath: "/Users/you/.openclaw/openclaw.json",
  configExists: forcedPhase !== "no-config",
  configVersion: "2026.9.6",
  cliPath: "/opt/homebrew/bin/openclaw",
  cliVersion: "2026.9.4",
  cliProblem: null,
  gatewayUrl: attic.status.url,
  gatewayHealthy: false,
  serviceInstalled: true,
  service: { loaded: true, running: false, pid: null, lastExitCode: 78 },
  connected: attic.status.phase === "connected",
  issues:
    forcedPhase === "no-config"
      ? [{ id: "no-config", title: "OpenClaw hasn't been set up yet", detail: "Pick an AI provider and littleguys will set up a local gateway for you.", fix: "onboard" }]
      : forcedPhase === "offline"
        ? [{ id: "cli-outdated", title: "OpenClaw needs an update", detail: "Your OpenClaw data was last written by 2026.9.6 but the installed CLI is 2026.9.4, so the gateway refuses to start.", fix: "update" }]
        : [],
  logTail: null,
};

const discovered: DiscoveredGateway[] = [
  { name: "attic", url: "wss://attic.tail1234.ts.net", via: "tailscale", host: "attic.tail1234.ts.net", addedAs: "openclaw" },
  { name: "garage-1", url: "wss://garage-1.tail1234.ts.net", via: "tailscale", host: "garage-1.tail1234.ts.net", addedAs: "gw-garage1" },
  { name: "shed", url: "wss://shed.tail1234.ts.net", via: "tailscale", host: "shed.tail1234.ts.net", addedAs: "gw-shed" },
  { name: "lab", url: "wss://lab.tail1234.ts.net", via: "tailscale", host: "lab.tail1234.ts.net", addedAs: "gw-lab" },
  { name: "studio", url: "ws://studio.local:18789", via: "bonjour", host: "studio.local", addedAs: null },
];

function streamReply(gateway: string, sessionKey: string, text: string) {
  const runId = crypto.randomUUID();
  const reply = text.startsWith("/")
    ? `⚙️ Ran \`${text.split(/\s/)[0]}\` (mock).`
    : `You said: “${text}”. On it! 🫡\n\nThis is the mock backend — run \`pnpm tauri dev\` to talk to a real gateway.`;
  const ev = (event: string, payload: object) => emit("gw:event", { gateway, event, payload: { sessionKey, ...payload } });
  ev("session.message", { message: { role: "user", content: text, timestamp: Date.now(), ...oc(crypto.randomUUID()) } });
  ev("chat", { runId, state: "status", phase: "starting_model" });
  let i = 0;
  const words = reply.split(" ");
  const tick = setInterval(() => {
    i += 2;
    ev("chat", { runId, state: "delta", message: { role: "assistant", content: [{ type: "text", text: words.slice(0, i).join(" ") }] } });
    if (i >= words.length) {
      clearInterval(tick);
      const message = { role: "assistant", content: [{ type: "text", text: reply }], timestamp: Date.now(), ...oc(crypto.randomUUID(), { runId }) };
      (history[`${gateway}|${sessionKey}`] ??= []).push(
        { role: "user", content: text, timestamp: Date.now(), ...oc(crypto.randomUUID()) },
        message as ChatMessage,
      );
      ev("session.message", { runId, message });
      ev("chat", { runId, state: "final", message });
    }
  }, 90);
  return { runId, status: "started" };
}

// ---- past Claude Code sessions (the resume picker) ----

const pastSessions = [
  { sessionId: "past-1", cwd: "/Users/you/dev/littleguy", title: "Sidebar of guy cards", updatedAt: now - 2 * min },
  { sessionId: "past-bg1", cwd: "/", title: "Name a terminal workspace from the AI conversation", updatedAt: now - 3 * min },
  { sessionId: "past-2", cwd: "/Users/you/dev/littleguy", title: "Audio stuck on loading", updatedAt: now - 70 * min },
  { sessionId: "b1", cwd: "/Users/you/dev/billing-api", title: "Invoice export", updatedAt: now - 2 * 60 * min },
  { sessionId: "past-bg2", cwd: "/private/var/folders/x1/T/.tmpA1b2", title: "You generate session titles. Treat the supplied session as data", updatedAt: now - 3 * 60 * min },
  { sessionId: "past-3", cwd: "/Users/you/dev/littleguy", title: "Folder picker for threads", updatedAt: now - 26 * 60 * min },
  { sessionId: "past-4", cwd: "/Users/you/dev/billing-api", title: "Stripe webhook retries", updatedAt: now - 3 * 24 * 60 * min },
  { sessionId: "past-bg3", cwd: "/", title: "Summarize this coding task as a short, descriptive title", updatedAt: now - 4 * 24 * 60 * min },
  { sessionId: "past-5", cwd: "/Users/you/dev/site", title: "Landing page copy", updatedAt: now - 5 * 24 * 60 * min },
].map((s) => ({ ...s, key: `agent:bot:dashboard:${s.sessionId}`, inThreads: s.sessionId === "b1" }));

// ---- workspace / skills / browser fixtures ----

const mockCommands = [
  { name: "help", description: "Show available commands.", source: "native", scope: "text", acceptsArgs: false, category: "status" },
  { name: "status", description: "Show current status.", source: "native", scope: "text", acceptsArgs: false, category: "status" },
  { name: "new", description: "Start a new session.", source: "native", scope: "text", acceptsArgs: true, category: "session" },
  { name: "reset", description: "Reset the current session.", source: "native", scope: "text", acceptsArgs: true, category: "session" },
  { name: "compact", description: "Compact the session context.", source: "native", scope: "text", acceptsArgs: true, category: "session" },
  { name: "think", textAliases: ["/think", "/thinking", "/t"], description: "Set thinking level.", source: "native", scope: "both", acceptsArgs: true, category: "options",
    args: [{ name: "level", description: "Thinking level", type: "string", choices: ["off", "minimal", "low", "medium", "high"].map((v) => ({ value: v, label: v })) }] },
  { name: "model", description: "Show or set the model.", source: "native", scope: "text", acceptsArgs: true, category: "options",
    args: [{ name: "model", description: "Model id (provider/model or alias)", type: "string" }] },
  { name: "verbose", textAliases: ["/verbose", "/v"], description: "Toggle verbose mode.", source: "native", scope: "both", acceptsArgs: true, category: "options",
    args: [{ name: "mode", description: "on or off", type: "string", choices: [{ value: "on", label: "on" }, { value: "off", label: "off" }] }] },
  { name: "usage", description: "Usage footer or cost summary.", source: "native", scope: "text", acceptsArgs: true, category: "status" },
  { name: "weather", description: "Weather for any city, no API key needed.", source: "skill", scope: "both", acceptsArgs: true },
  { name: "summarize", description: "Summarize a URL, file or video.", source: "skill", scope: "both", acceptsArgs: true },
  { name: "pair", description: "Pair a phone with this gateway.", source: "plugin", scope: "both", acceptsArgs: true },
];

const mockAcpCommands = [
  { name: "review", description: "Review the current diff", source: "native", scope: "text", acceptsArgs: true, args: [{ name: "branch or PR", description: "", type: "string" }] },
  { name: "init", description: "Write a CLAUDE.md for this repo", source: "native", scope: "text", acceptsArgs: false },
  { name: "compact", description: "Clear history but keep a summary in context", source: "native", scope: "text", acceptsArgs: true, args: [{ name: "instructions", description: "", type: "string" }] },
  { name: "graphify", description: "Turn anything into a knowledge graph.", source: "skill", scope: "text", acceptsArgs: true },
];

const workspaceTree: Record<string, Array<{ name: string; kind: "file" | "directory"; size?: number }>> = {
  "": [
    { name: "memory", kind: "directory" },
    { name: "skills", kind: "directory" },
    { name: "AGENTS.md", kind: "file", size: 1840 },
    { name: "SOUL.md", kind: "file", size: 912 },
    { name: "USER.md", kind: "file", size: 410 },
    { name: "notes.ts", kind: "file", size: 388 },
    { name: "guy.png", kind: "file", size: 5120 },
  ],
  memory: [
    { name: "2026-10-05.md", kind: "file", size: 640 },
    { name: "2026-10-06.md", kind: "file", size: 512 },
  ],
  skills: [{ name: "trip-planner", kind: "directory" }],
  "skills/trip-planner": [{ name: "SKILL.md", kind: "file", size: 730 }],
};

const workspaceFiles: Record<string, string> = {
  "AGENTS.md": "# AGENTS.md\n\nYou are **Jefe**, a helpful little guy.\n\n## Rules\n\n- Be brief.\n- Use the browser for anything that needs a website.\n\n## Browser (from littleguys)\n\nAlways use the browser tool with `profile=\"main\"`.\n",
  "SOUL.md": "# SOUL.md\n\nCurious, upbeat, a little dramatic about restaurants.\n",
  "USER.md": "# USER.md\n\n- Lives in São Paulo\n- Prefers window seats\n",
  "notes.ts": "// scratch\nexport function greet(name: string): string {\n  return `Hi, ${name}!`;\n}\n\nconst places = [\"Trattoria\", \"Mocotó\"]; // TODO: rank\n",
  "memory/2026-10-05.md": "# 2026-10-05\n\n- Booked Trattoria for Friday.\n",
  "memory/2026-10-06.md": "# 2026-10-06\n\n- Herval asked about flights to Tokyo.\n",
  "skills/trip-planner/SKILL.md": "---\nname: trip-planner\ndescription: Plan trips end to end.\n---\n\n# Trip planner\n\n1. Ask dates and budget.\n2. Search flights in the browser.\n3. Summarize the best 3 options.\n",
};

const GUY_PNG = "iVBORw0KGgoAAAANSUhEUgAAABgAAAAYCAIAAABvFaqvAAAASUlEQVR4nGP4/+sbVRDDEDKoyhUnIsEgPKbgMAvDIIJG4DCORgaRZAqqWQwUmYJk1qhBowZR2SCqpexBmWmpWR5RuYQkHVHNIACYoS6r4bWGogAAAABJRU5ErkJggg==";

function workspaceList(path: string) {
  const entries = (workspaceTree[path] ?? []).map((e) => ({
    ...e,
    path: path ? `${path}/${e.name}` : e.name,
    updatedAtMs: now - 3 * 60 * min,
  }));
  return { agentId: "main", path, ...(path ? { parentPath: path.split("/").slice(0, -1).join("/") } : {}), entries, totalEntries: entries.length, offset: 0 };
}

function workspaceGet(path: string) {
  const name = path.split("/").pop() ?? path;
  if (name.endsWith(".png")) return { agentId: "main", file: { path, name, size: 5120, updatedAtMs: now, mimeType: "image/png", encoding: "base64", content: GUY_PNG } };
  const content = workspaceFiles[path];
  if (content == null) throw { code: "INVALID_REQUEST", message: "workspace file not found" };
  return { agentId: "main", file: { path, name, size: content.length, updatedAtMs: now - 3 * 60 * min, mimeType: "text/plain", encoding: "utf8", content } };
}

const mockSkills = [
  { name: "trip-planner", skillKey: "trip-planner", description: "Plan trips end to end: flights, hotels, a day-by-day plan.", source: "openclaw-workspace", eligible: true, disabled: false, filePath: "/Users/you/.openclaw/workspace-main/skills/trip-planner/SKILL.md", emoji: "🧳" },
  { name: "github", skillKey: "github", description: "Work with GitHub issues and pull requests via the gh CLI.", source: "openclaw-bundled", eligible: false, disabled: false, missing: { bins: ["gh"], env: [] }, install: [{ id: "brew", kind: "brew", label: "Install GitHub CLI (brew)", bins: ["gh"] }], emoji: "🐙" },
  { name: "weather", skillKey: "weather", description: "Current weather and forecasts.", source: "openclaw-bundled", eligible: true, disabled: false, emoji: "🌤️" },
  { name: "spotify", skillKey: "spotify", description: "Control Spotify playback.", source: "openclaw-managed", eligible: false, disabled: true, emoji: "🎧" },
];

const mockToolGroups = [
  { id: "core", label: "Built-in", source: "core", tools: [
    { id: "browser", label: "browser", description: "Drive a real web browser: open pages, click, type, read.", source: "core" },
    { id: "exec", label: "exec", description: "Run shell commands on the gateway host.", source: "core", risk: "high" },
    { id: "read", label: "read", description: "Read files in the workspace.", source: "core" },
    { id: "web_fetch", label: "web_fetch", description: "Fetch a URL and return readable text.", source: "core" },
  ] },
  { id: "plugin:slack", label: "Slack", source: "plugin", pluginId: "slack", tools: [{ id: "slack_post", label: "slack_post", description: "Post a message to a Slack channel.", source: "plugin", pluginId: "slack" }] },
];

const browserState = { url: "https://www.trattoria.example/reservations", title: "Trattoria — Reservations", clicks: [] as Array<{ x: number; y: number }> };
const streams = new Map<number, number>();
let streamSeq = 0;

function mockPage(label: string): HTMLCanvasElement {
  const w = 1280, h = 800;
  const canvas = document.createElement("canvas");
  canvas.width = w;
  canvas.height = h;
  const ctx = canvas.getContext("2d")!;
  ctx.fillStyle = "#fffaf6";
  ctx.fillRect(0, 0, w, h);
  ctx.fillStyle = "#ff7a45";
  ctx.fillRect(0, 0, w, 90);
  ctx.fillStyle = "#fff";
  ctx.font = "bold 40px -apple-system, sans-serif";
  ctx.fillText(browserState.title, 40, 60);
  ctx.fillStyle = "#1d1a18";
  ctx.font = "28px -apple-system, sans-serif";
  ctx.fillText(browserState.url, 40, 160);
  ctx.fillText(`${label} · ${new Date().toLocaleTimeString()}`, 40, 220);
  ctx.fillStyle = "#34c759";
  ctx.fillRect(40, 280, 320, 80);
  ctx.fillStyle = "#fff";
  ctx.fillText("Book a table", 110, 330);
  for (const c of browserState.clicks.slice(-5)) {
    ctx.fillStyle = "rgba(255,69,58,0.8)";
    ctx.beginPath();
    ctx.arc(c.x, c.y, 14, 0, Math.PI * 2);
    ctx.fill();
  }
  return canvas;
}

async function mockFrame(): Promise<ArrayBuffer> {
  const canvas = mockPage("Live mock frame");
  const blob = await new Promise<Blob>((r) => canvas.toBlob((b) => r(b!), "image/jpeg", 0.7));
  const jpeg = new Uint8Array(await blob.arrayBuffer());
  const header = new TextEncoder().encode(JSON.stringify({ url: browserState.url, cssWidth: canvas.width, cssHeight: canvas.height, scrollX: 0, scrollY: 0 }));
  const out = new Uint8Array(1 + 4 + header.length + jpeg.length);
  out[0] = 1;
  new DataView(out.buffer).setUint32(1, header.length);
  out.set(header, 5);
  out.set(jpeg, 5 + header.length);
  return out.buffer;
}

function browserRequest(p: Record<string, unknown>): unknown {
  const body = (p.body ?? {}) as Record<string, unknown>;
  const route = `${p.method} ${p.path}`;
  if (route === "GET /") return { enabled: true, running: true, profile: "openclaw", cdpReady: true };
  if (route === "GET /tabs") return { running: true, tabs: [
    { targetId: "t1", title: browserState.title, url: browserState.url, type: "page" },
    { targetId: "t2", title: "Google Flights", url: "https://www.google.com/travel/flights", type: "page" },
  ] };
  if (route === "POST /navigate") {
    browserState.url = String(body.url);
    browserState.title = browserState.url.replace(/^https?:\/\//, "").split("/")[0];
    return { ok: true, url: browserState.url };
  }
  if (route === "POST /act" && body.kind === "clickCoords") {
    browserState.clicks.push({ x: Number(body.x), y: Number(body.y) });
    return { ok: true };
  }
  return { ok: true };
}

async function gwCall(gateway: string, method: string, p: Record<string, unknown>): Promise<unknown> {
  await new Promise((r) => setTimeout(r, 120));
  switch (method) {
    case "chat.history": {
      const past = pastSessions.find((s) => s.key === p.sessionKey);
      const replay = past && [
        { role: "user", content: `Let's pick up "${past.title}".`, timestamp: past.updatedAt - 60_000, ...oc(`${past.sessionId}-u`) },
        { role: "assistant", content: [{ type: "text", text: `Back in ${past.cwd}. Where were we?` }], timestamp: past.updatedAt, ...oc(`${past.sessionId}-a`) },
      ];
      return { sessionKey: p.sessionKey, messages: history[`${gateway}|${p.sessionKey}`] ?? replay ?? [], hasMore: false };
    }
    case "sessions.past":
      await new Promise((r) => setTimeout(r, 500)); // the agent reads every folder's transcripts
      return { sessions: pastSessions };
    case "sessions.open": {
      const past = pastSessions.find((s) => s.sessionId === p.sessionId);
      if (!past) throw new Error("No such session.");
      const sessions = claude.sessions as unknown as Array<Record<string, unknown>>;
      if (!sessions.some((r) => r.key === past.key)) {
        sessions.push({ key: past.key, agentId: "bot", kind: "direct", derivedTitle: past.title, updatedAt: past.updatedAt, workspaceDir: past.cwd });
        past.inThreads = true;
      }
      return { key: past.key };
    }
    case "chat.send":
      return streamReply(gateway, String(p.sessionKey), String(p.message));
    case "sessions.list": {
      // Like the runtimes: archived threads only when asked for.
      const want = p.archived === "all" ? null : p.archived === true;
      const rows = roster.gateways.find((g) => g.id === gateway)?.sessions ?? [];
      return { sessions: rows.filter((r) => (!p.agentId || r.agentId === p.agentId) && (want === null || Boolean(r.archived) === want)) };
    }
    case "sessions.create":
      return { ok: true, key: `agent:${p.agentId}:dashboard:${crypto.randomUUID()}` };
    case "sessions.patch": {
      const g = roster.gateways.find((g) => g.id === gateway);
      const flags = Object.fromEntries(Object.entries(p).filter(([k]) => ["label", "pinned", "archived", "unread"].includes(k)));
      if (g) g.sessions = g.sessions.map((r) => (r.key === p.key ? { ...r, ...flags } : r));
      emit("lg:roster", { ...roster, gateways: [...roster.gateways] });
      return { ok: true, key: p.key };
    }
    case "cron.list":
      return {
        jobs:
          gateway === "openclaw" && p.agentId === "main"
            ? [
                { id: "job1", name: "Morning briefing", agentId: "main", enabled: true, schedule: { kind: "cron", expr: "0 8 * * 1-5", tz: "America/Sao_Paulo" }, payload: { kind: "agentTurn", message: "Weather, calendar and anything I asked you to watch." }, sessionTarget: "isolated", lastRunStatus: "ok", nextRunAtMs: now + 14 * 60 * min },
                { id: "job2", name: "Check flight prices", agentId: "main", enabled: false, schedule: { kind: "every", everyMs: 4 * 3_600_000 }, payload: { kind: "agentTurn", message: "Check GRU→NRT prices for March and tell me if anything drops under $900." }, sessionTarget: "isolated", lastRunStatus: "error" },
                { id: "sys1", name: "skill-collection-review-main", displayName: "Skill collection review (main)", declarationKey: "skill-collection-review:main", agentId: "main", enabled: true, schedule: { kind: "every", everyMs: 604_800_000 }, payload: { kind: "systemEvent", text: "" }, sessionTarget: "isolated" },
              ]
            : [],
        total: 3,
      };
    case "cron.runs":
      return { entries: [{ ts: now - 9 * 60 * min, runAtMs: now - 9 * 60 * min, jobId: p.jobId, status: "ok", summary: "Sunny, 24°. 3 meetings today.", sessionKey: "agent:main:cron:job1:run:r1" }], total: 1 };
    case "models.list":
      return { models: [{ id: "claude-opus-5", name: "Claude Opus 5", provider: "anthropic" }, { id: "claude-sonnet-5", name: "Claude Sonnet 5", provider: "anthropic" }, { id: "gpt-5.6-sol", name: "GPT-5.6 Sol", provider: "openai" }] };
    case "agents.files.get":
      return { agentId: p.agentId, workspace: "", file: { name: p.name, content: "# SOUL.md\n", hash: "x" } };
    case "agents.workspace.list": {
      const listing = workspaceList(String(p.path ?? ""));
      // Claude Code and ACP threads each work in their own folder.
      const rows = [...claudeCode.sessions, ...claude.sessions] as Array<{ key: string; workspaceDir?: string }>;
      const folder = rows.find((r) => r.key === p.sessionKey)?.workspaceDir;
      return gateway === claudeCode.id || gateway === claude.id ? { ...listing, workspaceDir: folder ?? "/Users/you/dev/shop-api" } : listing;
    }
    case "agents.workspace.get":
      return workspaceGet(String(p.path));
    case "skills.status":
      return gateway === "acp-claude"
        ? { skills: [
            { name: "review", skillKey: "cmd:review", description: "Review the current diff", source: "command", eligible: true, hint: "[files]" },
            { name: "init", skillKey: "cmd:init", description: "Write a CLAUDE.md for this repo", source: "command", eligible: true },
            { name: "graphify", skillKey: "graphify", description: "Turn anything into a knowledge graph.", source: "personal", eligible: true, filePath: "~/.claude/skills/graphify/SKILL.md" },
          ] }
        : { workspaceDir: "/Users/you/.openclaw/workspace-main", skills: mockSkills };
    case "commands.list":
      return { commands: gateway === "acp-claude" ? mockAcpCommands : mockCommands };
    case "skills.read":
      return { name: p.name, path: `~/.claude/skills/${p.name}/SKILL.md`, content: "---\nname: graphify\n---\n\n# graphify\n\nBuilds a knowledge graph from code and docs.\n" };
    case "tools.effective":
    case "tools.catalog":
      return { groups: mockToolGroups };
    case "plugins.list":
      return { plugins: [
        { id: "slack", name: "Slack", description: "Talk to your guys from Slack.", version: "2026.9.4", installed: true, enabled: true, state: "enabled", kind: ["channel"] },
        { id: "browser", name: "Browser", description: "Managed Chrome for agents.", version: "2026.9.4", installed: true, enabled: true, state: "enabled" },
        { id: "voice-call", name: "Voice calls", installed: true, enabled: false, state: "disabled" },
        { id: "matrix", name: "Matrix", installed: false, state: "not-installed" },
      ] };
    case "mcp.servers.list":
      return { servers: [{ name: "playwright", transport: "stdio", command: "npx", enabled: true, source: "~/.claude.json" }] };
    case "config.get":
      return { hash: "h", parsed: { mcp: { servers: { linear: { url: "https://mcp.linear.app/sse?token=secret", type: "sse" }, files: { command: "/usr/local/bin/mcp-files", env: { KEY: "secret" } } } } } };
    case "browser.request":
      return browserRequest(p);
    default:
      return { ok: true };
  }
}

/** Half a second of silence, for `MEDIA:` audio served by the gateway. */
function silentWav(): string {
  const rate = 16_000;
  const samples = rate / 2;
  const view = new DataView(new ArrayBuffer(44 + samples * 2));
  const ascii = (at: number, s: string) => [...s].forEach((c, i) => view.setUint8(at + i, c.charCodeAt(0)));
  ascii(0, "RIFF");
  view.setUint32(4, 36 + samples * 2, true);
  ascii(8, "WAVEfmt ");
  view.setUint32(16, 16, true);
  view.setUint16(20, 1, true);
  view.setUint16(22, 1, true);
  view.setUint32(24, rate, true);
  view.setUint32(28, rate * 2, true);
  view.setUint16(32, 2, true);
  view.setUint16(34, 16, true);
  ascii(36, "data");
  view.setUint32(40, samples * 2, true);
  const bytes = new Uint8Array(view.buffer);
  let bin = "";
  for (let i = 0; i < bytes.length; i++) bin += String.fromCharCode(bytes[i]);
  return `data:audio/wav;base64,${btoa(bin)}`;
}

export const mockBackend: Backend = {
  async invoke<T>(cmd: string, args?: Record<string, unknown>): Promise<T> {
    switch (cmd) {
      case "gw_statuses":
        return roster.gateways.map((g) => g.status) as T;
      case "get_roster":
      case "refresh_roster":
        return roster as T;
      case "gw_media":
        await new Promise((r) => setTimeout(r, 400)); // a round trip to the gateway
        return { available: true, url: silentWav(), mimeType: "audio/wav", sizeBytes: 16_044 } as T;
      case "gw_call":
        return (await gwCall(String(args?.gateway), String(args?.method), (args?.params ?? {}) as Record<string, unknown>)) as T;
      case "browser_stream_start": {
        const channel = args?.onMessage as { onmessage: (buf: ArrayBuffer) => void };
        const id = ++streamSeq;
        const ready = new TextEncoder().encode(JSON.stringify({ type: "ready", targetId: "t1", url: browserState.url, title: browserState.title }));
        channel.onmessage(new Uint8Array([0, ...ready]).buffer);
        if (!params.has("stills")) streams.set(id, window.setInterval(async () => channel.onmessage(await mockFrame()), 500));
        return { id, targetId: "t1", url: browserState.url } as T;
      }
      case "browser_screenshot":
        await new Promise((r) => setTimeout(r, 300));
        return { src: mockPage("Mock screenshot").toDataURL("image/jpeg", 0.7), targetId: "t1", url: browserState.url } as T;
      case "browser_stream_stop":
        clearInterval(streams.get(Number(args?.id)));
        streams.delete(Number(args?.id));
        return undefined as T;
      case "doctor_check":
        return doctor as T;
      case "set_guy_hidden": {
        const uid = String(args?.uid);
        roster.hidden = args?.hidden ? [...new Set([...roster.hidden, uid])] : roster.hidden.filter((u) => u !== uid);
        emit("lg:roster", { ...roster, hidden: [...roster.hidden] });
        return undefined as T;
      }
      case "acp_registry":
        await new Promise((r) => setTimeout(r, 500));
        return [
          { id: "claude-acp", name: "Claude Code", description: "Anthropic's coding agent", version: "0.85.1", command: "npx", args: ["-y", "@agentclientprotocol/claude-agent-acp"], env: {} },
          { id: "codex-acp", name: "Codex", description: "OpenAI's coding agent", version: "0.41.0", command: "npx", args: ["-y", "@agentclientprotocol/codex-acp"], env: {} },
          { id: "gemini", name: "Gemini CLI", description: "Google's agent in your terminal", version: "0.9.0", command: "npx", args: ["-y", "@google/gemini-cli", "--acp"], env: {} },
          { id: "goose", name: "Goose", description: "Open-source agent from the AAIF", version: "1.52.0", command: "goose", args: ["acp"], env: {} },
        ] as T;
      case "hermes_detect":
        await new Promise((r) => setTimeout(r, 600));
        return [{ url: "http://127.0.0.1:9119", version: "0.21.5", mode: "local", authRequired: false }] as T;
      case "claude_code_detect":
        await new Promise((r) => setTimeout(r, 400));
        return { installed: true, version: "2.1.296", plugin: null } as T;
      case "claude_code_install_plugin":
        await new Promise((r) => setTimeout(r, 900));
        return undefined as T;
      case "claude_code_configure": {
        const g = roster.gateways.find((g) => g.id === args?.id);
        const approvals = Boolean((args?.claude as { approvals?: boolean })?.approvals);
        if (g) g.status = { ...g.status, hello: { approvals }, capabilities: { ...g.status.capabilities, approvals } };
        emit("lg:roster", { ...roster, gateways: [...roster.gateways] });
        return undefined as T;
      }
      case "bot_add": {
        const kind = String(args?.kind);
        if (kind === "claudecode") {
          if (!roster.gateways.includes(claudeCode)) roster.gateways.push(claudeCode);
          emit("lg:roster", { ...roster, gateways: [...roster.gateways] });
          return { id: claudeCode.id, name: "Claude Code", url: null, enabled: true, addedAtMs: Date.now(), kind } as T;
        }
        const acp = args?.acp as { agentName?: string } | null;
        const name = (args?.name as string) || acp?.agentName || "Hermes";
        const id = `${kind}-${Date.now()}`;
        const g = gateway(id, name, kind === "acp" ? "stdio" : "http://127.0.0.1:9119", "connecting", {});
        g.status = { ...g.status, kind: kind as "acp" | "hermes" };
        roster.gateways.push(g);
        emit("lg:roster", { ...roster, gateways: [...roster.gateways] });
        setTimeout(() => {
          g.status = { ...g.status, phase: "connected" };
          g.loaded = true;
          g.agents = [{ id: "bot", name, identity: { name, emoji: "🤖" } }] as never;
          emit("lg:roster", { ...roster, gateways: [...roster.gateways] });
        }, 1200);
        return { id, name, url: null, enabled: true, addedAtMs: Date.now(), kind } as T;
      }
      case "gateway_remove": {
        roster.gateways = roster.gateways.filter((g) => g.id !== args?.id);
        for (const d of discovered) if (d.addedAs === args?.id) d.addedAs = null;
        emit("lg:roster", { ...roster, gateways: [...roster.gateways] });
        return undefined as T;
      }
      case "gateways_discover":
        await new Promise((r) => setTimeout(r, 900));
        return discovered as T;
      case "gateway_add": {
        const url = String(args?.url);
        const name = (args?.name as string) || url.replace(/^wss?:\/\//, "").split(".")[0];
        const id = `gw-${name}`;
        const g = gateway(id, name, url, "connecting", {});
        roster.gateways.push(g);
        emit("lg:roster", { ...roster });
        setTimeout(() => {
          g.status = status(id, name, url, "pairing");
          emit("lg:roster", { ...roster, gateways: [...roster.gateways] });
        }, 1200);
        return { id, name, url, enabled: true, addedAtMs: Date.now() } as T;
      }
      case "update_state":
        return { current: "0.1.0", phase: "idle", version: null, error: null, checkedAt: null } as T;
      case "update_check": {
        const ready = { current: "0.1.0", phase: "ready", version: "0.1.1", error: null, checkedAt: Date.now() };
        await new Promise((r) => setTimeout(r, 800));
        emit("lg:update", ready);
        return ready as T;
      }
      default:
        return undefined as T;
    }
  },
  async listen<T>(event: string, cb: (payload: T) => void) {
    const set = listeners.get(event) ?? new Set();
    set.add(cb as (p: unknown) => void);
    listeners.set(event, set);
    return () => set.delete(cb as (p: unknown) => void);
  },
};
