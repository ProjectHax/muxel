# muxel Cloud sync protocol (v1)

muxel Cloud syncs a user's run tasks, loops, snippets, SSH hosts and appearance
between their devices, and shares tasks, settings and activity within a team.
The client side is part of muxel (GPL-3.0). The service runs at
`https://cloud.muxel.sh`.

**The server only ever stores ciphertext.** Encryption, signing and key
distribution happen in muxel. The server routes opaque envelopes and enforces
access control using a small set of plaintext metadata (§4.3). This document is
the contract between muxel and the service. A third-party server that
implements it can be used by pointing muxel at it (`MUXEL_CLOUD_URL`).

## 1. Transport and authentication

- **Transport.** JSON over HTTPS. The base URL is `https://cloud.muxel.sh/api/v1`.
- **Binary fields.** Byte strings use standard base64 with padding (RFC 4648 §4).
- **Ids.** All ids are UUID strings.
- **Times.** Times are unix milliseconds (`u64`).
- **Authentication.** Every call except the device-login pair takes `Authorization: Bearer <token>`. A token is one of two kinds:
  - `mxd_…` is a **device token** from the browser device login (§2);
  - `mxk_…` is an **API key** created in the web panel.

  Both kinds name a *device*. The server stores only a SHA-256 hash of each token.
- **Errors** use this shape: `{"error": "<code>", "detail": "<human text>"}`.

  | Status | Error code | Meaning |
  |---|---|---|
  | 401 | `unauthorized` | The token is missing, revoked or expired. |
  | 402 | `payment_required` | No plan covers this action. |
  | 403 | `forbidden` | The caller's role or sync policy doesn't allow it. |
  | 403 | `mfa_required` | The team requires 2FA and the caller hasn't turned it on (§5). |
  | 404 | `not_found` | |
  | 409 | `version_conflict` | The item changed since the client last read it (§4.2). |
  | 413 | `too_large` | The item is over 256 KiB, an activity envelope is over 32 KiB, or the storage quota is full. |
  | 423 | `read_only` | Grace period after a failed payment: reads work, writes don't. |
  | 429 | `rate_limited` | A `Retry-After` header says when to retry. |

## 2. Device login (modelled on RFC 8628)

```
POST /device/start   {"device_name": "laptop", "platform": "linux"}
→ 200 {"device_code": "…", "user_code": "WDJB-MJHT",
       "verification_uri": "https://cloud.muxel.sh/device",
       "verification_uri_complete": "https://cloud.muxel.sh/device?code=WDJB-MJHT",
       "interval": 5, "expires_in": 900}

POST /device/token   {"device_code": "…"}
→ 400 {"error": "authorization_pending" | "slow_down" | "expired_token" | "access_denied"}
→ 200 {"access_token": "mxd_…", "device_id": "<uuid>"}
```

1. muxel opens `verification_uri_complete` in the browser.
2. The user signs in to the web panel there, including 2FA, and approves the device.
3. muxel polls `/device/token` every `interval` seconds. If it receives `slow_down`, it adds 5 seconds to the interval.

Devices are listed in the web panel and can be revoked there. A revoked token returns 401.

## 3. Keys and cryptography

### Primitives

| Purpose | Algorithm |
|---|---|
| Authenticated encryption | XChaCha20-Poly1305 (24-byte random nonce) |
| Key agreement | X25519 |
| Key derivation | HKDF-SHA256 |
| Signatures | Ed25519 |
| Hashing | SHA-256 |

**Symmetric keys.** Every symmetric key is 32 random bytes from the OS CSPRNG.

**`secretbox(key, aad, plaintext)`** = `nonce(24) ‖ XChaCha20-Poly1305(key, nonce, aad, plaintext)`.

**`seal(recipient_pub, aad, plaintext)`** encrypts to an X25519 public key. Its output is `epk(32) ‖ secretbox(k, aad, plaintext)`, where:
- `epk` is a fresh ephemeral X25519 public key;
- `k = HKDF-SHA256(ikm = X25519(esk, recipient_pub), salt = epk ‖ recipient_pub, info = "muxel-cloud/v1/seal", L = 32)`.

### 3.1 Account keys

Each account has two keypairs, both generated **in muxel**:
- an X25519 **encryption** keypair (`enc`);
- an Ed25519 **signing** keypair (`sign`).

