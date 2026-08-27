# Rill Connection — Session Semantics Working Doc

Status: **draft / working doc**. Sits between `specs/protocol.md` (what bytes
mean) and the client/server implementations (what each side does about them).
Covers Communication Phases 1–2; TLS and identity land here in Phase 3 via
`specs/security.md`.

Values marked `[default]` are config knobs with the stated default.
Structural decisions are recorded in §12.

---

## 1. Address form

```text
rill://host[:port]/path
```

* `host`: DNS name, IPv4 literal, or bracketed IPv6 literal (`[::1]`).
* `port`: `[default 7331]`.
* `path`: required for `get`/`head`; must satisfy the protocol path rules
  (spec §7.1) — the CLI does not normalize, it validates and refuses.
* The URL splits at the third `/`: everything after `host[:port]` (including
  that `/`) is the resource path, sent verbatim.

```bash
rill get rill://localhost:7331/example.txt
rill get rill://files.example.net/private/notes/today.txt -o today.txt
```

---

## 2. Connection lifecycle

```text
TCP connect (TCP_NODELAY on)
    ↓
[Phase 3: TLS 1.3 handshake, ALPN "rill/1"]
    ↓
READY ──── requests / responses ────┐
    ↑                               │
    └── PING/PONG while idle ───────┘
    ↓
CLOSE sent → TCP shutdown
```

* **Clean close**: either side sends CLOSE, then shuts down its write half.
  The peer finishes nothing — CLOSE is immediate; a server mid-stream aborts
  the stream (the client discards the incomplete resource).
* **EOF without CLOSE**: abnormal. If a request is in flight → that request
  fails with a transport error. If idle → logged, not an error.
* **EOF mid-frame**: always an error (truncated frame).
* Any codec `FrameError`, wrong-direction frame, or session violation (§4)
  is **connection-fatal**: send ERROR (request ID 0, the mapped
  `wire_status()`), then CLOSE, then shut down.

---

## 3. Request IDs and ordering

* Client assigns IDs starting at 1, incrementing by 1 per request.
* Server enforces: first request ID on a connection is 1; each subsequent ID
  is exactly previous + 1. Violation → connection-fatal.
* **Server MUST respond in request order** (v1). Request IDs still tag every
  response frame; relaxing ordering later is a server-side change only.
* Client request model: **sequential — one request in flight at a time**.
  The wire format permits pipelining; the Phase 1–2 client does not use it.

---

## 4. Request lifecycle

Legal response sequences per request type — anything else is a session
violation (connection-fatal):

```text
GET  → RESOURCE(MORE)* RESOURCE(final)      success
     | ERROR                                 failure

HEAD → METADATA                              success
     | ERROR                                 failure
```

A request is complete when the final RESOURCE, the METADATA, or the ERROR
frame for its ID arrives. ERROR after one or more chunks aborts the request
(client discards partial data).

Client-side caps while streaming:

* total resource size: `[default 32 MiB]` — exceeding it aborts the request
  and closes the connection (the server is misbehaving or hostile). The
  default is sized so that reaching it is survivable on the smallest target:
  a cap the machine cannot reach without being killed first is not a cap. A
  caller who genuinely wants a large transfer raises it for that request;
* chunks MUST NOT exceed MAX_PAYLOAD (codec-enforced).

---

## 5. Keepalive

* Only the **client** pings; the server only pongs.
* Client pings after `[default 30 s]` idle; an unanswered PING after
  `[default 10 s]` closes the connection.
* PING payload: 8 counter bytes; PONG must echo exactly (protocol spec §7.3a).
* The CLI (one process per command) does not keep alive; the client *library*
  does, for future rill-view use.

---

## 6. Timeout and limit matrix

| Side   | Knob                        | Default | Meaning                                    |
|--------|-----------------------------|---------|--------------------------------------------|
| client | connect timeout             | 10 s    | TCP (+ TLS in Phase 3) established         |
| client | first-byte timeout          | 30 s    | request sent → first response header byte  |
| client | inter-chunk timeout         | 30 s    | gap between response frames                |
| client | total resource cap          | 32 MiB  | §4; abort + close on excess                |
| server | idle timeout                | 300 s   | READY with no bytes → CLOSE and drop       |
| server | intra-frame read timeout    | 30 s    | header started → rest of frame must arrive |
| server | write timeout               | 30 s    | per-frame write completes                  |
| server | connection cap              | 64      | accept() beyond this: refuse immediately   |
| server | per-source connection cap   | 16      | one *remote* address; loopback exempt      |
| server | in-flight per connection    | 1       | fixed in v1 (sequential ordering)          |

