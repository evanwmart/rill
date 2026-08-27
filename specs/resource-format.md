# Rill Resources — Content Addressing Working Doc

Status: **draft / working doc**. Covers Resource Phase 1 (milestones 5–6):
content hashes and the client cache. Compression (Phase 2) and packages
(Phase 3) come later. Builds on `specs/protocol.md` and
`specs/connection.md`.

---

## 1. Model

```text
path → current content hash        (mutable, the server's answer today)
hash → immutable bytes             (content-addressed, verified, cacheable)
```

* Hash algorithm: **BLAKE3-256**. On the wire a hash is an algorithm byte
  (`0x01` = BLAKE3-256) followed by 32 raw bytes; in text it renders as
  `blake3:<64 hex chars>`.
* Unknown algorithm bytes are a decode error (decidability; new algorithms
  are a protocol version bump).
* The hash is computed over the resource's **raw bytes** — exactly what a
  RESOURCE stream reassembles to. (When Phase 2 adds compression, the hash
  stays over decoded bytes; encoding is transport detail.)

## 2. Wire extensions (assigned from protocol.md's reserved values)

These fill the frame types reserved in protocol.md §4; protocol version
stays 1 (pre-release latitude — both ends ship together).

### GET_IF `0x05` (client → server)

"Send the resource unless it still hashes to what I have."

```text
| Offset          | Size | Field     |
|-----------------|------|-----------|
| [0..1]          | 2    | path_len  |
| [2..2+n-1]      | n    | path      |
| [2+n]           | 1    | hash algo (0x01) |
| [3+n..34+n]     | 32   | hash      |
```

Responses: `NOT_MODIFIED` | `RESOURCE…` | `ERROR`. Authorization is
identical to GET — an unauthorized GET_IF is `NOT_FOUND` and reveals
nothing, including whether the hash matched.

### NOT_MODIFIED `0x85` (server → client)

Empty payload; echoes the request ID. Means: the resource exists, you are
authorized, and its current bytes hash to the value you sent.

### METADATA v2 (append-only extension per protocol.md §7.3)

```text
size (u64) + reserved (u16, =0) + hash algo (u8, 0x01) + hash (32 bytes)
```

Old decoders see the longer payload and ignore the tail; new decoders accept
the 10-byte v1 form with `hash = None`.

## 3. Request flow (plan § Resource Phase 1)

```text
client wants (server, path)
    ↓ has cached hash for it?
yes → GET_IF path+hash          no → GET path
    ↓                                ↓
NOT_MODIFIED → serve from cache      RESOURCE stream
    ↓ (cache read re-verifies;       ↓
     corrupt → delete, refetch)      client hashes received bytes
                                     ↓
                                     object stored by hash; ref updated
```

* The client never trusts a cached object blindly: **every cache read
  re-hashes the object**; a mismatch deletes the entry and falls back to a
  full GET.
* The client computes hashes itself — a server cannot poison the cache with
  a wrong hash, only serve wrong bytes under their own (correct) hash.

## 4. Client cache layout

```text
[default ~/.cache/rill]           (RILL_CACHE env / --cache flag override)
├── objects/
│   └── 7f/92…                    object bytes, sharded by first hex byte
└── refs/
    └── <blake3(authority+path) hex>   one line: "<authority><path> blake3:<hex>"
```

* `objects/` is pure content-addressing: identical resources stored once,
  any number of refs may point at one object.
* A ref binds `host:port` **and** path to a hash — the same path on two
  servers is two refs (possibly sharing one object).
* Writes are temp-file + rename (no torn objects); object files are
  read-only by convention, refs are tiny rewritable text files.

## 5. Server-side hashes

The server maintains an in-memory memo: `canonical path → (mtime, size,
hash)`. A file is rehashed only when mtime or size changes; a restart
clears the memo. Accepted caveat (recorded decision): an edit preserving
both mtime and size serves a stale NOT_MODIFIED until restart — effectively
impossible to hit by accident with real editors.

The memo is consulted by GET_IF (compare) and HEAD (METADATA v2 hash);
plain GET never hashes — it just streams.

## 6. Verification matrix (plan § Resource Phase 1)

```text
unchanged resource                    → NOT_MODIFIED, zero payload bytes
changed resource                      → full RESOURCE stream, ref updated
corrupted download (hash mismatch)    → (TLS makes this ~impossible; the
                                        recompute-on-receive is what seeds
                                        the cache honestly)
corrupted cache entry                 → detected on read, deleted, refetched
identical resources, two paths        → one object, two refs
unauthorized GET_IF                   → NOT_FOUND, no hash oracle
```

## 7. Crate shape

```text
rill-store
    Hash (blake3 wrapper, text/wire forms); ObjectStore (put/get/verify/
    remove, sharded); RefIndex (authority+path → hash); Cache (compose).
    Used by the client now; the server reuses Hash + hashing helpers.

rill-client
    cache_dir config; GET_IF flow; Fetched { data, hash, from_cache }.

rill-server
    GET_IF/NOT_MODIFIED dispatch; METADATA v2; the mtime+size memo.

rill (CLI)
    rill get --cache DIR | --no-cache;  rill cache stats | verify | clear
```

## 8. Compression (Resource Phase 2)