The private halves are kept as `sealed_private = secretbox(AUK, "muxel-cloud/v1/account-private", enc_sk(32) ‖ sign_sk(32))`.

**AUK (Account Unlock Key).** A random 32-byte key that is never sent to the server in the clear.
- On each device it lives in the OS keychain.
- On the server it is stored only as `auk_recovery = secretbox(RK, "muxel-cloud/v1/auk", AUK)`.
- `RK = HKDF-SHA256(ikm = recovery_secret, salt = recovery_salt, info = "muxel-cloud/v1/recovery", L = 32)`.

**Recovery Key.** It consists of `recovery_secret`, 16 random bytes shown to the user once, and `recovery_salt`, 16 random bytes stored on the server.
- It is displayed as `MXL-` followed by the Crockford base32 encoding of the 16 bytes, in groups of 4 separated by `-`.
- When it is typed back in, case, `-` and spaces are ignored, and `O`→`0` and `I`/`L`→`1`.

```
GET  /account/keys   → 200 {"enc_pub", "sign_pub", "sealed_private", "auk_recovery",
                            "recovery_salt", "created_at"}  |  404 (no keys yet)
PUT  /account/keys   {same fields}   → 201 (only when none exist; 409 otherwise)
POST /account/keys/rotate-recovery {"auk_recovery", "recovery_salt"} → 200
```

**Adding a device.** A new device signs in (§2) and then either:
- (a) reads `/account/keys`, the user types the Recovery Key, and the device unseals the AUK; or
- (b) the device generates an ephemeral X25519 key and asks for approval:

  ```
  POST /account/device-approvals            {"device_pub"}  → {"approval_id"}
  GET  /account/device-approvals?pending=1  → [{"approval_id", "device_id", "device_name", "device_pub", "created_at"}]
  POST /account/device-approvals/{id}       {"sealed_auk": seal(device_pub, "muxel-cloud/v1/auk-approval", AUK)}
  GET  /account/device-approvals/{id}       → {"sealed_auk"} | 202 (still pending)
  ```

  An already signed-in device of the same user shows the pending request, including the device name and a fingerprint of `device_pub`. The user confirms it there.

  A device may have at most 3 unapproved requests waiting; a 4th returns 429 `rate_limited` until one expires (15 minutes).

**Fingerprint** of a public key = the first 10 bytes of `SHA-256("muxel-cloud/v1/fp" ‖ enc_pub ‖ sign_pub)`, written as 5 groups of 4 hex digits.

### 3.2 Scopes and scope keys

A **scope** is a key domain. Every user has one `personal` scope. Every team has three scopes:
- `settings`: shared runners, loops, snippets and hosts;
- `tasks`;
- `activity`.

Each scope has a current **generation** `g` and a 32-byte scope key `SK(scope, g)` for each generation. A **grant** gives one generation's key to one member:

```
grant.wrapped_key = seal(member.enc_pub, "muxel-cloud/v1/grant|" ‖ scope_id ‖ "|" ‖ g, SK)
grant.signature   = Ed25519(granter.sign_sk, "muxel-cloud/v1/grant-sig|" ‖ scope_id ‖ "|" ‖ g
                            ‖ "|" ‖ member_user_id ‖ "|" ‖ SHA-256(wrapped_key))
```

Readers verify `grant.signature` before trusting a scope key. A grant must be signed by one of these:
- the member themselves (for the personal scope);
- the team member who created the scope;
- a member whose role carries `members.manage` according to `/teams/{id}/members`.

```
GET  /scopes   → [{"id", "kind": "personal"|"team_settings"|"team_tasks"|"team_activity",
                   "team_id"|null, "generation", "rotation_required": bool,
                   "permissions": ["…"], "collections": ["…"]}]
GET  /keys     → [{"scope_id", "generation", "wrapped_key", "granter_user_id", "signature"}]
POST /scopes   {"id", "kind", "team_id"|null, "grants": [{"user_id","generation":1,"wrapped_key","signature"}, …]}
               → {"id"}   (creates the personal scope, or a team's three scopes the first time an owner's client connects)
GET  /grants/pending → [{"scope_id", "generation", "user_id", "enc_pub", "sign_pub", "fingerprint"}]
POST /grants   {"scope_id", "generation", "user_id", "wrapped_key", "signature"}
POST /scopes/{id}/rotate {"generation": g+1, "grants": [{"user_id","wrapped_key","signature"}, …]}
```

