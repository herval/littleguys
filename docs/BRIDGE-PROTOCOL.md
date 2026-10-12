# littleguys bridge protocol, v1

How littleguys for iPhone talks to littleguys for Mac. The Mac passes every bot it runs (OpenClaw, Hermes, ACP, Claude Code) to a paired phone, one to one, over one end-to-end encrypted session.

- **Mac side:** `src-tauri/src/bridge/`.
- **Phone side:** `Packages/LittleguysKit/Sources/LittleguysKit/Bridge/` in `littleguys-ios`.
- **Why it looks this way:** `littleguys-ios/MAC-RELAY-PLAN.md`.

## Encodings

- **Binary values** in JSON and in the pairing link (keys, secrets, proofs) are unpadded base64url.
- **`macId`** is `hex(SHA-256(id key)[0..16])`: 32 lowercase hex characters.
- **JSON** is UTF-8. Unknown fields are ignored on both sides.

## Keys

| Key | Who | Type | Used for |
|---|---|---|---|
| id key | Mac | Ed25519 | the Mac's identity; the push sender key (`littleguys-relay`) |
| Noise key | Mac | X25519 | the responder static key of every session |
| bridge key | phone | X25519 | the initiator static key; the phone's identity on the Mac |

The phone's bridge key is separate from its OpenClaw device identity and from its push keys.

## Pairing link

`Settings → Phones → Pair mobile app` on the Mac shows this as a QR code (and offers Copy link):

```
littleguys://pair?v=1&name=<url-encoded>&id=<id key>&nk=<Noise key>&addr=<host:port,…>&relay=<url>&secret=<32 bytes>
```

| Param | |
|---|---|
| `v` | `1` |
| `name` | the Mac's name, shown on the phone until `hello` says otherwise |
| `id`, `nk` | the Mac's public keys |
| `addr` | comma-separated `host:port`. Order: private LAN IPv4 addresses, then the tailnet MagicDNS name, then the tailnet IPv4 address. IPv6 goes in brackets. |
| `relay` | the push relay's URL; omitted when push isn't set up |
| `secret` | 32 random bytes. Single use, valid for 10 minutes. |

Phones ignore parameters they don't know.

## Transport

- **A WebSocket at `ws://<host:port>/v1/ws`.** The Mac answers anything else with a 404.
- **Binary messages only.** Each one is exactly one Noise message, at most 65,535 bytes.
- **Default port 47100.** If it's taken, the Mac uses any free port. Either way, the port in use goes in the link and in Bonjour.
- **Bonjour:**
  - the Mac advertises `_littleguys._tcp` with instance name `littleguys-<first 6 hex of macId>`;
  - TXT is `v=1` and `k=<macId>`.

## Session: Noise

`Noise_IK_25519_ChaChaPoly_SHA256`, phone = initiator, Mac = responder. The phone learns the Mac's Noise key from the pairing link. The prologue is the UTF-8 bytes of `"lg-bridge-v1\n" + macId`.

### Handshake message 1 (phone → Mac)

The payload is JSON:

```json
{ "v": 1,
  "client": { "name": "Herval's iPhone", "model": "iPhone17,1", "version": "0.1.0", "platform": "ios" },
  "pair": { "proof": "<base64url>" } }
```

- `pair` is present only when pairing.
- `proof` is HMAC-SHA256 with **key** = the 32 secret bytes and **message** = the UTF-8 of `"lg-bridge-pair-v1\n" + <phone bridge key, base64url> + "\n" + <Mac Noise key, base64url>`.

If the Mac can't read message 1 (a wrong Noise key, or garbage), it closes the WebSocket with code **4001**.

### Handshake message 2 (Mac → phone)

The Mac now knows the phone's static key.

- **Accepted** (a paired phone, or a valid `proof` for an open pairing): the payload is `hello`.

  ```json
  { "protocol": 1,
    "mac": { "name": "Herval's MacBook Pro", "version": "0.2.0", "id": "<id key>" },
    "bots": [ BotStatus, … ],
    "policy": { "maxPayload": 33554432, "tickIntervalMs": 15000, "maxMediaBytes": 104857600 },
    "push": { "relay": "https://…", "registered": false } }
  ```

  - `push` is omitted when push isn't set up.
  - A handshake message can't be split. If `hello` wouldn't fit (more than 65,487 bytes), the Mac sends it with `"bots": []` and the full list as a `bots` event right after. Phones should take the list from whichever comes last.

- **Rejected:** the payload is `{"error": {"code", "message", "retryAfterMs"?}}`. The Mac then closes with code **4003**.

