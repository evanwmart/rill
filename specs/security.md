# Rill Security — Identity & Authorization Working Doc

Status: **draft / working doc**. Covers Communication Phase 3: TLS transport,
device identity, and deny-by-default authorization. Builds on
`specs/protocol.md` (bytes) and `specs/connection.md` (sessions).

Structural decisions are recorded in §10.

---

## 1. Threat model (what this phase defends against)

* Passive and active network attackers: all traffic is TLS 1.3; nothing about
  requests, resources, or even frame types is visible on the wire.
* Unauthorized clients: private resources require an enrolled device; everyone
  else sees `NOT_FOUND`, indistinguishable from absence.
* Server impersonation: clients verify the server's identity and refuse to
  send anything to an unrecognized server.

Out of scope for this phase: compromised endpoints, traffic analysis
(sizes/timing), denial of service beyond the existing connection caps.

---

## 2. Identity model

```text
Anonymous       no client certificate presented
Known device    certificate fingerprint present in the server's registry
Unknown device  well-formed certificate, fingerprint not in the registry
Invalid cert    malformed/unparseable certificate → handshake rejected
```

* A **device identity** is a name (`desktop`, `laptop`, `phone`,
  `friend-laptop`) bound to the SHA-256 fingerprint of a self-signed
  certificate whose private key never leaves that device.
* **Unknown devices are treated as anonymous for authorization**, and the
  server records their fingerprint in `pending.toml` beside the registry.
  `rill auth pending <server-dir>` lists them; `rill auth enroll` approves
  one and clears its entry. Recording grants nothing — approval stays a
  human editing the registry — so this is a convenience, not a control,
  and a failure to write it must never cost the connection.
  The list is capped (attacker-influenced by construction: anyone reaching
  the port can add an entry) and a repeat sighting is counted rather than
  rewritten, so a client retrying in a loop is not a file write per attempt.
  A log line still names the fingerprint too, but the file is the workflow:
  it survives the log being off, stderr going nowhere, and a restart.
* There is no CA in this phase: trust is a flat fingerprint registry on the
  server, and a pinned server fingerprint on each client. Enrollment is
  editing a file; revocation is deleting a line. The server's device lookup
  sits behind a small `DeviceAuth` trait so a CA-based verifier can slot in
  later without touching the dispatch path (§10 decision 1).

---

## 3. Transport

```text
TCP → TLS 1.3 (rustls) → Rill frames
```

* TLS 1.3 only. ALPN is required and negotiates exactly `rill/1` — this is
  the protocol version negotiation from protocol.md §2.
* Server presents its self-signed certificate; client verifies it by exact
  SHA-256 fingerprint match against its pinned value. No WebPKI, no hostname
  verification (the fingerprint is stronger and names nothing).
* Server **requests but does not require** a client certificate: absence →
  Anonymous; presence → fingerprint lookup after the handshake.
* Certificates are long-lived (10 years) and expiry is not enforced —
  fingerprint pinning is the trust decision; validity windows add rotation
  burden without adding security in a pinned model.
* There is **no plaintext mode** once this phase lands. Tests generate real
  certificates; `rill://` always means TLS.
* TLS session resumption stays on; 0-RTT stays off (connection.md).

---

## 4. Key material and file layout

Server identity directory (`rill-server serve <root> --identity <dir>`):

```text
<dir>/
├── server-key.pem       private key (0600)
├── server-cert.pem      self-signed certificate
├── devices.toml         device registry
└── policy.toml          authorization policy
```

Client identity directory (`[default ~/.config/rill]`, `--identity` to
override):

```text
~/.config/rill/
├── device-key.pem       this device's private key (0600)
├── device-cert.pem      this device's certificate
└── servers.toml         pinned server fingerprints
```

`devices.toml`:

```toml
# name = "sha256 fingerprint of the device certificate (hex)"
desktop = "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08"
laptop  = "60303ae22b998861bce3b28f33eec1be758a213c86c93c076dbe9f558c11c752"
```

`servers.toml`:

```toml
# "host:port" = "sha256 fingerprint of the server certificate (hex)"
"files.example.net:7331" = "fd61a03af4f77d870fc21e05e7e80678095c92d808cfb3b5c279ee04c74aca13"
```

---

## 5. Enrollment flows

Device setup (on each client machine):

```bash
rill auth init                 # generate device-key.pem + device-cert.pem
rill auth fingerprint          # print this device's fingerprint
```