**Creating a scope** is a single atomic `POST /scopes`.
- The client picks the scope's `id`, because grant signatures bind it.
- The body includes the creator's own grant; a create without one is refused with 400.
- The server returns 409 in these cases:
  - a second personal scope for the same user;
  - a second scope of the same kind for the same team;
  - an id that already exists.
- The client that loses a creation race reloads `/scopes` and `/keys`.
- Team scopes may be created only by members holding `members.manage`.
- `POST /grants` exists for fulfilling pending grants. It is accepted from members holding `members.manage` on team scopes.
- A scope needs a plan to be created (402 otherwise). Rotation and `POST /grants` don't, because they keep removed members out.
- Deleting an account never deletes a team's scopes, items or other members' grants. Grants the deleted user had signed are no longer served by `/keys`, because no client could verify them. A manager may replace such a grant with `POST /grants` at the same generation.

**Pending grants.** When a member joins, or their role starts allowing a scope, the server adds a *pending grant*. Any online client whose user holds `members.manage` and has the scope key fulfils it in the background.

**Rotation.** When a member leaves, or their role stops allowing a scope, the server sets `rotation_required`. The next such client creates generation `g+1`, grants it to every remaining entitled member and calls `/rotate`. New writes use the newest generation. Rotation does not revoke data the departed member already saw.

**Re-sealing.** Clients trust a grant only from a *current* member holding `members.manage`, and an item only from a *current* member allowed to write it. They don't keep scope keys between syncs, and pending grants and rotation only hand out the current generation. So an item sealed under an older generation, or written by someone who has since left or lost write permission, stops opening on new devices and for later joiners. To keep team data alive, every client whose user may write a team scope (`settings.write`, or `tasks.update`; deleted tasks also need `tasks.delete`) re-seals such items each sync. It writes its own synced copy again under the current generation, signed by itself, with `base_version` set to the version it last synced. It re-seals only a copy it verified (or wrote) and hasn't changed since, so the server can't get anything else signed this way. A 409 means someone else got there first, and a 403 (for example a sync policy) leaves the item to another writer. Clients record each item's author and generation to decide this. Data no writer's device had verified before its author left can't be recovered: that's the price of an untrusted server. A grant for an older generation from someone no longer trusted is skipped without an error.

### 3.3 Item envelope

Every synced object is an **item** `(scope_id, collection, item_id)`. It is encrypted under its own data key (DEK) and serialized as a JSON object, called the *envelope*, which the server stores as opaque bytes:

```json
{
  "v": 1,
  "gen": 3,
  "wk": "<b64 secretbox(SK(scope,gen), AAD_K, DEK)>",
  "wa": "<b64 seal(recipient.enc_pub, AAD_K, DEK)>   (optional; reserved for handing one item to a single reader)",
  "ct": "<b64 secretbox(DEK, AAD_C, plaintext)>",
  "by": {"user": "<uuid>", "device": "<uuid>"},
  "sig": "<b64 Ed25519(author.sign_sk, SIGMSG)>"
}
```

```
AAD_C  = "muxel-cloud/v1/item|" ‖ scope_id ‖ "|" ‖ collection ‖ "|" ‖ item_id
AAD_K  = AAD_C ‖ "|key|" ‖ gen
SIGMSG = "muxel-cloud/v1/item-sig|" ‖ scope_id ‖ "|" ‖ collection ‖ "|" ‖ item_id ‖ "|" ‖ gen
         ‖ "|" ‖ hex(SHA-256(wk ‖ wa ‖ ct))      (wa = empty when absent; raw bytes, not base64)
```

Binding the AAD to the item's location means the server cannot move a ciphertext to another item or collection. For example, it cannot turn a snippet into a loop.

**Readers check:**
- the signature against the author's `sign_pub`;
- that the author is the scope's owner (personal) or a team member allowed to write that collection;
- for loops and runners, which execute, they reject the item if any check fails.

The **plaintext** is JSON in the form `{"hlc": "<HLC>", "kind": "<collection>", "data": {…}}`. Deleting an item writes `"data": null` and sets `deleted: true` in the request.

