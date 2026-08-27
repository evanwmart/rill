# Rill Protocol — Wire Format Working Doc

Status: **draft / working doc** — version 1 of the wire format. Nothing here is
frozen until milestone 2 ("protocol frames are inspectable") is done.

Covers Communication Phases 1–2. TLS/identity concerns live in
`specs/security.md`; this doc only defines what bytes cross an
already-established stream.

---

## 1. Conventions

* All multi-byte integers are **big-endian** (network byte order).
* `u8`, `u16`, `u32`, `u64` are unsigned integers of that width.
* Byte offsets are zero-based. Ranges are inclusive: `[4..5]` means bytes 4 and 5.
* All text (paths, error messages) is UTF-8, never NUL-terminated — lengths are
  always explicit.
* There is no padding and no alignment requirement anywhere in the format.

Rationale for big-endian: it's the network convention, and lengths read
left-to-right in a hex dump (`00 00 00 0E` is obviously 14).

---

## 2. Connection model

```text
TCP connect
    ↓
TLS 1.3 handshake (ALPN negotiates "rill/1")   ← Phase 3; plaintext in Phases 1–2
    ↓
frames, both directions, until CLOSE or EOF
```

* Protocol **version is negotiated once per connection** via ALPN
  (`rill/1`). The header still carries a version byte as a consistency check
  and so that captured frames are self-describing for `rill inspect`.
* Version 1 is strict request/response: the client sends a request frame, the
  server sends one or more response frames, in order. Request IDs exist so
  pipelining can be added without a format change.
* Either side may send CLOSE and then shut down. Anything else arriving after
  CLOSE is ignored.

---

## 3. Frame layout

Every frame is a **16-byte header** followed by `payload_len` bytes of payload.

```text
 0                   1                   2                   3
 0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
|      'R'      |      'I'      |      'L'      |      'L'      |
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
|    version    |  frame type   |             flags             |
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
|                          request ID                           |
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
|                       payload length                          |
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
|                     payload (payload_len bytes) ...           |
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
```

| Offset    | Size | Field        | Type  | Value / meaning                          |
|-----------|------|--------------|-------|------------------------------------------|
| `[0..3]`  | 4    | magic        | bytes | `52 49 4C 4C` (`"RILL"`)                 |
| `[4]`     | 1    | version      | u8    | `0x01`                                   |
| `[5]`     | 1    | frame type   | u8    | see §4                                   |
| `[6..7]`  | 2    | flags        | u16   | see §5                                   |
| `[8..11]` | 4    | request ID   | u32   | see §6                                   |
| `[12..15]`| 4    | payload len  | u32   | bytes of payload following the header    |

### Header validation order

Decoders MUST validate in this order, and reject the connection (not just the
frame) on any failure — a peer that sends a malformed header is either broken
or hostile, and resynchronizing a byte stream is not worth attempting:

```text
1. magic == "RILL"          → else PROTOCOL_MALFORMED, close
2. version == 1             → else UNSUPPORTED_VERSION, close
3. payload_len <= MAX_PAYLOAD  ← BEFORE any allocation
4. frame type is known      → else UNKNOWN_FRAME_TYPE, close
5. critical flags all known → else UNKNOWN_CRITICAL_FLAG, close
6. read payload_len bytes
7. per-type payload parsing (§7)
```

Step 3 before step 6 is the load-bearing rule: never allocate or read based on
an unvalidated length.

---

## 4. Frame types (byte `[5]`)

The high bit encodes direction: `0x00–0x7F` client → server,
`0x80–0xFF` server → client. A frame arriving in the wrong direction is
`PROTOCOL_MALFORMED`.

| Value  | Name         | Direction | Payload                        |
|--------|--------------|-----------|--------------------------------|
| `0x01` | GET          | C → S     | path (§7.1)                    |
| `0x02` | HEAD         | C → S     | path (§7.1)                    |
| `0x03` | PING         | C → S     | 0–64 opaque bytes              |
| `0x04` | CLOSE        | C ↔ S     | empty                          |
| `0x05` | GET_IF       | C → S     | path + hash (§7.1a; resource-format.md) |
| `0x07` | ACTION       | C → S     | path + typed fields (§7.5)     |
| `0x81` | RESOURCE     | S → C     | raw resource bytes (§7.2)      |
| `0x82` | METADATA     | S → C     | resource metadata (§7.3)       |
| `0x83` | ERROR        | S → C     | status + message (§7.4)        |
| `0x84` | PONG         | S → C     | echo of PING payload           |
| `0x85` | NOT_MODIFIED | S → C     | empty (§7.1a)                  |