Server setup:

```bash
rill auth init-server <dir>    # generate server key + cert + empty registry/policy
```

Approving a device: run `rill auth fingerprint` on the device (or read the
server's "unknown device" log line), then add one line to `devices.toml`.

Trusting a server from a client:

```bash
rill auth trust rill://files.example.net:7331
# connects, prints the server's fingerprint, asks for confirmation, pins it
```

Trust-on-first-use with explicit confirmation; after pinning, any fingerprint
change is a hard connection failure until re-trusted.

---

## 6. Authorization policy

`policy.toml`, exactly the plan's shape:

```toml
default_access = "deny"

[[rule]]
path = "/public/**"
allow = ["anonymous"]

[[rule]]
path = "/private/**"
allow = ["desktop", "laptop"]
allow_actions = ["desktop"]     # optional: who may write here
```

### Semantics

* `default_access = "deny"` is the only accepted value (the key exists so the
  file states its own posture; anything else is a startup error).
* `allow` entries are device names, or the special token `"anonymous"`.
  **`"anonymous"` grants access to every connection** — anonymous, unknown,
  and enrolled devices alike (public means public; enrolling a device must
  never *reduce* what it can see).
* A device name grants access to exactly that enrolled device.
* Rule order: **first matching rule wins**; if no rule matches, access is
  denied.
* Denied and unmatched are both answered `NOT_FOUND` (hidden), per
  protocol.md §8.

### Verbs: reading and acting

A request asks for one of two things, and the policy can answer them
separately:

```text
Read   GET, HEAD, GET_IF
Act    ACTION
```

* `allow` answers **Read**.
* `allow_actions` answers **Act** when present. When **absent**, `allow`
  answers both — so every policy written before this key existed keeps
  meaning exactly what it meant.
* An empty `allow_actions = []` is a real statement ("readable here, nobody
  writes") where an empty `allow` is not (that is a startup error: omit the
  rule instead, since the default is deny).
* The rule that matches the path answers for both verbs. A verb miss does
  **not** fall through to a later rule — "first matching rule wins" is what
  makes a policy file readable top to bottom.
* An Act denial is `NOT_FOUND`, identical to a nonexistent endpoint. A
  principal that may read a resource must not be able to enumerate the
  actions it may not invoke.

The verb pair is deliberately closed. Reading and mutating are the two things
the protocol can express; a policy language that grows a verb per feature
stops being auditable, which is the property that matters more here than
expressiveness. Field-level and parameter-level authorization are **not** in
this model: an action's fields are opaque to the policy, and a handler that
accepts a reference as a parameter is responsible for authorizing that
reference itself until schemas can declare which fields are references.

### Path patterns

Matched against the request path (already validated by the codec; no
normalization happens here). The pattern language is deliberately minimal and
hand-implemented — its complete semantics are the three lines below, so the
matcher is exhaustively testable and no external glob library's behavior
becomes part of this security spec:

```text
/literal/segments       exact match
*                       exactly one segment  (/public/*  ≠ /public/a/b)
**                      any remaining suffix, including empty
                        (only valid as the final segment)
```

* Patterns failing these rules are startup errors, not silent non-matches.
* At startup the server lints the policy: a rule that can never match
  (shadowed by an earlier rule) is a warning.

### Check point

Per connection.md §7, authorization runs **after** request decode and
**before** resource resolution — a denied request never touches the
filesystem, and never reaches a handler:

```text
decode → ID check → AUTHORIZE(identity, verb, path) → resolve → stream
```

The verb is `Read` for GET/HEAD/GET_IF and `Act` for ACTION. Note what this
ordering buys for conditional actions in particular: a caller who may not act
is refused before the server reads the resource its condition names, so a
denied conditional action cannot be used to probe whether a revision matches.

---

## 7. Connection context

After the TLS handshake, the server derives one immutable value per
connection:

```rust
enum Identity {
    Anonymous,          // no cert, or unknown fingerprint (logged)
    Device(String),     // enrolled name from devices.toml
}
```

Every authorization decision on the connection uses this value; there is no
per-request re-authentication and no way to upgrade identity mid-connection
(reconnect to present a certificate).

---

## 8. Verification matrix (from the plan, § Communication Phase 3)

```text
Anonymous → /public/…                 allowed
Anonymous → /private/…                NOT_FOUND (hidden)
Enrolled device → /private/…          allowed
Unknown device → /private/…           NOT_FOUND (hidden) + fingerprint logged
Malformed client certificate         TLS handshake rejected
Server fingerprint ≠ pinned          client refuses to proceed
No ALPN agreement                     handshake rejected
```

Exit condition: one approved device securely retrieves a private file over an
untrusted network.

---

## 9. Crate responsibilities

```text
rill-auth (crate)
    Identity type; fingerprints; devices.toml / policy.toml / servers.toml
    parsing; pattern matching; the authorize() decision; rustls config
    builders (server: optional-client-cert verifier; client:
    pinned-fingerprint verifier); certificate generation (rcgen).

rill-server
    --identity <dir>; wraps accepted TCP in TLS; derives Identity;
    calls authorize() in the dispatch pipeline.

rill-client
    device cert loading; pinned server verification; TLS connect with
    ALPN rill/1.

rill (CLI)
    rill auth init | init-server | fingerprint | enroll | trust
```

Dependency policy: rill-auth carries rustls/tokio-rustls/rcgen/sha2/toml.
rill-protocol stays zero-dep; rill-wire stays tokio-only (TLS streams are
just AsyncRead + AsyncWrite to it).

---

## 10. Decisions (resolved 2026-08)

1. **Trust model: fingerprint registry now, CA-ready seam.** Flat registry as
   specced; the server consumes device identity through the `DeviceAuth`
   trait so a CA verifier can replace the registry later without touching
   dispatch or policy.
2. **Rule semantics: first-match-wins**, with a startup lint warning on
   unreachable (shadowed) rules.
3. **Pattern matching: hand-rolled minimal language** (literal / `*` / final
   `**` only); anything else is a startup error.
4. **Auth CLI: `rill auth` subcommands** in the main binary — one tool per
   device, managing the same identity directory `rill get` uses.

---

## 11. Isolation follows exposure (recorded 2026-08-19)

Direction, not yet built; the design question it answers came from the
sharing scenario: one server process hosts three apps — one sensitive,
two regular — and one regular app is granted to a less-trusted identity.

**The policy layer already contains this scenario completely.**
Authorization is per-device-identity, deny-by-default, checked before
dispatch, and denials read as NOT_FOUND — the guest granted app A cannot
discover that app S exists, let alone reach it. Sharing A widens A's
audience and nothing else's.

**What policy cannot see is co-residency.** All handlers share one
address space — and with it every app's in-memory data and the server's
identity key. The guest's only path is protocol frames into a fuzzed
codec and then A's handler logic; Rust removes most of the memory-safety
class. But if A's handler *is* compromised (logic bug, unsafe block, a
dependency), the prize is the process, not A. Sharing an app raises the
attacker value of its handler while in-process hosting keeps its blast
radius at maximum.