Encodings: `raw` and `zstd`. Compression is **transport-only** — the hash, the
cache, METADATA's size, and every layer above see decoded bytes; only RESOURCE
payloads on the wire are ever compressed. This is the plan's
"representation and compression remain separate" rule made concrete.

### Negotiation (protocol.md §5 flags)

```text
client: GET/GET_IF with ACCEPT_ZSTD (0x0001, ignorable)
            ↓
server: RESOURCE chunks with CONTENT_ZSTD (0x0200, critical)  — or raw
```

* A server ignoring ACCEPT_ZSTD (or choosing not to compress) sends raw —
  always correct, never negotiated further. Zero round trips.
* CONTENT_ZSTD chunks concatenate to **one** zstd stream for the whole
  resource (compress-then-chunk, not chunk-then-compress).
* NOT_MODIFIED, METADATA, and the hash are untouched: the hash is over
  decoded bytes, so a cached object fetched raw and one fetched compressed
  are the same object.

### Server policy (config, not wire law)

Compress iff: client sent ACCEPT_ZSTD ∧ decoded size ≥ `[default 1 KiB]` ∧
extension not in the known-compressed skip list (jpeg/png/webp/audio/video/
archives — the plan §Resource Phase 2 table). Level `[default 3]`. Streaming:
the server compresses chunk-by-chunk with bounded memory; it never buffers
the whole file.

### Client obligations

* Cap the **compressed** stream at `max_resource` while receiving, and cap
  the **decoded** output at `max_resource` while decompressing — the second
  cap is the decompression-bomb guard.
* Enforce uniform CONTENT_ZSTD across a response's chunks; mixed → connection
  error.

## 9. The `.rillpack` format (Resource Phase 3)

One deterministic, indexed, random-access artifact holding a complete site or
application. Same conventions as the wire format: big-endian, explicit
lengths, no padding, strict validation — including on **read**: a pack whose
index is unsorted, out of bounds, or malformed is rejected, so determinism is
enforced, not just promised.

```text
[ header 48B ][ string table ][ index count×64B ][ blobs ][ footer 36B ]
```

### Header (48 bytes)

| Offset    | Size | Field               | Value                     |
|-----------|------|---------------------|---------------------------|
| `[0..3]`  | 4    | magic               | `"RPCK"`                  |
| `[4]`     | 1    | version             | `0x01`                    |
| `[5..7]`  | 3    | reserved            | 0                         |
| `[8..11]` | 4    | resource count      | u32                       |
| `[12..19]`| 8    | string table offset | u64 (= 48)                |
| `[20..27]`| 8    | string table size   | u64                       |
| `[28..35]`| 8    | index offset        | u64                       |
| `[36..43]`| 8    | index size          | u64 (= count × 64)        |
| `[44..47]`| 4    | reserved            | 0                         |

### String table

Concatenated UTF-8 path bytes (no separators, no NULs), in index order.
Paths obey the protocol §7.1 rules.

### Index — one 64-byte entry per resource, sorted strictly ascending by
path bytes (binary-searchable; strictness also guarantees uniqueness)

| Offset    | Size | Field        | Notes                                |
|-----------|------|--------------|--------------------------------------|
| `[0..3]`  | 4    | path offset  | u32, into the string table           |
| `[4..5]`  | 2    | path length  | u16                                  |
| `[6]`     | 1    | encoding     | 0 = raw, 1 = zstd                    |
| `[7]`     | 1    | reserved     | 0 (future: resource type)            |
| `[8..39]` | 32   | hash         | BLAKE3 of **decoded** bytes          |
| `[40..47]`| 8    | blob offset  | u64, absolute file offset            |
| `[48..55]`| 8    | encoded size | u64                                  |
| `[56..63]`| 8    | decoded size | u64                                  |

The fixed-size entries *are* the plan's "metadata blocks" — merged into the
index so lookup is one binary search plus one ranged read.

### Blobs

Encoded resource bytes, concatenated in index order. Extraction reads one
blob's range, decodes if zstd, and verifies the hash — never the whole
package (the "no full decompress" requirement).

### Footer (36 bytes)

BLAKE3 (32 bytes) of the entire file before the footer, then tail magic
`"KCPR"`. Opening a pack checks structure and magics only; `rill pack
verify` checks the footer hash and every resource hash. Per-resource reads
are always hash-verified regardless.

### Determinism

Byte-identical output for identical input trees: paths sorted, no
timestamps, fixed zstd level, attempt-and-compare compression (per resource:
compress iff eligible — ≥1 KiB, extension not known-compressed — **and**
the result is actually smaller; offline build can afford the comparison the
streaming server skips). Caveat: determinism is per zstd library version.

## 10. Decisions (resolved 2026-08)

1. **Server hash strategy: mtime+size memo**, stale-edit caveat accepted.
2. **Cache reads always re-verify**; corrupt entries deleted and refetched.
3. **`rill cache stats|verify|clear` ships now** as a thin store wrapper.
4. **Compression negotiation via flags** (ACCEPT_ZSTD ignorable request bit,
   CONTENT_ZSTD critical response bit); hash always over decoded bytes;
   server policy = accept ∧ ≥1 KiB ∧ not known-compressed extension.
5. **Pack format per §9**: metadata merged into fixed-size index entries;
   strict-sorted index validated on read; attempt-and-compare compression at
   build time; serving *from* packs deferred to the application phases.