`0x00` is deliberately unassigned — an all-zero type byte is always invalid,
which catches zero-filled buffers.

Reserved for later phases (do not reuse): `0x06` (CANCEL, see §13).

---

## 5. Flags (bytes `[6..7]`)

The 16 bits are split by compatibility behavior:

```text
bit  15  14  13  12  11  10   9   8   7   6   5   4   3   2   1   0
    ├─────────── critical ────────────┤├─────────── ignorable ─────┤
    unknown bit set → reject frame      unknown bit set → ignore bit
```

* **Bits 8–15 (critical):** an unknown set bit here means the sender is relying
  on semantics this decoder doesn't implement → `UNKNOWN_CRITICAL_FLAG`, close.
* **Bits 0–7 (ignorable):** unknown set bits are ignored. This is the
  forward-compatibility channel for hints that are safe to miss.

### Assigned flags (version 1)

| Bit | Mask     | Name         | Applies to        | Meaning                                     |
|-----|----------|--------------|-------------------|---------------------------------------------|
| 0   | `0x0001` | ACCEPT_ZSTD  | GET, GET_IF       | sender can decode zstd-encoded responses    |
| 8   | `0x0100` | MORE         | RESOURCE          | another chunk with this request ID follows  |
| 9   | `0x0200` | CONTENT_ZSTD | RESOURCE          | the chunk stream is one zstd-compressed stream |
| 10  | `0x0400` | ACTION_CAS   | ACTION            | conditional: apply only if the resource still hashes to the `_expected` field |

All other bits MUST be sent as zero. A known flag on a frame type it does not
apply to is `PROTOCOL_MALFORMED`.

The halves are chosen by failure mode: ACCEPT_ZSTD is ignorable (a server
that misses it simply sends raw — correct, just larger); MORE and
CONTENT_ZSTD are critical (ignoring either silently corrupts the resource);
ACTION_CAS is critical because ignoring it would apply, unconditionally, a
mutation the caller marked conditional — a receiver that does not implement
conditions must refuse the frame, not guess.
CONTENT_ZSTD MUST be set uniformly on every chunk of a response; mixed flags
within one response are malformed. See resource-format.md §9 for semantics.

---

## 6. Request IDs (bytes `[8..11]`)

* Chosen by the client. MUST start at `1` and increase by 1 per request on a
  connection. (Strictly increasing is what the server validates; the +1 rule
  keeps IDs human-readable in dumps.)
* Every response frame echoes the request ID it answers.
* **`0` is reserved for connection-level frames** — PING/PONG, CLOSE, and any
  ERROR the server sends when it can't attribute the problem to a request
  (e.g. a malformed header).
* Wrap-around is not supported: a connection that exhausts `u32::MAX` requests
  must reconnect. (At one request per millisecond that's ~50 days of
  continuous requests on one connection.)

---

## 7. Payload layouts

### 7.1 Path payload — GET, HEAD

```text
| Offset      | Size | Field    | Type  |
|-------------|------|----------|-------|
| [0..1]      | 2    | path_len | u16   |
| [2..2+n-1]  | n    | path     | UTF-8 |
```

`payload_len` MUST equal `2 + path_len` exactly — trailing bytes are
`PROTOCOL_MALFORMED`.

A path is valid iff **all** of the following hold (checked by the decoder,
before authorization ever sees it):

* `1 <= path_len <= 1024`;
* valid UTF-8;
* first byte is `/`;
* contains no NUL (`0x00`) byte;
* no segment is empty (`//`), `.`, or `..`;
* does not end with `/` (except the root path `/`).

Anything failing these is `PATH_INVALID` — the decoder rejects it; there is no
normalization step that "fixes" paths. What the client sends is what gets
matched against policy.

### 7.1a GET_IF and NOT_MODIFIED — conditional fetch (resource-format.md)

GET_IF payload: the §7.1 path payload followed by a hash algorithm byte
(`0x01` = BLAKE3-256; anything else rejects) and 32 raw hash bytes.
`payload_len` MUST equal `2 + path_len + 33` exactly. Semantics: "send the
resource unless its current bytes hash to this value."