**HLC** is a hybrid logical clock: `"<wall_ms:013>-<counter:04x>-<device_id>"`. It compares as a string. A newer HLC wins, and the client never accepts an HLC lower than the newest it has already applied for that item. This rule rejects server rollback.

### 3.4 Activity envelope

Activity is per device and ephemeral. Its form is `secretbox(SK(activity, gen), "muxel-cloud/v1/activity|" ‖ team_id ‖ "|" ‖ user_id ‖ "|" ‖ device_id, json)` plus `gen` and an author signature over the same string ‖ the ciphertext hash, encoded like an item envelope without `wk`/`wa`. The `activity` scope key encrypts it directly.

An activity envelope is at most 32 KiB (decoded); a larger one returns 413 `too_large`.

## 4. Items API

### 4.1 Pulling changes

```
GET /changes?cursor=<int>&limit=<≤500>
→ {"items": [{"scope_id", "collection", "item_id", "version", "seq",
              "author_user_id", "author_device_id", "assignee_user_id"|null,
              "deleted", "envelope": "<b64>", "updated_at"}],
   "cursor": <int>, "more": bool}
```

`seq` is a server-wide monotonic sequence number. A client stores `cursor` and passes it back. Only items the caller may read are returned (§5). Pass `cursor=0` to resync from the start.

**Page size.** A page holds at most `limit` items and about 8 MiB of envelopes (counted decoded, before base64), whichever comes first. A page always holds at least one item when any are left, even if that item alone is larger. `more: true` means items are left: the client keeps asking with the returned `cursor` until `more` is false. A short page with `more: true` is normal; it doesn't mean the end.

**Export.** `GET /changes?cursor=<int>&limit=<≤500>&export=1` is the same read, except it also covers scopes whose plan has lapsed (§6). A client uses it to decrypt everything the account can read into a local file. Writes still need a plan.

### 4.2 Writing an item

```
PUT /scopes/{scope_id}/items/{collection}/{item_id}
    {"base_version": <int, 0 = create>, "envelope": "<b64>", "deleted": bool,
     "assignee_user_id": "<uuid>"|null}
→ 200 {"version", "seq"}
→ 409 {"error": "version_conflict", "current": {item as in /changes}}
```

On 409 the client decrypts `current`, merges by HLC, and retries with the new `base_version`.

**Limits:** 256 KiB per envelope, and a per-account storage quota. The quota counts each item's envelope size plus a fixed per-item overhead (512 bytes), tombstones included, so many tiny items fill it too.

### 4.3 Plaintext metadata the server sees

The server sees only:
- scope id, collection, item id, version and seq;
- author user and device;
- the timestamps;
- the deleted flag;
- the envelope size;
- for `tasks` only, the assignee's user id (§5, the Assignee role).

Nothing else is readable by the server.

### 4.4 Collections

| Scope | Collections |
|---|---|
| personal | `runners`, `loops`, `snippets`, `hosts`, `identities`, `appearance`, `keybindings`, `tasks` |
| team_settings | `runners`, `loops`, `snippets`, `hosts`, `identities` |
| team_tasks | `tasks` |

**Item payloads** (`data`) are muxel's own types, encoded as JSON, with machine-local fields removed:
- **`runners`** and **`loops`**:
  - carry `preset_name` instead of the per-install `preset_id`;
  - loops carry `project_key`, a normalized git remote URL plus subpath, or the project name, instead of `project_id`;
  - loops drop `last_run` and `enabled`. On every device a pulled loop starts switched off.
- **`hosts`** and **`identities`** never contain passwords or key material. `identity_file` is a path hint only.

## 5. Teams and roles

```
GET /teams                       → [{"id", "name", "my_role", "permissions", "seats", "seats_used", "mfa_required"}]
GET /teams/{id}/members          → [{"user_id", "email", "name", "role", "permissions",
                                     "enc_pub", "sign_pub", "fingerprint", "sync_policy"}]
PUT  /teams/{id}/activity        {"envelope"}             (this device's presence)
GET  /teams/{id}/activity        → [{"user_id", "device_id", "envelope", "updated_at"}]
```

An activity envelope is at most 32 KiB (§3.4). `GET /teams/{id}/activity` returns the freshest rows first, up to about 4 MiB of envelopes in total; older rows past that budget are left out.