| `error.code` | When |
|---|---|
| `NOT_PAIRED` | unknown phone key and no `pair` |
| `BAD_PROOF` | `pair.proof` matches no open pairing secret |
| `PAIRING_EXPIRED` | the matching secret is older than 10 minutes, or already used |
| `PROTOCOL_MISMATCH` | `v` isn't 1 |
| `RATE_LIMITED` | more than 10 failed handshakes in a minute from that IP (carries `retryAfterMs`) |

The phone treats `PROTOCOL_MISMATCH` and `RATE_LIMITED` as retryable later; the others need the person.

### Committing a pairing

A pairing is committed (the phone's key stored, the secret spent) only when the **first valid transport message** from the phone arrives. A replayed message 1 can't pair anything, because the replayer can't finish the handshake.

The phone's first transport message is `req presence`.

### Transport records

After the handshake, each Noise message's plaintext is one record:

```
[kind u8][flags u8][body …]        body ≤ 65,517 bytes (65,535 − 16-byte tag − 2)
```

| kind | body | flags |
|---|---|---|
| `0` JSON frame | a chunk of one UTF-8 JSON frame | bit 0: more chunks follow. The receiver joins chunks until one has bit 0 clear. |
| `1` stream data | `[u32 big-endian streamId][bytes]` | bit 0: end of stream. Bit 1: aborted (the bytes are a UTF-8 reason). |

- **Frame size:** a joined JSON frame is at most `policy.maxPayload` bytes, and a bigger one is a protocol error (the receiver closes). JSON chunks and stream records may interleave: a sender finishes one JSON frame's chunks before starting another, but stream records can go in between.
- **Close codes:**
  - **4000:** a protocol error;
  - **4008:** too slow (the event queue is full);
  - **4010:** this phone was removed on the Mac;
  - **1000:** a normal close.

## Frames

OpenClaw-shaped:

```
{"type":"req",   "id": "<string>", "method": "<string>", "params": {…}}
{"type":"res",   "id": "<string>", "ok": true,  "payload": …}
{"type":"res",   "id": "<string>", "ok": false, "error": {"code", "message", "details"?}}
{"type":"event", "event": "<string>", "payload": …, "seq": <int, per session, from 1>}
```

Only the phone sends `req`. Only the Mac sends `event`.

## Methods

| Method | Params | Result |
|---|---|---|
| `presence` | `{foreground: bool, focused?: {gateway, sessionKey}}` | `{}` |
| `bots.list` | `{}` | `{bots: [BotStatus]}` |
| `bot.call` | `{gateway, method, params?, timeoutMs?}` | the runtime's result. The runtime's error comes back as the `res` error, as-is. |
| `bot.reconnect` | `{gateway}` | `{}` |
| `bot.media` | `{gateway, source, sessionKey?, agentId?}` | `{available: true, stream, mimeType?, sizeBytes?}` or `{available: false, code?, reason?}` |
| `phone.forget` | `{}` | `{}`. The Mac drops this phone, then closes with 4010. |
| `push.register` | `{registrationId, relayKey, boxKey, environment}` | `{}`, or `NOT_REGISTERED` / `NO_RELAY` ([Push](#push)) |
| `push.unregister` | `{}` | `{}` |

**`bot.call`:**
- An unknown `gateway` → `UNKNOWN_GATEWAY`.
- The Mac's default timeout is the runtime's own. `timeoutMs` overrides it, capped at 10 minutes.

**`bot.media`:**
- For OpenClaw bots, the Mac does what `gw_media` does (an authorized `meta=1` request, then the ticketed URL). Instead of returning the URL, it downloads the bytes and sends them as stream records on `stream`.
- Other bots answer `{available: false, code: "UNSUPPORTED"}`.
- **Limits:** files over `policy.maxMediaBytes` → `{available: false, code: "TOO_LARGE"}`. At most 4 streams per session at a time; more → `BUSY`.

## Events

| Event | Payload |
|---|---|
| `bots` | `{bots: [BotStatus]}`, whenever the list changes (added, removed, renamed, turned on or off) |
| `bot.status` | `{status: BotStatus}`, whenever one bot's connection status changes |
| `bot.event` | `{gateway, event, payload}`, every runtime event (`chat`, `session.message`, `agent`, `sessions.changed`, `exec.approval.requested`, `exec.approval.resolved`, `cron`, …) |
| `tick` | `{ts}` (unix ms), every `policy.tickIntervalMs` |
| `phone.removed` | `{}`, just before closing with 4010 |

**Back-pressure:** each session queues at most 2,000 outgoing events. When full, the Mac closes with 4008, and the phone reconnects and refetches.

**Watchdog:** the phone reconnects after `3 × tickIntervalMs` without any record.

## BotStatus

The Mac's `ConnStatus` (camelCase, as the webview gets it in `gw:status`), with Mac-only fields removed and two added:

```json
{ "gatewayId": "hermes-9f8e7d6c5b", "gatewayName": "Hermes", "kind": "hermes", "kindLabel": "Hermes",
  "capabilities": { "manageGuys": false, "routines": true, … },
  "phase": "connected", "url": "http://127.0.0.1:9119", "message": null, "detailCode": null,
  "requestId": null, "serverVersion": "0.9.1", "scopes": [], "retryAtMs": null,
  "hello": { "policy": {…}, "features": {…} },
  "endpoint": "http://127.0.0.1:9119" }
```

- **Removed:** `configPath`, `configVersion`, `deviceId`, `credentialIssue`, `mode`.
- **`hello`:** only its `policy` and `features` (or `null`).
- **`kind`:** `openclaw | hermes | acp | claudecode | …`. Phones treat kinds they don't know as generic bots and show `kindLabel`.
- **`phase`:** `starting | connecting | connected | pairing | auth-failed | offline | no-config | error`.
- **`endpoint`:** the normalized URL the bot connects to, or `null` for local agents. Phones use it to skip a relayed bot they also reach directly.

## Push

Pushes go through `littleguys-relay` (`littleguys-relay/PROTOCOL.md`: signed requests, HPKE, message format). Pairing for push happens over this session, not with the relay's `meta.proof` matching.

**Keys and URLs:**
- The Mac's **sender key is its id key**, so a phone that paired already knows it.
- The relay URL is `https://littleguys-relay.vercel.app` by default. It's in the pairing link (`relay`) and in `hello.push.relay`.

**The phone:**
1. Makes a relay key (Ed25519) and a box key (X25519), one pair per phone.
2. Calls `POST /v1/registrations` with these fields:
   - `sender`: the Mac's id key;
   - `token`: its APNs token;
   - `topic`: its bundle id;
   - `environment`;
   - `meta`: `{"v":1,"name":"<phone name>"}`.
3. Sends `push.register {registrationId, relayKey, boxKey, environment}` over the session.
4. Does it again on launch, whenever the APNs token changes, and whenever `hello.push.registered` is false.

**The Mac**, on `push.register`:
- It checks with `GET /v1/registrations` (signed with its id key) that a registration with that `id` exists and its `owner` is `relayKey`. If not, it answers `NOT_REGISTERED`. With no relay configured, it answers `NO_RELAY`.
- Then it stores `{registrationId, relayKey, boxKey, environment}` on the phone's record, and `hello.push.registered` becomes true.
- `push.unregister` and `phone.forget` drop it. A `410 gone` from the relay also drops it.

**What the Mac pushes**, to every registered phone that isn't present in the foreground (the last `presence` said `foreground: true` on a live session):

| When (`activity.rs`) | `type` | message `kind` |
|---|---|---|
| a reply finished (`notify_reply_later`) | `alert` | `reply` |
| a routine finished (`notify_routine`) | `alert` | `routine` |
| an approval was requested (`notify_approval`) | `alert` | `approval` |
| an approval was resolved | `background` | `approval.resolved` |

- **The message** is as in the relay protocol: `{kind, title, body, gateway, sessionKey?, approvalId?, ts}`.
  - `gateway` is the **Mac's** gateway id; the phone maps it to its own.
  - `title` and `body` are what the Mac would show in its own notification.
- **Sealing:**
  - The Mac signs `"lg-msg-v1\n" + registrationId + "\n" + <message bytes>` with its id key.
  - It seals `<base64url signature>.<message bytes>` with HPKE base mode to the phone's box key: `DHKEM(X25519, HKDF-SHA256)`, `HKDF-SHA256`, `ChaCha20-Poly1305`, with `info = "lg-push-v1"` and `aad = registrationId`.
  - `ciphertext` is base64url(encapsulated key ‖ sealed box).
- **Size:** if `ciphertext` would pass 3,000 characters, the Mac shortens `body` (ending it with `…`) until it fits.
- **Grouping:** `collapseId` is the first 16 hex characters of SHA-256(`gateway|sessionKey`) for replies and routines (when they have a thread), and of SHA-256(`approvalId`) for approvals. `approval.resolved` has none.
- **Expiry:** `ttl` is 1 hour for replies and routines, 10 minutes for approvals, and 0 for `approval.resolved`.

## Rate limits and safety

- **Before the handshake:** at most 10 failed handshakes a minute per source IP. A socket that hasn't completed the handshake within 10 s is closed.
- **Nothing listens** while there's no paired phone and no open pairing.
- **Pairing secrets** live only in memory.
- **The Mac stores each phone** in `phones.json` (0600) in its data dir: key, name, model, `pairedAtMs`, `lastSeenAtMs`, and its push registration.