NOT_MODIFIED is the empty-payload success response; RESOURCE and ERROR are
the other legal responses. Authorization is identical to GET — unauthorized
GET_IF answers NOT_FOUND and never reveals whether the hash matched.

### 7.2 RESOURCE payload

The payload is the raw resource bytes — no inner framing, no status field
(a RESOURCE frame *is* the success case; failures use ERROR).

Resources larger than `MAX_PAYLOAD` are sent as multiple RESOURCE frames with
the same request ID: every frame except the last sets MORE (`0x0100`). The
client concatenates payloads in arrival order. An empty resource is a single
RESOURCE frame with `payload_len = 0` and MORE clear. A frame with
`payload_len = 0` and MORE **set** is `PROTOCOL_MALFORMED` — zero-length
non-final chunks would allow an infinite stall.

Chunking bounds each frame but not the total resource size. That is
deliberate: the total cap is **client policy** (a configurable limit in
rill-client, so a hostile or broken server cannot fill the disk), not a wire
constant.

The server MUST NOT interleave chunks of different requests in version 1
(responses are strictly sequential); the MORE flag rather than a total-count
field is what leaves interleaving possible in a future version.

Chunk size: `MAX_PAYLOAD` is the decoder's hard upper bound, not the target.
Senders SHOULD send `DEFAULT_CHUNK` (256 KiB) chunks: per-chunk overhead is
negligible either way (16 bytes), but a chunk is the unit of scheduling — at
1 MiB a single chunk monopolizes the connection for ~84 ms at 100 Mbit/s,
versus ~21 ms at 256 KiB. Sender chunk size is a config knob; changing it
never changes the wire format.

### 7.3 METADATA payload — response to HEAD

```text
| Offset    | Size | Field      | Type | Notes                        |
|-----------|------|------------|------|------------------------------|
| [0..7]    | 8    | size       | u64  | total resource size in bytes |
| [8..9]    | 2    | reserved   | u16  | MUST be 0                    |
| [10]      | 1    | hash algo  | u8   | v2 (rill-store): 0x01=BLAKE3 |
| [11..42]  | 32   | hash       |      | v2: hash of the raw bytes    |
```

The v1 struct is bytes `[0..9]` only; v2 appends the hash. Decoders accept
either length (`hash = None` for v1) and ignore bytes beyond the newest
struct they know. Future fields extend at the end; `payload_len` tells the
decoder which version it received. Fields are never reordered or removed.

### 7.3a PING / PONG payloads

PING carries 0–64 opaque bytes. PONG MUST echo the PING payload byte-for-byte.
A PONG whose payload does not match the outstanding PING is
`PROTOCOL_MALFORMED` → close (reject-don't-repair; a peer that garbles an echo
cannot be trusted with anything else).

### 7.4 ERROR payload

```text
| Offset      | Size | Field    | Type  |
|-------------|------|----------|-------|
| [0..1]      | 2    | status   | u16   | see §8
| [2..3]      | 2    | msg_len  | u16   | 0–512
| [4..4+n-1]  | n    | message  | UTF-8 | optional, human-readable
```

The status code is the machine-readable truth; the message is advisory and
MUST NOT be parsed. Messages MUST NOT leak internal detail (filesystem paths,
policy rule names).

### 7.5 ACTION payload — the write verb

ACTION is the only frame that mutates server state (application-model.md §10).
It carries a target path and a list of typed named fields drawn from a
document's declared state slots (document-format.md). Authorization runs before
any handler, exactly as for GET.

```text
| Offset      | Size | Field     | Type  |
|-------------|------|-----------|-------|
| [0..1]      | 2    | path_len  | u16   | 1..=MAX_PATH
| [2..2+p-1]  | p    | path      | UTF-8 | validated as §7.1
| [..]        | 2    | count     | u16   | 0..=MAX_ACTION_FIELDS
| count × field                          | see below
```

Each **field** is:

```text
| Size | Field      | Type  |
|------|------------|-------|
| 2    | name_len   | u16   | 1..=MAX_FIELD_NAME
| n    | name       | UTF-8 |
| 1    | value_tag  | u8    | 1 = string, 2 = number, 3 = bool
| …    | value      |       | by tag (below)
```

Value encodings by tag:

* `1` string — `u16 len` (0..=MAX_FIELD_STRING) then `len` UTF-8 bytes;
* `2` number — 8 bytes, IEEE-754 big-endian `f64`, MUST be finite;
* `3` bool — 1 byte, `0` = false, `1` = true (any other byte is malformed).

Trailing bytes after the last field are malformed. A field-name of length 0,
an unknown value tag, a non-finite number, or a string over MAX_FIELD_STRING is
`PROTOCOL_MALFORMED` (connection-fatal).

**Response.** On success the server replies with the handler's document as a
normal RESOURCE frame (§7.2) sharing the request ID; on failure, an ERROR
(§7.4). The client renders the returned document, replacing the current page.

#### Conditional actions (compare-and-swap)

An ACTION carrying the critical `ACTION_CAS` flag (§5) is **conditional**: it
applies only if the resource it mutates still hashes to the revision the
caller observed. The revision travels as a reserved field:

```text
name   "_expected"
value  string, "blake3:<64 hex>" — the content hash of the resource, as
       resource-format.md addresses it and as HEAD reports it
```

* The flag and the field travel together. `ACTION_CAS` without a valid
  `_expected` field is `PROTOCOL_MALFORMED` (connection-fatal): a condition
  with nothing to test is a promise the receiver cannot keep. An `_expected`
  field without the flag is an ordinary field, and a server that does not
  implement conditions will treat it as one — which is exactly why the flag,
  not the field, is what makes the request conditional.
* Field names beginning with `_` are **reserved**; applications MUST NOT
  define their own.
* If the hash does not match, the server answers `CONFLICT` (§8) and MUST NOT
  apply the mutation. CONFLICT is request-scoped: the connection stays open
  and the caller re-reads and decides. The protocol detects conflicts; it
  does not resolve them.
* **Which** resource the hash refers to is the handler's to know — the path
  of an action (`/notes/actions/save/x`) is not the path of the thing it
  writes (`/notes/note/x/data`). A future schema may declare the subject; the
  server-side helper `verify_expected` takes the current bytes from the
  handler and answers.
* The comparison and the write MUST happen together, under whatever lock the
  handler already holds. Two callers that each compare and then write can
  both pass the comparison and still lose one of the two writes.

---

## 8. Status codes (u16)

Grouped by hundreds in hex for legibility in dumps.

| Code     | Name                   | Meaning                                        |
|----------|------------------------|------------------------------------------------|
| `0x0000` | OK                     | reserved; success never uses ERROR frames      |
| `0x0100` | PROTOCOL_MALFORMED     | bad magic, bad payload structure, wrong direction |
| `0x0101` | UNSUPPORTED_VERSION    | header version byte not accepted               |
| `0x0102` | UNKNOWN_FRAME_TYPE     | frame type byte not assigned                   |
| `0x0103` | FRAME_TOO_LARGE        | `payload_len > MAX_PAYLOAD`                    |
| `0x0104` | UNKNOWN_CRITICAL_FLAG  | unknown bit set in flags[8..15]                |
| `0x0105` | PATH_INVALID           | path failed §7.1 rules                         |
| `0x0200` | NOT_FOUND              | resource absent **or access denied** — deliberately indistinguishable (see security.md) |
| `0x0201` | CONFLICT               | conditional ACTION whose `_expected` revision is stale; nothing was applied (§7.5) |
| `0x0300` | INTERNAL               | server-side failure; safe catch-all            |

Codes `0x01xx` accompany connection closure. `0x02xx`/`0x03xx` answer a single
request; the connection stays usable.

There is intentionally no FORBIDDEN code: unauthorized private resources
answer `NOT_FOUND`, so anonymous clients cannot enumerate what exists.

CONFLICT discloses nothing that NOT_FOUND protects: a caller can only receive
it by presenting a revision, which it can only have by having read the
resource. **A server MUST NOT let a handler choose any other status.** The
status vocabulary is a protocol surface, and application code reaching it
directly is two bugs waiting: a `0x01xx` status returned for a bad field
would close a working connection, and a status that distinguishes "malformed
parameter" from "no such thing" is an enumeration oracle for exactly the
distinction NOT_FOUND exists to erase. Handler results are clamped to
`{NOT_FOUND, CONFLICT, INTERNAL}` at the dispatch boundary.

---

## 9. Limits (version 1)