The panel at `cloud.muxel.sh` handles membership, invites, roles and billing.

**Required 2FA.** When a team's owner requires 2FA, a member without it keeps personal sync but loses the team until they turn 2FA on. `/teams` still lists the team, with `"mfa_required": true`. `/scopes` and `/keys` leave out its scopes. Its team endpoints and items return 403 `mfa_required`.

**Permission atoms:**

| Atom | What it allows |
|---|---|
| `billing` | Billing pages |
| `members.manage` | Manage members and invites |
| `roles.assign` | Assign roles |
| `settings.read` | Read shared settings |
| `settings.write` | Write shared settings |
| `tasks.read_all` | Read every task |
| `tasks.read_assigned` | Read only tasks assigned to you |
| `tasks.create` | Create tasks |
| `tasks.assign` | Assign tasks to anyone |
| `tasks.take` | Take an unassigned task |
| `tasks.update` | Update tasks you can see, including closing them (Done, Cancelled) |
| `tasks.delete` | Delete tasks permanently (write a tombstone) |
| `activity.read` | Read team activity |
| `activity.publish` | Publish your own activity |

**Built-in roles:**

| Role | Atoms |
|---|---|
| owner | all |
| admin | all except `billing` |
| manager | `settings.*`, `tasks.*`, `activity.*` |
| member | `settings.read`, `tasks.read_all`, `tasks.create`, `tasks.take`, `tasks.update`, `activity.*` |
| assignee | `tasks.read_assigned`, `tasks.update` |
| settings_only | `settings.read` |
| observer | `tasks.read_all`, `activity.read` |
| billing | `billing` |

**Enforcement on the server:**
- **Reads.** A `team_tasks` item is returned only with `tasks.read_all`, or with `tasks.read_assigned` when `assignee_user_id` is the caller.
- **Writes** require the matching write atom:
  - `tasks.update` may change only tasks the caller can read;
  - changing `assignee_user_id` to another user needs `tasks.assign`;
  - setting it to yourself on an unassigned task needs `tasks.take`;
  - a tombstone (`deleted: true`) for a team task needs `tasks.delete`. It keeps the task's current `assignee_user_id`, whatever the request says, so an Assignee-role member receives the deletion. Personal tasks can always be deleted.
- **Deleting tasks (clients).** A task is deleted only by an explicit tombstone, never because it's missing locally (a role change or a reassignment also hides one). A client drops a pulled team-task tombstone unless its author's role has `tasks.delete`. A task deleted locally stays deleted until its tombstone is sent, even if a pull returns it meanwhile.
- **Sync policy.** A member's `sync_policy` (a list of allowed collections, set by an admin) narrows `team_settings` reads further.

**Key grants** follow the role:
- `settings` with `settings.read`;
- `tasks` with `tasks.read_all` **or** `tasks.read_assigned`;
- `activity` with `activity.read`.

Assignee-only members hold the tasks key because writing a progress update means wrapping its data key for everyone else who reads tasks. Which task items they *receive* is limited by the server (the read rule above). Against teammates, the filtering is therefore enforced by the server; against the server itself, the end-to-end guarantees are unchanged. Removing `tasks.read_assigned` from a member rotates the tasks scope like any other loss of a scope.

## 6. Account and billing status

```
GET /me → {"user": {"id", "email", "name"}, "device": {"id", "name"},
           "entitlements": {"personal_sync": bool, "read_only": bool,
                            "plan": "none"|"individual"|"team", "interval": "month"|"year"|null},
           "teams": [{"id", "name", "role"}]}
```

`personal_sync` is true when the user has an active Individual plan or a seat on a paid team.

After a failed payment:
- for the first 7 days, service continues unchanged;
- until day 30, `read_only` is true and writes return 423, while reads keep working;
- after day 30, the plan no longer entitles anything, and the stored data is kept until the account is deleted.

**Other error codes:**
- storage quota exceeded: 413 `too_large`;
- other conflicts (keys already uploaded, a duplicate scope, a different grant already held): 409;
- malformed requests: 400 `bad_request`.

Writing an item that doesn't exist with `base_version > 0` returns 409 with `"current": null`. `current` is also `null` when the caller may not read the conflicting item.

**Fingerprints** are lowercase hex in groups of four, separated by single spaces.