**The rule:** isolation follows **exposure**, not only provenance.
Handlers sort on two axes — who wrote them, and who can reach them:

* First-party handler, reachable only by the owner's devices → shared
  process, forever. Cheap and fine.
* Third-party / Tier-1 WASM handler → isolated regardless of audience
  (provenance axis, already the compute-apps position).
* **Any handler granted to a less-trusted identity → the grant itself
  promotes it out of the shared process** (own process, own user,
  minimal fds, no key access, talking over the AppHandler-shaped IPC
  seam). A compromised shared app then yields that app's data and
  nothing else.

One line: **when an app's audience widens, its blast radius should
shrink** — and the trigger is the sharing rule, mechanical, not
judgment. This refines (does not replace) the process-per-app hardening
recorded in docs/bare-metal-plan.md §"On isolated": that section says
isolation effort belongs server-side; this section says *when* to spend
it.

Adjacent items the scenario surfaced, nearest first:

1. **Per-identity rate limits (QoS).** Read budgets bound one connection;
   nothing yet stops a guest hammering their granted app from degrading
   the co-resident sensitive app. A guest should be able to degrade at
   most the app they were granted.
2. **Key custody.** The server identity key lives in the handler
   process — fine while all handlers are first-party, wrong eventually.
   Later hardening: key behind its own minimal process or kernel
   keyring, so a full handler compromise cannot impersonate the server.
3. **Timing side-channels (note-and-move-on class).** Shared memoization
   makes cache-warm responses faster — in principle an oracle about what
   other apps have touched. Recorded for the threat model; not worth
   engineering yet (traffic analysis is already out of scope in §1).