| Constant           | Value            | Enforced by                     |
|--------------------|------------------|---------------------------------|
| MAX_PAYLOAD        | 1 MiB (`0x0010_0000`) | both sides, pre-allocation |
| DEFAULT_CHUNK      | 256 KiB          | sender policy (configurable)    |
| MAX_PATH           | 1024 bytes       | decoder                         |
| MAX_ERROR_MSG      | 512 bytes        | decoder                         |
| MAX_PING_PAYLOAD   | 64 bytes         | decoder                         |
| MAX_ACTION_FIELDS  | 32               | ACTION decoder (§7.5)           |
| MAX_FIELD_NAME     | 64 bytes         | ACTION decoder (§7.5)           |
| MAX_FIELD_STRING   | 1024 bytes       | ACTION decoder (§7.5); clients cap input to match |
| header size        | 16 bytes, fixed  | —                               |

Limits are protocol constants, not negotiated. Raising one is a version bump.
DEFAULT_CHUNK is the exception: it is sender policy, not a wire constant —
any chunking `<= MAX_PAYLOAD` is valid on the wire.

---

## 10. Annotated examples

### GET `/example.txt`, request ID 1

```text
52 49 4C 4C   magic          "RILL"
01            version        1
01            frame type     GET
00 00         flags          none
00 00 00 01   request ID     1
00 00 00 0E   payload len    14

00 0C         path_len       12
2F 65 78 61 6D 70 6C 65 2E 74 78 74
              path           "/example.txt"
```

30 bytes total on the wire.

### RESOURCE response (file contents `Hello, Rill!\n`)

```text
52 49 4C 4C   magic          "RILL"
01            version        1
81            frame type     RESOURCE
00 00         flags          none (MORE clear → final/only chunk)
00 00 00 01   request ID     1  (echoes the GET)
00 00 00 0D   payload len    13

48 65 6C 6C 6F 2C 20 52 69 6C 6C 21 0A
              payload        "Hello, Rill!\n"
```

### ERROR response — missing (or denied) resource

```text
52 49 4C 4C   magic          "RILL"
01            version        1
83            frame type     ERROR
00 00         flags          none
00 00 00 02   request ID     2
00 00 00 04   payload len    4

02 00         status         NOT_FOUND (0x0200)
00 00         msg_len        0
```

### 1 MiB file → chunked RESOURCE (256 KiB sender chunks)

```text
frame 1: type 81, flags 01 00 (MORE), req ID 3, payload len 00 04 00 00 (256 KiB)
frame 2: type 81, flags 01 00 (MORE), req ID 3, payload len 00 04 00 00 (256 KiB)
frame 3: type 81, flags 01 00 (MORE), req ID 3, payload len 00 04 00 00 (256 KiB)
frame 4: type 81, flags 00 00,        req ID 3, payload len 00 04 00 00 (256 KiB)
```

---

## 10a. Relation to TLS, and performance notes

### Layering

```text
IP → TCP → TLS 1.3 records → Rill frames
```

Rill frames are opaque application data inside TLS. TLS records carry at most
16 KiB of plaintext, so frame and record boundaries are unrelated: a 256 KiB
RESOURCE chunk spans ~16 records; small frames share records with neighbors.
Consequences:

* **Overhead:** ~22–29 bytes per TLS record (~0.17% on bulk) plus our 16-byte
  header per frame. Negligible in both directions.
* **Privacy:** frame headers, types, and paths are all encrypted. Nothing is
  observable on the wire except sizes and timing. Corollary: `rill inspect`
  operates on decoded frames (from disk or a debug tap inside an endpoint),
  never on packet captures.
* **Delivery granularity:** receivers drain plaintext in ≤16 KiB slices
  regardless of Rill chunk size; a decoder should not assume a frame arrives
  in one read.

### Latency budget (reference: 30 ms RTT)

| Scenario                                   | Cost                          |
|--------------------------------------------|-------------------------------|
| Fresh connect (TCP 1 RTT + TLS 1.3 1 RTT)  | 2 RTT before first request    |
| Cold connect → first resource byte         | ~3 RTT (~90 ms)               |
| Additional request, serial                 | 1 RTT each                    |
| N requests, pipelined                      | ~1 RTT total + transfer       |
| App as one .rillpack, warm connection      | ~1 RTT + transfer             |