All knobs are config, none are wire constants; changing them never affects
interoperability.

---

## 7. Server dispatch pipeline

```text
read frame (framed I/O layer)
    ↓
direction check (allowed_from_client)
    ↓
request ID check (§3)
    ↓
[Phase 3: authorization — BEFORE resource access]
    ↓
resource resolution (§8)
    ↓
stream response (DEFAULT_CHUNK chunks)
```

Error classes:

* **connection-fatal** (`0x01xx` statuses): ERROR with request ID 0 → CLOSE.
* **request-fatal** (`0x02xx`/`0x03xx`): ERROR with the request's ID; the
  connection continues.

Mapping filesystem outcomes to statuses — chosen so that nothing about the
tree's shape leaks:

```text
file exists, readable        → RESOURCE / METADATA
file absent                  → NOT_FOUND
permission denied            → NOT_FOUND      (hidden, not FORBIDDEN)
path is a directory          → NOT_FOUND      (no listings in v1)
open/read I/O failure        → INTERNAL
[Phase 3] unauthorized       → NOT_FOUND      (verification matrix, plan §3)
```

ERROR messages stay generic ("not found", "internal error") — never paths,
never OS error text.

---

## 8. Resource resolution (root jail)

The Phase 1–2 store is "a directory served as a tree" (real rill-store comes
later; this contract is what it must preserve):

1. At startup, canonicalize the content root; refuse to start if it fails.
2. Request path → strip leading `/` → join to root. (The codec has already
   rejected `..`, empty segments, and NUL — resolution never re-interprets.)
3. Open the file, then verify the **canonicalized** opened path still has the
   root as a prefix; otherwise → NOT_FOUND.
   Symlink policy: symlinks inside the root work; symlinks that resolve
   outside the root are hidden (NOT_FOUND).
4. `fstat` the opened handle (not the path — no TOCTOU) for size, then stream.

---

## 9. Framed I/O layer

The shared read/write loop both endpoints use:

```text
read exactly 16 bytes → decode_header
    → read exactly payload_len bytes → decode_payload
write: encode → single write (header + payload together)
```

Owns the concerns the codec deliberately cannot: direction validation,
timeouts (§6), and the read buffer (one MAX_PAYLOAD buffer per connection,
reused).

It lives in **`crates/rill-wire`**: async functions over tokio's
`AsyncRead`/`AsyncWrite` traits, used by both endpoints. `rill-protocol`
stays zero-dependency and sans-I/O; `rill-wire` is the only crate that pairs
the codec with a runtime.

---

## 10. Debug tap

Both endpoints accept `--dump-frames <dir>`: every frame sent or received is
written as `NNNN-{tx,rx}-<TYPE>.bin` (raw header + payload). This is the
inspection path once TLS makes wire capture useless:

```bash
rill get rill://localhost:7331/example.txt --dump-frames /tmp/frames
rill inspect /tmp/frames/0001-tx-GET.bin
```

---

## 11. Phase 1 verification (from the plan, restated as tests)

In-process loopback (server on `127.0.0.1:0`, real client):

* round trip: served file `cmp`-identical to downloaded file;
* missing file → NOT_FOUND;
* traversal / malformed / oversized path → rejected (codec or PATH_INVALID);
* raw hostile bytes at the server socket → ERROR + close, server stays up;
* CLI: `rill-server serve ./content --port 7331` + `rill get ...` works.

---

## 12. Decisions (resolved 2026-08)

1. **Concurrency model: tokio async.** Timeouts, cancellation, and connection
   caps come from the runtime; tokio-rustls slots in for Phase 3; rill-view's
   future concurrent fetches need no rewrite.
2. **Framed I/O home: `crates/rill-wire`.** Async read_frame/write_frame over
   `AsyncRead`/`AsyncWrite` plus direction and timeout enforcement, shared by
   both endpoints. `rill-protocol` stays zero-dependency.
3. **Symlink policy: deny-escaping.** Symlinks resolving inside the root
   work; anything resolving outside is NOT_FOUND (§8).
4. **Client request model: sequential.** One request in flight; pipelining is
   a later client-side addition against already-order-preserving servers.

Remaining open: none blocking implementation. Timeout/limit defaults in §6
stand until real-world use argues otherwise.
