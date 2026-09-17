# c-minimal — a Rill endpoint written from the specs alone

The first **foreign** implementation of the wire's endpoint role. It shares
no code with the Rust crates: portable C for the protocol and document, our
own BLAKE3 from its spec, and a thin TLS seam (OpenSSL on the host).
Everything in it was written from four documents —

- `specs/protocol.md` — frame layout, types, flags, statuses, limits
- `specs/connection.md` — session lifecycle, TLS 1.3 + ALPN `rill/1`
- `specs/security.md` — self-signed certificate, pinned by fingerprint
- `specs/document-format.md` — the RDOC document a device emits

— and then pointed at the real Rill client. The question it exists to
answer is the stranger-implementability bar: *can someone who is not the
author build a working endpoint from the written spec?* The answer is yes,
with three spec corrections, recorded below.

## What it is

A **serve-only** sensor endpoint: it serves one live document at `/temp`
(a real reading from the host's thermal sensor) with a `live` node, so a
Rill viewer polls it once a second. That is the whole role of a device on
the wire — values plus a clock, no rendering, no code shipped to the
viewer. The MCU version of this (the appliance lead scene) is this program
with a smaller TLS stack and a temperature pin.

## Run it, and prove it against the real client

```sh
make                 # gcc + OpenSSL 3
./gen-cert.sh        # self-signed P-256 cert; prints the fingerprint the viewer pins
./endpoint 7430      # TLS 1.3, ALPN rill/1

# from the repo root, with any device identity dir:
rill auth trust rill://127.0.0.1:7430 --identity $ID --yes   # pins the fingerprint
rill get   rill://127.0.0.1:7430/temp -o /tmp/temp.rdoc --identity $ID
rill doc inspect /tmp/temp.rdoc                             # the real decoder's verdict
```

Measured 2026-09-12 on the workstation, against `rill` built from the tree:
the TLS 1.3 + ALPN handshake succeeds; `rill auth trust` pins exactly the
fingerprint `gen-cert.sh` printed; `rill get` returns the document; a second
cached fetch sends `GET_IF` and accepts the answer; `rill doc inspect`
decodes it as the intended tree; and rill-vector's headless renderer draws
it (`RILL_DOC=… DOC_PREVIEW=… cargo test -p rill-vector render_document --
--ignored`).

## What it implements

| Spec | Implemented |
|---|---|
| Header validation in order (§3): magic, version, length-before-allocation, type, critical flags | yes |
| GET, HEAD, GET_IF, PING, CLOSE, ACTION (C→S); RESOURCE, METADATA, PONG, ERROR (S→C) | yes |
| Path rules (§7.1): leading `/`, no NUL, no empty/`.`/`..` segments, no trailing `/`, UTF-8, ≤1024 | yes |
| Request IDs strictly increasing from 1; `0` for PING/CLOSE (§6) | yes |
| Fatal path (connection.md §2): ERROR on request 0, then CLOSE, then shut down | yes |
| TLS 1.3 only; ALPN `rill/1` required; client cert requested, not required | yes |
| RDOC v8: header, sorted string table, node table (Text, Column, Live), tree canonicalization | yes |
| GET_IF → `NOT_MODIFIED` when the viewer's BLAKE3 matches the current bytes (§7.1a) | yes |
| HEAD → METADATA v2: size + BLAKE3 of the bytes (§7.3) | yes |

## Deliberately minimal — each a choice, not an omission

- **No zstd.** `ACCEPT_ZSTD` is an ignorable flag; we send raw. Legal.
- **No actions.** A read-only sensor has nothing to act on; `ACTION` answers
  `NOT_FOUND`, the only status a handler may choose (§8).
- **Policy is "everything public to anonymous."** The client certificate is
  requested but never inspected. A sensor readout has no private paths.
- **No styles.** Every node carries `style_ref = 0xFFFF`; the viewer's own
  theme dresses it. A device should not have to know about fonts.
- **One forked child per connection, no pipelining** — v1 is strict
  request/response anyway (§2).

## Findings — where the spec was not the spec

The first two were found by this program failing against the real decoder,
which is the point of writing it from the documents alone; the third came
from auditing the decoder's remaining node arms against the spec table
afterwards. All three are fixed in `specs/document-format.md` as of
2026-09-12.

1. **The document version byte.** The spec's header table said `[4]
   version = 0x01`. The shipping decoder (`rill_doc::VERSION`) requires
   **8** and rejects anything else with "unsupported document version". The
   format had evolved through seven versions — state and action tables, new
   node types — and the header table never followed.
2. **Row/Column carry a `target`.** The spec's node table gave the body as
   `gap Dim, padding Dim, child_count u16, children u32×n`. The v8 decoder
   reads `gap Dim, padding Dim, target u16, child_count u16, children` —
   containers became link targets (`0xFFFF` = none) and the table was not
   updated. A from-spec encoder is two bytes short and fails
   canonicalization.
3. **Button carries an `icon`.** The spec's row gave `label_idx u16,
   action_idx u16`. The decoder reads `label_idx, icon_idx (0xFFFF =
   none), action_idx`. Same shape of drift: a field added in code, the
   table not updated. (Slider, Link, Scroll, When, Icon, Key, Menu, Keys,
   TextInput and Chrome were audited and match.)

Everything else in the four documents was sufficient to implement, first
try: the wire framing, flags, request-id rules, the fatal path, TLS/ALPN,
the pinning model, the string table, and the Text/Live bodies. That is a
strong result for the spec, and the three drifts are exactly what the
conformance-vector work (the funded R1) exists to make impossible.

**Tooling note for a stranger:** `rill inspect` reads *frame* dumps, not
documents; the document tool is `rill doc inspect`. Worth a line in the
spec's implementation notes.

## Files, and the two seams

```
endpoint.c      the protocol + the document — portable C, spec-only
blake3.{h,c}    BLAKE3-256 from its spec; verified against the published
                vectors and, at 19 sizes across every block/chunk boundary
                (0 B … 1 MiB+1), against the Rust client's own blake3 crate
tls.h           the TLS seam: 7 functions the endpoint needs from a TLS stack
tls_openssl.c   the host implementation (OpenSSL 3)
```

The MCU port supplies `tls_mbedtls.c` against `tls.h` and a sensor read;
`endpoint.c` and `blake3.c` compile unchanged. `make test` runs the BLAKE3
vectors; `./blake3_test FILE` hashes a file for an external oracle.

**Observed live, 2026-09-13:** with the endpoint on the workstation and the
Pi's bare-metal glass polling it across the LAN, a restart of the endpoint
was survived transparently — the viewer reconnected and its first request
was a `GET_IF` carrying the hash it already held, answered `NOT_MODIFIED`.
Every poll since answers `NOT_MODIFIED` until the reading changes. That is
the honest behaviour for a sensor on a metered link: a stable value costs
16 bytes a second, not a document.

## Next

- Port to the chosen MCU (hardware selection is an open work item; ESP32-
  class with hardware crypto is the pragmatic TLS-capable candidate). The
  protocol and document code here is the port; only the TLS stack and the
  sensor read change.
- The node-table audit against `codec.rs` is done for the assigned v1
  types: three rows were stale (fixed), the rest match. Each stale row is a
  conformance vector waiting to be written.