* ALPN version negotiation rides inside the TLS handshake: zero extra RTT.
* TLS session resumption is kept on (rustls default); 0-RTT early data is
  deliberately **off** (replay risk, negligible benefit here).
* TCP slow start delivers ~14 KiB in the first RTT; compact documents that fit
  the initial congestion window arrive in one round trip.
* PING exists to hold connections through NAT idle timeouts — reconnecting
  costs 2 RTT, which dwarfs everything else above.

### Site/app workloads and head-of-line blocking

Version 1 responses are strictly sequential, so a large asset queued ahead of
a small one delays it (classic pipelining HOL blocking). This is accepted, not
solved, because:

1. clients MUST pipeline and SHOULD request in priority order
   (document → styles → assets);
2. a second connection for bulk assets is cheap (resumed handshake) if it ever
   matters;
3. the structural fix is `rill-pack`: an app ships as one artifact, so the
   many-small-resources workload that forced HTTP/2-style multiplexing on the
   web mostly does not exist here. Multiplexing is deliberately out of scope
   for the protocol.

---

## 11. Decoder shape (implementation note)

`rill-protocol` stays sans-I/O: decoding operates on `&[u8]`, encoding
produces buffers; no sockets, no async. The natural API:

```text
decode_header(&[u8; 16]) -> Result<Header, FrameError>
decode_payload(header, &[u8]) -> Result<Frame, FrameError>
encode(&Frame, &mut Vec<u8>)
```

This makes the crate directly fuzzable (`cargo-fuzz` target = feed bytes to
`decode_*`) and testable with the Phase 2 matrix: round trips, truncated
frames, invalid types, unsupported versions, oversized lengths, unknown
ignorable flags (accepted), unknown critical flags (rejected).

`rill inspect` is a thin printer over `decode_*`.

---

## 12. Design principles

What "well designed" means for this protocol. Size is not the metric —
ambiguity is. A protocol is broken when the answer to "what do these bytes
mean?" is "it depends."

1. **Decidability.** Every byte sequence is either valid with exactly one
   meaning or invalid with exactly one rejection. No value has two encodings.
2. **History-free parsing.** Connection state lives above the codec; the
   byte-level meaning of a frame never depends on prior frames. `rill inspect`
   on a lone frame is the enforcement mechanism.
3. **Reject, don't repair.** No lenient parsing. We control both endpoints;
   strictness is free and leniency becomes load-bearing.
4. **Extensibility only where declared.** The ignorable-flag byte, reserved
   frame types, and the append-only METADATA struct are the only extension
   points. Everything else is rigid on purpose.
5. **Layer ignorance.** The protocol moves opaque named bytes. Document
   encodings (rill-doc), compression (rill-store), and packaging (rill-pack)
   must be swappable without touching this spec. If a frame type ever needs to
   know what a document node is, the layering has failed.
6. **Spec first.** This document is the source of truth; the implementation
   witnesses it. Round-trip property tests and fuzzing stand in for a second
   independent implementation.

Note for upper layers: compiled/enum-packed document encodings inherit these
same obligations one level up — explicit stable discriminants, a version
field, and defined unknown-variant behavior belong in
`specs/document-format.md`.

---

## 13. Open questions

1. **Checksum in the header?** TLS already provides integrity on the wire, and
   content hashes arrive with rill-store. Leaning **no** — a CRC would only
   protect plaintext Phase 1 traffic and inspect-from-disk cases.
2. **Should HEAD → METADATA also precede RESOURCE?** i.e., does GET return
   METADATA + RESOURCE so the client can preallocate the full size before
   chunk 1? Cheap now, mildly redundant. Currently: GET returns RESOURCE only.
3. **PING liveness policy** — who pings, how often, and does an idle timeout
   belong in the spec or in server config? Currently server config.
4. ~~Reserved frame types `0x05`/`0x85` for conditional GET~~ — resolved:
   assigned in §7.1a per resource-format.md.
5. **Is strictly-sequential response ordering worth keeping through Phase 3**,
   or should MORE-interleaving be legal from the start? Current answer: keep
   sequential; request IDs make relaxing it a server-side change only.
6. **Request cancellation.** In v1 a client that stops wanting an in-flight
   resource can only close the connection. Probably reserve `0x06` CANCEL
   (payload: request ID being cancelled) rather than design it now; becomes
   worth having once large resources are common.
