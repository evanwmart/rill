# Rill Semantic History — pre-design (VERY HIGH PRIORITY, 2026-08-11)

Status: **requirements + design directions, not yet designed.** Elevated by
from "session recording demo" to a committed pillar: *the session is
data — memory, observation, sharing, agency, and compliance are queries
over one log.* Recording/remoting/teacher-view/agent-context are not five
features; they are one substrate read five ways.

Requirements, verbatim: **secure, lightweight, scalable, reliable, fast.**

## What already exists (the seed, all shipped)

* `.rillrec` codec (`crates/rill-ui/src/recording.rs`): `RRC\x01` header,
  `Stamped{t_ms, RecEvent::{Window,Closed,Order,Frame,Pointer}}`, strict
  decode + `decode_lossy` (a log killed mid-write replays to its last
  whole event — the crash-honest property to preserve at every layer).
* Compositor recorder (`recorder.rs`): state-diff sync once per tick (no
  per-mutation hooks), frames captured verbatim at the stream latch (the
  client's own bytes, no re-encode), write-failure stops recording and
  reports once.
* Replay (`rill-vector --replay`), timeline dump (`rill inspect`).

Known gaps, previously logged: BufWriter tail lost on SIGTERM (no signal
handler); toggle-based, not a durable always-available service; one flat
file per session; no compression, no index, no retention, no encryption,
no query surface.

~~Blocking bug: recorder window ids collide across clients.~~ FIXED and
verified 2026-08-17 (see TODO.md) — the compositor assigns its own
monotonic id per surface instead of `protocol_id()`, which is unique only
within one client's connection. Two separate clients now record as two
windows with distinct ids, which is what everything downstream inherits.

## Requirements decomposed

* **Reliable** — crash of compositor, client, or power must lose at most
  the current in-flight event batch. Segmented logs sealed at boundaries;
  fsync policy explicit; signal handler flushes; `decode_lossy` semantics
  hold per segment. Recording failure NEVER stalls or degrades
  compositing — history is a shed load under pressure, marked as a gap
  event (an honest hole beats a silent one, and beats a stalled desktop).
* **Fast / lightweight** — write path stays off the render loop (already
  true: latch-point capture + per-tick diff). Target: history steady-state
  cost invisible in the HUD (<1% CPU, ~zero allocations per frame beyond
  the verbatim blob it already holds). Frames are command streams and
  **compress 11–22× (MEASURED)**; compress at chunk flush and seal, never
  on the hot path.
* **Scalable** — MEASURED: ~6 MiB/hour compressed while busy, ~6 GiB of
  frames and ~159 MiB of transcript per working year; a decade of
  transcript is ~1.6 GiB. Mechanisms: segment rotation (size/time),
  zstd per segment, retention policy as config, and an index so search
  never replays raw logs.
  Multi-device: histories are per-device streams that can ship to the
  owner's server like any other resource (content-addressed segments —
  the relay/backup subscription carries them; cross-device timeline is a
  server-side merge by timestamp).
* **Secure** — this is the sensitive file on the disk. At rest:
  encrypted segments (key held by the device identity / owner key,
  scheme TBD). Access: reading history is a brokered capability, and
  reads are themselves logged — the audit trail has an audit trail.
  Scope: sensitivity tiers over WHAT detail is retained and who may
  open it (see the tier model), with the capability log proving the
  scope. **Never record raw keystrokes** —
  semantic events only (actions, frames); masked inputs must reach the
  log masked, which they do structurally IF the frame is the source of
  truth (the caret/mask is rendered, keys are not events in the stream).
  Pixel windows already record as labeled placeholders — a privacy
  accident worth keeping as a rule: what the semantic layer cannot see,
  history does not keep.

## Design directions (proposed, not decided)

* **Storage: stay append-only, no database in the base image.**
  `~/.local/share/rill/history/<device>/<segment>.rillrec[.zst]` +
  a tiny sidecar index per sealed segment. The log remains the source of
  truth; indexes are derived, disposable, rebuildable. (House ethos:
  a format one person can fully audit beats an embedded DB.)
* **Index: extract at seal time.** Text runs (with window + timestamp),
  window titles, app identities, action invocations → per-segment
  inverted index; "find the moment this error appeared" = index hit →
  seek → replay from nearest Order event. Hand-rolled and minimal first;
  fancier search lives in a history *app*, not the substrate.
* **Query surface: history is served, not linked.** A `rill://` endpoint
  (or local store API) over segments — the history app, the agent
  surface, and compliance export are all *clients*. The LLM featureset
  reads the same interface a human's timeline app does.

* **RULE — a viewer of the history renders only stable functions of it
  (established 2026-08-21, by measurement).** Recording is always-on and a
  history viewer is itself recorded, so the two form a loop, and the loop
  has two dimensions. The *content* dimension is closed by classification:
  a viewer's pages declare a raised tier, so its own reflection never
  enters the index it reads. The *volume* dimension is closed only by
  determinism: the served page must be a pure function of the corpus
  content it shows — same content, same bytes — so the live tick answers
  NOT_MODIFIED, the client never redraws, and the recorder stays quiet.

  What violates it, concretely, because each of these shipped in the first
  history app and compounded: relative timestamps ("55s ago" ages every
  refresh into a changed page); live counters over the *open* segment (the
  count ticks because the page was shown — the viewer's own frames are in
  it); anything derived from the wall clock or the viewer's own recording
  rather than from the content displayed. Measured cost of violating it:
  an idle desktop with one History window open recorded 104 KiB/min of its
  own reflection — ~150 MB/day of nothing — against ~6 MiB/hour measured
  for a genuinely busy desktop. Fixed (absolute clock stamps, stats over
  sealed segments only), the same idle hour records zero after a 13-event
  settle, and the test pins the mechanism: an open segment gaining only
  the viewer's own tier-raised events must not change the served bytes.

  This binds every future surface that shows history — a dock ticker, an
  agent status pane, a timeline widget — and both halves are needed: the
  tier declaration alone leaves the volume leak, determinism alone leaves
  the index echo.

## Decisions (one at a time, author calls)

All nine are settled as of 2026-08-17 — 1–3, 6 and 9 on 2026-08-11, and
4, 5, 7 and 8 on 2026-08-17. What remains open in this spec is not
decisions but *measurements* (zstd level and whether a DrawCommand
dictionary pays, transcript text-delta strategy, bloom sizing), each
already marked "start simple and measure", none of which blocks the
writer. Decisions 6–9 are stated further down, beside the sections that
raised them.

1. **Default posture: ALWAYS-ON (decided 2026-08-11).** History records by
   default. The corpus must exist to be worth anything; the Recall
   failure was careless substrate, not the idea — here it is semantic,
   local, encrypted, owner-held. A visible "rec" indicator (HUD/dock)
   ships in the same slice.

   **AMENDED 2026-08-12 (author decision): no pause, and no exclusions.** The
   original decision paired always-on with a one-gesture pause and
   per-app exclusions. The later tier model made both incoherent: a
   pause *is* the omission model this design rejects — it destroys
   information rather than compartmenting it, puts a hole in the
   timeline that must either leak metadata (visible) or lie (invisible),
   and weakens the "complete within your entitlement" claim the
   enterprise and education framings rest on. Per-app exclusion is the
   same mistake in smaller clothes.

   The invariant is therefore absolute: **nothing on the machine is ever
   unrecorded; sensitivity is expressed only as classification.** An app
   the owner does not want in the searchable corpus is *pinned to a high
   tier*, not excluded. Consequences accepted:

   * The escape hatch is **hard delete** (decision 3), which is a
     strictly better answer than pause: explicit, logged, destructive by
     intent, and available *after* the fact rather than requiring the
     user to predict the moment. The case for pause was always foresight
     the user will not have.
   * **Optics:** an always-on recorder with no off switch is the shape
     that got Recall in trouble. What carries it here is that the record
     is local, encrypted, owner-keyed, tier-compartmented, and deletable
     — and that the indicator never lies about what is happening.
   * **Third-party data** (a guest at the keyboard, someone else's
     password) has no preventive answer; only hard delete afterwards.
     Stated plainly rather than glossed.

   Follow-on, not built: **retroactive classification** — an append-only
   event that seals a *past* time range, so "that should not have been
   captured" can be answered without rewriting the log.

2. **Key custody: DEVICE KEY NOW, OWNER KEY LATER (decided 2026-08-11).**
   Slice 1 encrypts segments to the device's existing identity key —
   zero new UX, protects against media theft and backup leaks. The
   segment header MUST carry a keyslot table from day one (even with one
   slot), so an owner-level passphrase key can wrap in later without
   reformatting history, and so replicated segments can eventually be
   ciphertext-only to the server. Honest limit accepted: whoever
   controls the unlocked device reads its history — the single-user
   posture today.

   **Built 2026-08-21.** Per-segment random data keys; chunks and the
   seal's index blobs XChaCha20-Poly1305 under them (compress-then-
   encrypt); the data key wrapped into a device keyslot under a KEK
   derived from `device-key.pem` — zero new files, zero new UX, and the
   keyslot table now holds what it was shaped for. Locked is an error,
   never an empty read; the CLI counts what it could not open; an
   unenrolled machine records plaintext and announces it once at boot.
   Retention re-encrypts through rewrites under fresh data keys. The
   chunk header's plaintext hash survives encryption, so crash-honesty
   and the merkle are unchanged. One leak found live and closed: the
   plaintext corpus manifest had cached an encrypted segment's bloom — a
   vocabulary-membership oracle beside the ciphertext — so encrypted
   rows are never persisted and the manifest format bumped to discard
   every pre-fix cache. Remaining from this decision: the owner
   passphrase keyslot, and keyslot re-wrap on identity rotation.

3. **Retention: TIERED FIDELITY DECAY (decided 2026-08-11).** The corpus
   has two economies — transcripts carry all the search/agent value at
   **3.3–7% of frame bytes raw** (MEASURED by the spike; the ~1% first
   assumed here was wrong), and frames buy pixel-perfect replay at
   essentially all the rest. Compressed the gap widens further: ~81
   KiB/hour of transcript against ~3–6 MiB/hour of frames, so decay
   still buys ~30×. Fidelity decays instead of records being deleted:

   ```text
   transcripts   kept indefinitely     (search + agent memory never expire)
   frames        rolling 90 days       (replayable recent past)
   pinned range  kept whole, forever   (explicit user act)
   hard delete   first-class operation (policy/legal, not disk-pressure)
   ```

   Product line this yields: *"remembers everything you've ever seen,
   can replay the last few months."* Only a semantic format can make
   this trade — screenshot systems must OCR-and-discard, which is lossy
   and expensive; here the cheap artifact already sits beside the
   expensive one. Measured cost of "forever": **~159 MiB/year of
   transcript** at the spike's activity level (8h/day × 250d), so a
   decade is ~1.6 GiB. A 90-day frame window is ~1.5 GiB. Implementation: aging drops frame chunks from sealed
   segments and rewrites the footer, keeping transcript+index intact
   (segments must therefore be laid out frames-separable — a format
   constraint, noted for the writer).

   **Status (2026-08-19).** Two of the three preconditions hold: chunks are
   frames-separable (the writer never mixes frame payloads with events), and
   the transcript is now stored as `Text` (0x04) rather than derived, so
   dropping frame chunks no longer takes the searchable history with them —
   which it did, silently, for as long as the transcript was recomputed from
   the frames.

   ~~The third does not: there is no seal and no footer.~~ **Sealed
   2026-08-21.** `SegmentWriter::finish` now seals: the segment gains its
   per-tier stored indexes, a footer (event count, span, tiers, chunk
   count, flat blake3 over the chunk plaintexts, merkle root over their
   per-chunk hashes), and a 40-byte tail whose region hash covers the seal
   itself — added after the first tamper test showed a flipped byte inside
   a stored index reading back as a valid, different index. Semantics that
   fell out of building it:

   * **A sealed segment refuses damage rather than salvaging around it**
     (`SealBroken`); the crash-tolerant read belongs to open segments only.
     Salvage semantics on a sealed file would let tampering pass as crash
     damage.
   * **Sealing is idempotent and is the crash-recovery path**: `seal_path`
     on a segment a dead writer left open truncates the torn tail to the
     last durable chunk — the same bytes the tolerant reader would have
     surrendered — and seals what remains.
   * **The corpus reads the seal, not the chunks**: a manifest rescan of a
     sealed segment costs two small reads (header + seal region) instead of
     decoding every chunk, and search/tail/show consult the stored index.
     The log stays the source of truth — a stored index is a skip of the
     rebuild, never a different answer.
   * The `segment_read` fuzz target now asserts the sealed contract too
     (a reported seal implies verified counts and no torn tail); 820k
     executions clean at landing, with sealed, open, and seal-tampered
     seeds in the corpus.

   **Retention landed 2026-08-21**: aging, pinning and hard delete, one
   event-level rewrite worn three ways (`rill-history::retention`). The
   chunk-verbatim copy the spec's "rewrites only the footer" implied turned
   out to be wrong in a way worth recording: every event carries a delta
   from its predecessor, so dropping a frame chunk would silently shift
   every later timestamp earlier by the span it covered. The rewrite
   re-anchors each kept event at its original absolute time instead —
   what survives says exactly when it always said. Aged and cut segments
   re-seal; pins are sidecar files (`.rhs.pin` — pinning never rewrites
   what it protects, and `ls` shows the policy); delete is dry-run by
   default, refuses pinned segments (two explicit intents in conflict is
   a person's call), removes fully-covered segments and cuts ranges out
   of partially-covered ones. The compositor ages at boot
   (`RILL_HISTORY_FRAME_DAYS`, default 90). All four verified against
   the live corpus, including a pinned refusal and a mid-segment cut.

   **The live recorder landed the same day**: the compositor writes `.rhs`
   always-on (decision 1, with the corner badge; `RILL_HISTORY=0` is a
   boot-time configuration, not a pause), sheds-and-marks under
   backpressure, extracts transcripts at the frame latch from commands it
   had already decoded, rotates and seals at the segment target, seals on
   shutdown, and seals a crashed predecessor's segment on the next start —
   verified with kill -9. Aging — dropping frame chunks from sealed
   segments and rewriting the footer — is now the next slice.

   Counterweights accepted: hard-delete-by-range must be a real
   operation from the start (enterprises mandate deletion; households
   need forgetting); pinning must exist at v1 or the first thing anyone
   loses teaches them the policy existed; both defaults live in config
   (the appliance/SD profile will want a shorter frame window).
4. **Recording scope: THE DOCUMENT DECLARES ITS TIER (decided
   2026-08-17).** The question as originally posed — per-app allow/deny,
   private mode, how a "do not record" surface declares itself — was
   answered by the decision-1 amendment before it was ever taken: there is
   no exclusion and no pause, so nothing declares "do not record". The only
   move in the vocabulary is *raising* sensitivity, and the live question
   was at what granularity an app may do it.

   **Granularity: the served document, via a `sensitive tier=N` node** —
   a declaration alongside `page`, `live`, `keys` and `closing`. Per-app
   was rejected for the reason the tier model exists at all: sensitivity
   is a property of what happened, not of who was running, and the same
   notes app writes a shopping list and a recovery phrase. A manifest
   number re-adopts exactly the app-class thinking the tier section
   supersedes. Owner-policy-only was rejected because it burdens the one
   party who cannot know which of an app's pages is sensitive, and leaves
   a well-behaved app no way to protect its users.

   **How it reaches the writer, and why this is not a second mechanism.**
   The observation boundary settles that clients declare tier over
   `rill_stream_v1::set_tier(n)`, latched with the next frame. That is the
   *client→compositor* leg. But the app is a **server** serving documents;
   the client holding the stream is `rill-vector`. The app therefore has
   exactly one channel to its own viewer — the document — so the node is
   the missing *server→client* leg, and `set_tier` is what carries it the
   rest of the way:

   ```text
   app  --document(`sensitive tier=2`)-->  rill-vector
        --set_tier(2) latched with frame-->  compositor  --> history
   ```

   The ratchet still applies at the end of that chain: owner and org
   policy may raise what the document claimed, and only the owner lowers.

   **Built 2026-08-21, end to end.** `sensitive tier=N` is node `0x0014` —
   the one declaration in the *critical* type half, so an old viewer
   refuses the document rather than recording it at T0 (verified by
   accident during the build: a stale binary hit exactly that refusal).
   The tree hoists it (two declarations ratchet to the higher), the
   viewport exposes it, rill-vector sends `set_tier` (stream v3) before
   the frame it classifies and refuses to attach declared-tier frames to
   a pre-v3 compositor, the compositor latches it per frame and
   post_errors unknown tiers, and the history writer stamps Frame, Text,
   Window and Closed events at the window's tier — with segment-seed
   snapshots split per tier so a sensitive title never rides a routine
   Snapshot into the T0 index. Proven live: a page declaring tier=1
   recorded as T0,T1; `rill history grep` at T0 cannot find its text and
   at `--tier 1` finds it in ~270µs. Owner/org policy ratchet over the
   declared tier is the remaining unbuilt piece of this decision.

   **Consequence — this node is CRITICAL, not ignorable.** `closing` sits
   in the ignorable half because a viewer that skips it degrades to the
   timeout, which is merely a lost courtesy. A viewer that skips
   `sensitive` records the page at T0: a *fail-open* on a classification
   control, which is precisely the direction the tier model forbids
   ("unknown-but-higher tiers must fail closed — a reader lacking a key
   for level N treats it as unreadable, never as T0"). So `sensitive`
   takes a critical type code, and a viewer too old to understand it must
   refuse the document rather than render it at the wrong classification.
   The same rule binds the wire: a compositor that does not implement
   `set_tier` must not accept frames from a client that needs it.

   **Frames stay window-granular regardless.** A frame is a whole window,
   so tier contagion cannot be finer than the window no matter how precise
   the declaration is. A sub-document declaration (a single field marked
   sensitive) would therefore buy finer *transcript* classification only,
   at the cost of a tier that changes mid-page. Deferred, not rejected —
   the node carries a tier for the document it appears in, and a future
   scoped form can layer on when redaction granularity (decision 7) needs
   it. Decision 7 has since answered that question — shares cut whole
   windows and leave no trace — so the scoped form waits on P4 semantic
   identity rather than on a decision.
5. **Server shipping: ON SEAL, AND THE SERVER NEVER INDEXES (decided
   2026-08-17).**

   **When: on seal.** Content-addressing decides it. A segment's hash is
   not stable until it is sealed, and replication is promised resumable,
   idempotent and de-duplicated — three properties that hold only for
   immutable objects. Shipping an open segment means shipping mutating
   partial data and surrendering all three. The exposure window is
   therefore the seal cadence: device theft or a dead disk loses at most
   the one unsealed segment, and shortening the cadence is the knob for
   anyone who wants that window smaller (the appliance profile will).

   **Indexing: never, in v1 — the server holds pure ciphertext.** It
   stores opaque content-addressed segments and merges timelines by
   segment *metadata* (origin device, time range); search runs on a
   device that holds the key, so cross-device search means one device
   awake. Rejected: a blind index (HMAC'd token postings the server can
   match without plaintext) and a plaintext index for enterprise
   deployments.

   Rationale, and it is not primarily cryptographic. "The relay
   structurally cannot read your traffic" is a load-bearing claim of the
   personal revenue model, and a blind index would demote it to "cannot
   read it, but can see its shape" — token frequency and co-occurrence
   leak, and natural-language frequency distributions are distinctive
   enough that this is a real weakening rather than a theoretical one.
   The asymmetry settles it: an index can be added later, but it cannot
   be withdrawn once people rely on it without taking a feature away.
   Keep the strong claim while it is free to keep.

   Consequence accepted: "what was I doing at 3pm Tuesday" across
   devices needs a device online, and the server is dumb storage plus a
   metadata merge. If that proves intolerable in use, the blind index is
   the first thing to reconsider — as an explicit, named, per-owner
   opt-in, documented as the weakening it is, never as a default.

## Format design (v2 draft, 2026-08-11 — flesh before code)

Design stance on packing, stated honestly up front: the wins that matter
are **enums + varints + delta timestamps + event coalescing + zstd**.
Aggressive bit-level packing beyond that buys single-digit % *after* zstd
(which eats cross-event redundancy anyway) and pays for it in decode
complexity and fuzz surface — and this codec feeds on the machine's most
sensitive data, so strict, boring decode wins. Pack where frequency
demands it (pointer events), stay byte-aligned everywhere else.

### Event vocabulary (u8 tag; 0x00–0x1F core, 0xE0–0xFF meta; rest reserved)

```text
0x00 Sync     { wall_ms u64 }            clock correlation, ~1/min + at seal
0x01 Window   { id, title, app, vector:flags, rect }   open/retitle
0x02 Closed   { id }
0x03 Order    { count, ids... }          full bottom→top restack
0x04 Text     { id, len, utf8 }           what a frame put on screen (see below)
0x05 Frame    { id, len, bytes }         verbatim stream blob (unchanged)
0x06 Click    { win, btn, x, y, hit }     press/release ONLY — no motion
0x0D Drag     { win, from, to, kind }     one event per completed drag
0x0E Scroll   { win, axis, delta }        coalesced per gesture, not per tick
0x07 Focus    { id }
0x08 Action   { app, verb-string }       declared verbs invoked (semantic!)
0x09 Snapshot { full window set + last frame ref per window }   keyframe
0x0A Gap      { reason u8, dropped u32 } shed load, honestly marked
0x0B Scope    { pause|resume, scope }    the audit of the auditing
0x0C Capability { id, kind, granted }    grant/deny — always full metadata
```

`Rect` was specified at 0x04 and never built: a move or resize is an upsert
of `Window` by id, which carries the rect, so a second event would be a
second way to say the same thing. 0x04 is `Text`.

**`Text` is what makes decision 3 possible, and is the reason it exists.**
The transcript must be *stored*, not derived. Recomputing it by decoding the
frames means the frames can never be dropped — and dropping them at 90 days
while keeping transcripts indefinitely is precisely what decision 3 buys. A
producer already holds the decoded commands (the compositor decoded them to
display them), so it writes the text beside the frame and readers never
decode anything. Written only when a window's text *changes*, so a typing
session costs one entry per visible change rather than one per frame.

Readers fall back to decoding `Frame` when a segment carries no `Text` — for
segments written before this — and must not use both sources in one segment,
or every line is entered twice.

* **Timestamps:** per-chunk absolute monotonic base; each event a varint
  delta in ms (1 byte for <128ms gaps — the common case). Monotonic is
  the spine; `Sync` events correlate to wall clock so replay survives
  clock jumps and timezone edits.
* **Ids:** u32 per segment, varint-encoded; a fresh segment re-declares
  live windows (via the opening `Snapshot`), so segments decode alone.
* **No continuous pointer motion (decided 2026-08-11 — replaces the
  earlier 20Hz-coalesced `Pointer` event).** Only clicks, completed
  drags, and scroll gestures are recorded. Three reasons, each
  sufficient: (a) **redundant** — hover feedback is a visual change, so
  the compositor renders a frame and the frame already captures it;
  motion over dead space signifies nothing; (b) **it would break the
  idle guarantee** — a static screen with a moving mouse produces zero
  frames but would produce a steady 20Hz of events, making a quiet
  desktop write to disk, which is precisely what "idle costs nothing"
  promises it will not do; (c) **motion is biometric** — cursor
  dynamics fingerprint a user, and hesitation patterns leak deliberation
  ("moved toward Delete, paused, moved away"), which is surveillance
  telemetry a memory product has no use for.
  **Replay synthesizes the cursor** by interpolating between recorded
  click/drag points — honest labelling required: the replay cursor is a
  reconstruction, not a recording. Continuous motion becomes an opt-in
  debug/UX-research mode, off by default, and (being high-rate and
  personal) recorded at T1.
* Raw keyboard is NEVER an event type — the absence is structural, not
  policy.
* **Keyframes (`Snapshot`) every ~10s or 512 events**, video-GOP style:
  "state at time T" seeks to the nearest keyframe and rolls forward,
  never scans a segment from its head.
* **Strings** (titles, verbs): length-prefixed UTF-8, capped like every
  other codec in the tree; same fuzz treatment as stream/doc decode.

### Segment anatomy

```text
<epoch>-<seq>.rhs                       (rill history segment)
┌ header (plaintext): magic RHS\x01, format ver, device fingerprint,
│   monotonic+wall base, keyslot table (≥1 slot — decision 2), flags
├ chunk*: [len | AEAD(XChaCha20-Poly1305, nonce=chunk#) over
│   zstd(events)]           compress-then-encrypt, per-chunk auth
└ footer (present iff sealed): event count, time range, blake3 of
    plaintext, index offset, INDEX (below), seal mark
```

* **Chunked AEAD preserves the crash-honesty property**: a torn write
  costs at most the trailing chunk; every authenticated chunk before it
  decodes (`decode_lossy` at chunk granularity). Chunks flush on ~256KiB
  raw or ~5s, whichever first — that is also the fsync cadence and the
  bounded-loss window.
* **Active segments are already encrypted** (no plaintext window on
  disk); sealing only appends footer + index — no rewrite, no
  double-write amplification.
* Rotation: ~64MiB raw or hourly, whichever first. Recorder writes from
  its own thread; the compositor hands it event batches through a
  bounded queue — overflow drops batches and emits `Gap` (never blocks).

### Index design (this is the agent-speed story)

Two layers, both derived and rebuildable from segments alone:

1. **Per-segment index (in the footer):**
   * time → chunk offset table (binary-search any timestamp);
   * keyframe directory (timestamp → offset);
   * **transcript**: at seal time, frames are decoded ONCE and their
     text runs extracted into (t, window-id, text-delta) records +
     window/app/title/action tables + token postings (lowercased,
     alnum-split, delta-varint postings). Agents and search read the
     transcript; they never decode DrawCommands.
2. **Corpus manifest (`history/MANIFEST`):** per-segment row (path, time
   range, event count, sealed?) + a per-segment **bloom filter** over
   tokens. A grep touches the blooms, then only the segments that can
   match. A year of history is a few thousand segments — bloom scan is
   microseconds each.

### Performance budgets (targets, to be MEASURED like everything else)

```text
append cost            < 5µs/event amortized, zero on idle desktop
"state at T"           < 50ms   (manifest → segment → keyframe → roll)
grep, 1-year corpus    < 1s     (blooms → parallel transcript scans)
agent tail-context     < 10ms   ("last N minutes as transcript" — the
                                 hot path for LLM agency; served from
                                 the active segment's in-memory tail)
storage                MEASURED 6.1 MiB/hour at zstd -3, 3.1 at -19,
                       with a dashboard animating throughout (2.4
                       frames/s). Transcript adds ~81 KiB/hour.
                       Idle contributes ~nothing (damage-gated).
```

Compression is MEASURED, not assumed: **zstd -3 gives 11–15×, -19 gives
19–22×** on real DrawCommand streams. Level matters far more than the
trained dictionary here (+16% on a small sample, +2% on a large one),
so pick the level first and treat the dictionary as a later refinement.

The agent fast path deserves emphasis: **agents consume transcripts, not
frames.** The transcript (text deltas + windows + actions, time-ordered)
is the LLM-legible diary; raw frames exist for pixel-perfect replay and
audit. Both come from one log; neither blocks the other.

### Sensitivity tiers — classification, not omission (decided 2026-08-11)

**Supersedes the earlier app-recording-class model.** the correction:
sensitivity is a property of *what happened*, not of *who was running* —
the same notes app writes a shopping list and a recovery phrase. And
the choice is never "record or not": sensitive events are recorded into
a **compartment that requires more to open**. Classification in the
clearance sense, not deletion.

Why this is strictly better than app classes: an app-level `silent`
forced a choice between an audit hole (malware hides) and a privacy
hole (vaults leak). Tiers refuse the choice — nothing is unrecorded, so
hiding is not in the vocabulary; the only move available is *raising*
sensitivity, which makes a thing more locked, never more invisible.

```text
tier          key           replicates   agent      search index
─────────────────────────────────────────────────────────────────
T0 routine    device/owner  yes          on grant   main
T1 sensitive  subkey        NO (local)   per-read   T1 index
                                          grant
T2 sealed     re-auth key   never        never      T2 index
              (passphrase)               (also excluded from exports)
```

**Numbering direction and expandability.** Two different conventions
exist and conflating them is a classic bug: for *privilege* (what a
subject may do) lower is conventionally stronger — ring 0, uid 0. For
*data sensitivity* (how protected an object is) higher is conventional
— SELinux MLS runs s0 (lowest) upward, and classification lattices put
Top Secret above Confidential. **These tiers label data, so higher =
more protected**, matching the sensitivity convention. If accessor
clearance is ever numbered, it must use the SAME axis so the
Bell-LaPadula comparison reads naturally: a subject cleared to level N
may read labels ≤ N ("no read up").

Encoding: **tier is a `u8` (0–255), not a 2-bit field**, with 0/1/2 the
only *named* tiers in v1. Deployments get room for their own levels
(an org inserting a tier between routine and sensitive should not need
a format change), and unknown-but-higher tiers must fail closed — a
reader lacking a key for level N treats it as unreadable, never as T0.

Reserved beside it: an optional **compartment set** field (unused in
v1). A linear scale cannot express "my doctor may read medical but not
financial" — real MLS solves that with (level, category-set) labels
where dominance requires level ≥ *and* categories ⊇. Compartments are
not being built now, but the field exists from byte one, the same move
as the keyslot table: don't implement it, don't get trapped without
it.

Three mechanics make it hold:

* **Sealed events leave NO trace at lower tiers (decided 2026-08-11 —
  reversing an earlier draft).** The first draft wrote a T0 "something
  sealed happened here" marker for audit completeness. The author's
  objection stands: **metadata is data.** A T0 stream of sealed-event
  markers leaks timing, frequency, duration and clustering of exactly
  the activity that was classified — to every accessor holding T0,
  including the agent, shares, exports and the replicated server. So a
  T2 event is invisible outside its own tier: no marker, no gap
  annotation, nothing to correlate.

  **Accepted costs, stated plainly:** (a) tamper-evidence weakens — a
  hash-chained log can no longer distinguish "nothing happened" from
  "events were removed" at lower tiers (mitigation: chain *within*
  each tier, so deletion inside a tier is still detectable to a holder
  of that tier's key); (b) the enterprise/education "no blind spots"
  claim must be stated honestly as *no blind spots within the
  accessor's entitlement* — an auditor sees a complete record of what
  they are cleared for, not a complete record of the machine. Deploy-
  ments that genuinely require hole-free capture must forbid T2 by
  policy rather than assume markers will reveal it.
* **Each tier has its own transcript + postings, encrypted to that
  tier's key.** T2 text never enters the main index, so search while
  locked cannot leak it (sub-decision: does a locked search report a
  *count* of sealed hits? that count is itself a small leak).
* **Tier is temporally contagious to frames.** If a submit is T2, the
  frames of that window while the sensitive content was on screen are
  T2 as well — otherwise the pixels walk around the classification. A
  window's frames inherit the highest tier active in that window during
  that interval. (Format consequence: chunks are tier-homogeneous, so
  a tier change forces a chunk boundary.)

**Tier authority: RATCHET (decided 2026-08-11).** The effective tier is
the **maximum** claimed by (action-category default, app manifest,
owner policy, org policy). Raising is free and needs no ceremony — it
is always the safe direction. **Lowering requires an explicit owner
act, and that act is recorded at T0** ("Scope: owner lowered T2→T0"),
so the corpus explains why it contains what it contains. Consequences
worth stating: an app can protect its own actions by declaring them
high, but can never push anything *down* into the replicating,
agent-readable tier; and no compartment on the owner's machine is
permanently beyond the owner — sovereignty holds, but only through a
deliberate, audited move.

**Action categories** (the declared-verb taxonomy — also useful far
beyond history):

```text
navigate      recorded verb-only always (it's visible in frames anyway)
set-state     verb-only
submit        verb + param HASHES by default — proves what was sent
              without storing it; full params only at T0 AND with user
              policy params=full (off by default)
capability    always recorded in full metadata (grant/deny, kind,
              scope) — this IS the audit trail; content never
destructive   (delete-class verbs) verb + param hashes always, at every
              tier — the category where audit outweighs
```

**Input classes:**

* Pointer: clicks/drags/scrolls only, no motion (above).
* Keyboard: **no raw keys, structurally** — text enters history only as
  rendered frames; masked inputs therefore arrive masked.
* Clipboard: record the *event* (copy/paste, source window → dest
  window, content hash + length) — flow tracking without content.
* File picks / broker: `Capability` events (see vocabulary addition).

**Timing is a privacy dimension.** Millisecond frame timestamps during
continuous typing leak keystroke timing — a known sidechannel that
recovers typed-content statistics without any key events. Mitigation:
at **T1 and above**, frame timestamps are **quantized to 100ms** and
consecutive typing-burst frames coalesce their deltas; ms precision is
reserved for T0, where params are already recordable anyway. Cheap to
do at append time; impossible to retrofit.

Vocabulary addition for the above: `0x0C Capability { app, kind,
grant|deny, scope-hash }`. A window's *current* tier rides in the
`Window` event's flags; a tier change emits a fresh `Window` event and
(per the contagion rule) forces a chunk boundary.

### Worked example — one minute of a session, end to end

Concrete shapes, so the abstractions above have something to point at.
Sizes are illustrative but realistic.

**1. Events as written** (annotated; every field varint unless noted):

```text
tag  Δt      payload                                          bytes
──────────────────────────────────────────────────────────────────
09   0       Snapshot { win 1: "Notes — draft", app=notes,       48
                        rect 100,80,900,600, tier 0 }
05   0       Frame    { win 1, len 1840, <blob> }             1846
06   2310    Click    { win 1, btn L, @412,233, hit input#3 }    11
05   30      Frame    { win 1, len 1844, <blob> }   caret on   1850
05   1790    Frame    { win 1, len 1902, <blob> }   text drawn 1908
08   140     Action   { win 1, app=notes, verb="submit",         26
                        cat=submit, tier 0, phash 8bytes }
01   60      Window   { win 1, title "Notes — saved" }            22
00   61200   Sync     { wall_ms 1760000531000 }                    9
```

Note what is *absent*: no keystrokes (the typed text exists only inside
those two Frame blobs, as rendered runs), and no pointer motion between
the click and anything else.

**2. Segment on disk** (`0041.rhs`, one sealed hour):

```text
header    120 B   RHS\x01 | ver | device fp | mono+wall base
                  | keyslots[0]=device | flags
chunk 0    68 KB  AEAD(zstd(events t=0…312s))     ~256KB raw
chunk 1    71 KB  AEAD(zstd(events t=312…690s))
…
chunk 47   44 KB  AEAD(zstd(events …t=3600s))
footer     96 KB  count 8412 | range 14:02:11–15:01:58
                  | blake3 | merkle_root | anchor:none
                  | INDEX ↓
──────────────────────────────────────────────────────────
total     ~3.1 MB on disk   (~11 MB raw events)
```

**3. Transcript records** (in the footer, one per text change):

```text
t=4120    win 1  "Meeting notes — the TLS handshake fails when the"
t=9880    win 1  "…fails when the cert fingerprint is stale"
t=61340   win 2  "cargo test -p rill-auth"
```

**4. Postings** (the searchable part — token → where):

```text
"tls"          → [4120, 9880]
"handshake"    → [4120, 9880]
"fingerprint"  → [9880]
"cargo"        → [61340]
```

**5. Corpus manifest row** (one line per segment, always in memory):

```text
0041.rhs | 14:02:11–15:01:58 | 8412 ev | sealed | tiers{0,1}
         | bloom 4 KiB | frames:present | pinned:no
```

**6. A search, step by step** — `rill history grep "tls handshake"`:

```text
1. bloom-test every manifest row for both tokens   ~2400 segments,
   → 11 candidates                                  ~µs each
2. open those 11 footers, read postings             11 × ~1 ms
3. intersect → 3 hits, with (segment, t, win)
4. print:  14:02:15  Notes — draft   "…the TLS handshake fails…"
5. `replay 14:02:15` → seek keyframe @14:02:10, roll forward 5 s
```

Nothing in steps 1–4 decodes a single DrawCommand; only step 5 touches
frames.

### Runtime mechanics — idle, hot path, flush discipline

**At idle the recorder is not "cheap" — it is OFF.** The chain: idle
desktop → damage-gated compositor → no renders → no frames → no events
→ recorder thread parked on a blocking channel recv. Zero CPU, zero
timers, zero writes, zero syscalls. Clock `Sync` events are *lazy* —
emitted on the next real event if >60s has passed, never by a timer —
so a desktop idle overnight appends **zero bytes**. Idle memory: the
bounded queue (empty) + one ≤256KiB chunk buffer + zstd/AEAD contexts —
a few MB, fixed, forever.

**Hot path (compositor side) — the complete list of operations:**

1. Frame capture: Arc-share the stream blob the compositor already
   holds (today it stores `raw: Vec<u8>`; recording clones — switch to
   `Arc<[u8]>` so capture is a refcount bump, not a copy).
2. State diff once per tick (existing recorder design — no
   per-mutation hooks).
3. `try_send` the batch into a bounded channel. Full channel = drop
   batch, bump a counter that becomes a `Gap` event — the compositor
   NEVER blocks, allocates, compresses, encrypts, or touches a file.

That is the entire render-loop tax: a refcount, a diff, a try_send.

**Recorder thread (the only writer):** drain batches → varint-encode
into the chunk buffer (append-only, no allocation churn) → on flush
trigger: zstd-compress the chunk, AEAD-seal it, one `write(2)`, one
`fdatasync`. Flush triggers — and this is the answer to "do we
constantly write files": **no.** Writes happen only on:

```text
· chunk buffer reaches ~256KiB raw
· ~5s elapsed SINCE THE FIRST UNFLUSHED EVENT (not a periodic timer —
  no events, no deadline, no write)
· scope/pause change, segment rotation, shutdown, or signal
```

So disk activity is bursty and strictly proportional to screen
activity. Appliance/SD-card note: append-only sequential writes at
single-digit MB per active hour is the *best possible* flash wear
pattern; a year of heavy use is well under one SD endurance cycle.

**Seal (rotation) is the one heavier op** — frames decode once for
transcript + index. It runs on the recorder thread between segments (or
deferred: unsealed segments stay queryable via the slow path and **seal
when idle** — the idle-costs-nothing dividend paying for its own
indexing). Sealing is bounded by segment size and never contends with
an active desktop.

**Reads never fight writes:** the active segment's tail lives in memory
(this doubles as the agent's <10ms transcript context); sealed segments
are immutable, so concurrent reads are trivially safe. No locks span
the writer and any reader.

### SPIKE RESULTS (2026-08-11) — assumptions tested against real recordings

Measured with `crates/rill-ui/examples/history-spike.rs` (throwaway) over
two real `.rillrec` captures: a 50-frame session and a 150s session with
an animating dashboard + a static app. **These convert several PROJECTED
figures to MEASURED — and corrected two of them.**

**Found a bug first (fix before building anything on it):** window ids in
the recorder are `surface.id().protocol_id()`, which is unique only
*within a client connection*, not across clients. Two clients each
numbered a surface 7, so they overwrote each other in the recorder's map
and every sync re-emitted both as "changed": **22,784 Window events in
150s (~961 KiB, 25% of the log)** where ~20 were warranted. Also means
frames are attributed to the wrong window on replay. Fix: map `ObjectId`
→ a compositor-side monotonic u32. Sites: `main.rs:262` and `:2049`.

```text
CONFIRMED
  frames dominate            99.6% of bytes (short capture),
                             74.9% when the id bug inflates the rest;
                             ~100% once the bug is fixed
  compression is excellent   zstd -3  → 11–15x
                             zstd -19 → 19–22x
  storage rate               6.1 MiB/hour at -3, 3.1 at -19
                             (dashboard animating ~2.4 frames/s)
                             → "single-digit MB per active hour" HOLDS
  idle really is free        damage-gating means a static app
                             contributed almost no frames

CORRECTED
  transcript ratio           spec said ~1% of frame bytes.
                             MEASURED 3.3% (long) / 7.0% (short) raw.
                             Compressed: 81 KiB/hour, ~159 MiB/year.
                             Tiered decay still clearly right (30x),
                             but "forever" costs 3-7x what was assumed.
  dedup vs diffing           byte-identical frames: only 0.8–2%, so
                             hash-dedup is nearly worthless alone.
                             Commands identical to the previous frame of
                             the same window: 20–45% (naive positional
                             compare — a real aligned diff beats it).
                             ⇒ diffing is the win, dedup is not;
                             swap their priority below.
  zstd dictionary            +16% at -3 on the small sample, +2% on the
                             large one. Worth having, not urgent.
                             (Level matters far more: -19 nearly doubles
                             -3's ratio here.)
```

Year projection at this activity (8h/day × 250d): **~6 GiB/yr of frames
at -19, ~159 MiB/yr of transcripts.** So a 90-day frame window is ~1.5
GiB and a decade of transcripts is ~1.6 GiB — comfortable on a laptop,
and the appliance profile wants the shorter frame window as expected.

### Optimization strategies (2026-08-11 — not yet built, ordered by leverage)

Governing principle worth naming: **this system is idle most of the
time, and idle is free.** Damage-gating makes "doing nothing" a real,
detectable state, so every expensive operation can be deferred to it —
sealing, indexing, recompression, dictionary training, summarization.
Most systems cannot exploit this; Rill can.

1. **Frame diffing — highest leverage, and shared with P3.** Frames are
   nearly all the bytes, and **20–45% of a frame's commands are
   identical to the previous frame of the same window (MEASURED, with a
   naive positional compare — a real aligned diff does better)**. Store
   frame N as a delta vs N−1. **P3's command-list diffing/damage
   tracking wants the same primitive** — build once, two payoffs
   (compositor skips redraw, history skips bytes).
2. **Pick the zstd level deliberately.** MEASURED: -3 → 11–15×, -19 →
   19–22×. Doubling the ratio for CPU spent off the hot path (at seal,
   or on idle) is the cheapest win available, and it needs no format
   change. A trained dictionary adds +16% on small samples but only +2%
   on large ones — worth having eventually, not urgent.
3. **Frame dedup by hash — DEMOTED (was #2).** MEASURED: only 0.8–2% of
   frames are byte-identical, so dedup alone is nearly worthless. Keep
   it only as a cheap side-effect of diffing (an unchanged frame is a
   zero-length delta).
4. **Column-oriented transcripts.** Transcripts are kept *forever*, so
   their layout matters most. Split (t, win, text) into three columns —
   timestamps delta-encoded, window ids RLE'd, text contiguous — so
   like values sit adjacent and compress far better. The Parquet
   insight applied to the longest-lived artifact.
5. **Two-level bloom filters.** A per-*day* bloom above the per-segment
   ones: a year-long search touches ~365 filters and descends only into
   matching days, instead of ~2400. Hierarchical rather than linear as
   the corpus reaches a decade.
6. **Trigram index (later).** Token postings match whole words;
   "grep" implies substrings (`handshak`, `rill-auth` would miss).
   Trigram indexing is the standard fix — the difference between search
   working *technically* and working *as people expect*.
7. **Hierarchical summarization at seal (strategic).** With a local LLM
   and abundant idle time: summarize each sealed segment, roll hours →
   days → weeks. "What was I working on in March" becomes a lookup over
   ~30 summaries, not a scan of thousands of transcripts — long-range
   agent memory stops growing with history length. Runs on time that
   was already being wasted.
8. **String interning per segment.** Titles, app ids, font names, verb
   names repeat constantly; a segment-local string table with varint
   refs shrinks pre-compression size (zstd catches some of this, but
   not the parse cost).
9. **Idle-time recompression of cold segments** at higher zstd levels,
   and mmap'd footers so the page cache manages index residency.

**Graceful degradation instead of a drop cliff** (correctness-flavored):
under sustained pressure the writer should first *thin frames* (keep
all events, sample frames), then drop frames entirely, and only then
drop whole batches to `Gap`. The transcript — the part that matters
forever — survives even when perfect replay cannot.

### Format micro-decisions still open

* zstd level / dictionary (train a dictionary on DrawCommand streams?);
* transcript text-delta algorithm (full text per frame vs diff — start
  full-per-seal, measure);
* bloom sizing (bits/token) once real corpora exist;
* ~~whether `Action` carries argument hashes~~ — superseded by the
  action-category table above (hashes for submit/destructive, verb-only
  for navigate/set-state, full metadata for capability);
* ~~recording-class precedence edge~~ — resolved by the ratchet rule:
  an app may raise its own tier freely; only the owner may lower, and
  the lowering is recorded. No app gets an un-overridable floor on the
  owner's own machine.

## High-assurance profile (regulated sectors) — hook now, build later

an earlier review raised the heavyweight variant (health/gov/security, "distributed
ledger, immutable"). **Decision (2026-08-11): reserve the hook, defer
the machinery.**

The requirement decomposes into three properties, none of which need a
chain: **tamper-evidence**, **non-repudiation**, and **provable
completeness**. Per-tier hash chains + content-addressed segments +
device signatures already deliver these *against outsiders*. What they
do not deliver is evidence against the record's own custodian — the
owner holds every key and could rewrite and re-sign. Regulators care
about precisely that case.

**The standard answer is a witnessed Merkle log, not a blockchain.**
Merkle root per sealed segment → chained roots → roots published
periodically to parties the operator does not control (a compliance
officer's key, the customer's witness server, an RFC 3161 timestamping
authority). Altering the past then requires forging every witnessed
root. This is the Certificate Transparency model: proven at scale,
gives inclusion proofs ("this event is in the log") and consistency
proofs ("this log extends the one you saw last month"), costs a few
hundred bytes per anchor. A true distributed ledger solves consensus
among mutually distrusting parties with no authority — not the
hospital's problem, where an authority exists; it would buy throughput
limits and explanation burden for a property already held.

**Structural rule, non-negotiable: anchor commitments, never content.**
Immutability and deletion mandates (GDPR erasure, HIPAA disposal, court
-ordered destruction) are in direct legal conflict. Anchoring only
hashes keeps data local, encrypted and deletable: erase the data and
the commitment survives as proof that *something* existed and when,
revealing nothing and blocking no erasure. Content on a shared ledger
would make the compliance product itself unlawful in the jurisdictions
it targets.

Reserved now (~a day, zero runtime cost, no new deps):

```text
segment footer:
  merkle_root : blake3 tree over the segment's events
  anchor      : Option<AnchorRecord> { root, witness_id, sig, time }
```

Deferred until a paying customer with a real regime exists: witness
protocols and cadence, inclusion/consistency proof export, dual-control
(two-officer) unlock, WORM storage targets, retention-hold overrides
that outrank tiered decay. Noted against risks.md #1 — this is the
"generalized infrastructure for an imagined market" pattern, and the
hook is the disciplined amount of it.

## Access, visibility, and multi-device topology (2026-08-11)

The governing principle, inherited from the rest of Rill: **visibility is
a projection computed by a key-holder, never a filter trusted to a
reader.** You cannot un-see what you were handed, so no accessor is ever
handed more than its entitlement permits and told to be polite. Redaction
happens where the plaintext is; everyone downstream gets a
purpose-built view, not the corpus with a mask over it.

### Accessors and what each is permitted to see

```text
accessor            sees                                    key path
────────────────────────────────────────────────────────────────────
owning device       everything its tier policy recorded     device key
                    (the corpus, full detail)               (local)
owner, 2nd device   same corpus, merged timeline            owner key
                    (all their devices as one history)      (see below)
owner's agent       the TRANSCRIPT (text/actions/windows),  agent grant,
                    scoped to a time/app window it was       brokered +
                    granted; frames only if asked & granted  logged
shared viewer       a PROJECTION: chosen sessions/time       per-share
(human helper,      range, with per-field redaction baked    view key
 teacher, family)   in — cannot widen it                    (ephemeral)
compliance auditor  a signed, complete-within-scope export;  export key
(enterprise)        tamper-evident, not redactable post-hoc  + owner sig
relay / transport   NOTHING — opaque ciphertext + routing    no key ever
                    metadata (size, timing) only
```

Each row is a different *key*, not a different query against one key.
That is what makes "the relay carries it but cannot read it" and "my
helper sees this window but not that field" the same mechanism.

### The key hierarchy this implies (extends decision 2)

```text
device key      encrypts that device's own active segments (slice 1)
   └─ wrapped by ─┐
owner key         the human's root; can unwrap any of their devices'
                  segments. Custody = the open recovery question.
   ├─ derives ─ agent grant key   (scoped: time range + app set + tier)
   ├─ derives ─ share view key     (one projection, ephemeral, revocable)
   └─ derives ─ export key         (signed compliance dumps)
```

The device→owner wrap is why decision 2 demanded a keyslot table from
byte one: a segment sealed today by the device key gets an owner-key
slot added when the owner key exists, with no reformat. Projections and
exports are **new ciphertext produced by a key-holder**, never the
original segments with permissions attached.

### Multi-device record topology

* **Each device records its own stream** — its own segments, its own
  monotonic clock, `Sync` events tying it to wall time. No device writes
  another's history; there is no shared-writer contention across the
  fleet, ever.
* **The owner's server is the merge point, not a recorder.** Devices
  replicate sealed, content-addressed segments to it (like any Rill
  resource — the relay/backup subscription is literally this traffic).
  Content-addressing means a segment is stored once and de-duplicated;
  replication is resumable and idempotent.
* **A unified timeline is a merge by (wall-corrected monotonic) time
  across devices' transcripts — assembled by a key-holder, not by the
  server** (consequence of decision 5: the server holds pure ciphertext,
  so it can order *segments* by their metadata but cannot see the events
  inside them; the event-level timeline is built on a device that can
  decrypt). "What was I doing at
  3pm Tuesday" spans the laptop and the desk without either device
  having known about the other. Clock skew between devices is bounded by
  their `Sync` events; the merge is best-effort-ordered and labels each
  span with its origin device (honest about which glass you were at).
* **The server can index what it cannot decrypt** *only if* the device
  ships a transcript-index encrypted to a search key the server holds
  while segment bodies stay under the owner key — an explicit choice
  (decision 5), not a default. Otherwise the server holds pure
  ciphertext and search happens on a device that has the key.

### Protocol considerations (remote access / share)

History is **served, not synced-as-files**: a `rill://…/history` query
surface over the existing mutual-TLS protocol, so device identity,
deny-by-default policy, and the audit log come for free — a remote
history read is authorized exactly like a remote file read, and is
itself recorded (the audit trail's audit trail).

Query verbs (sketch, all read-only; recording is local-only, never
remote-write): `range(t0,t1)`, `grep(token)`, `at(t)` (state
reconstruction), `tail(n)` (agent hot path), `export(scope)`. Each
returns a *projection the server/source is entitled to produce for that
identity* — an unentitled field never crosses the wire, so there is
nothing to leak even under a protocol bug (the same inert-boundary
argument as documents).

**Sharing a session with another person** is the interesting new flow
and the one genuinely missing primitive:

```text
owner device: pick session/range → choose redaction (fields, apps,
  timing precision) → produce a PROJECTION segment (new ciphertext) →
  wrap it to a share view key → hand the recipient the key (out of
  band, or grant their enrolled device via policy). Revocable by
  dropping the grant; the recipient never held the corpus key, so
  revocation is real, not hopeful.
```

Semantic redaction is the payoff pixels can't touch: "show my helper
this window with the password field blank *for them*" is a projection
that re-renders the frame with that field's runs omitted — impossible
when the artifact is a bitmap, trivial when it is commands. Live
share (streaming a redacted projection in real time) is the same
projection applied to the active tail instead of sealed segments —
noted as a later slice; it needs the redaction engine the offline
share flow builds first.

### New open decisions from this section

6. **Owner-key custody: PASSPHRASE + PRINTED RECOVERY CODE (decided
   2026-08-11).** The owner key is never stored — it is *unwrapped* by
   whatever fills a keyslot (the LUKS model), so unlock methods are
   pluggable and additive; this decision picks only the FIRST one.

   ```text
   keyslot 0: device key      (slice 1, already decided)
   keyslot 1: passphrase      Argon2id(pass, salt)
   keyslot 2: recovery code   printed once, offline, fire-safe
   keyslot 3: (empty — future hardware token / TPM)
   keyslot 4: (empty — future device quorum share)
   ```

   Rationale: works day one on one device, nothing to buy, works
   headless (Pi over SSH), degrades gracefully via the printed code, and
   is explainable in a sentence. Hardware token and M-of-N device quorum
   (the philosophically native option — it reuses the fingerprint trust
   graph) become later slots with **no re-encryption**, once a real
   fleet exists to design against. Accepted consequence, stated plainly
   in the UX: lose both passphrase and printed code and the corpus is
   gone — there is no company in the middle, which is the point.
7. **Share redaction: WHOLE WINDOWS, AND THE OMISSION LEAVES NO TRACE
   (decided 2026-08-17).** Big cuts, not fine ones — and the recipient must
   not be able to tell that anything was cut. the framing: *a secret is
   only kept if the fact of its existence was kept too.*

   This is decision 1's sealed-event rule applied to shares, and the
   argument transfers unchanged: **metadata is data.** A share that said
   "a window was here, redacted" would leak the timing, duration and
   clustering of exactly the activity that was withheld — to the one
   party the withholding was for. So an excluded window does not appear
   as an omission; it does not appear.

   Field-level redaction was rejected as the *starting* granularity for a
   reason worth keeping written down: redaction is a promise, and the two
   granularities fail differently. "I only shared the terminal" is a
   promise you can inspect and keep. "I removed the password" that caught
   nine of its ten appearances is worse than the coarse version, because
   everyone involved now believes the share is clean and stops looking. A
   correct field-level cut needs to know that a run in one frame is the
   *same* run as in another — P4 semantic identity, unbuilt — and doing
   it by text matching is approximate in both directions (misses a
   re-wrapped or scrolled variant, catches the same word elsewhere).

   **What "no trace" costs, enumerated — because it is stronger than
   "excluded" and each one is a real residue:**

   ```text
   window ids     the projection renumbers from 1; a gap in the id
                  sequence would itself be the marker
   Order events   rebuilt over included windows only, never filtered
                  copies of the originals
   Pointer        clicks/drags carry coordinates: one over an excluded
                  window betrays its position and existence, so pointer
                  events are dropped unless they land in an included one
   occlusion      NOTHING TO DO — see below
   timing gaps    accepted; they are ambiguous by nature (see below)
   structure      the projection is new ciphertext built fresh, so no
                  segment or chunk boundary survives to be read
   ```

   **Occlusion is where the architecture pays off, and it is worth
   naming.** A screen recording cannot do this at all: remove a window
   from a composited video and you are left with a hole showing whatever
   was behind it, or a rectangle of the wrong pixels. Here each window is
   its own DrawCommand stream, composited only at replay — so a window
   that was *on top of* the one you are sharing never touched its frames.
   Delete it and the remaining stream is complete and correct, with
   nothing to reconstruct and no hole to explain. Removing a window from
   the record is a thing only a vector-native history can honestly do.

   **Timing gaps are accepted rather than papered over.** An excluded
   window's activity leaves a quiet stretch in the share. That is
   tolerable because it is *ambiguous*: a gap is indistinguishable from
   the owner being idle, thinking, or in some other application that was
   never part of the share's scope. Re-basing time to close the gaps was
   rejected — it would destroy "when did this happen", which is most of
   what a share is for, to buy nothing against a recipient who cannot
   distinguish the cases anyway.

   Deferred, not rejected: field-level redaction as a *second* operation
   once P4 lands and the claim can be made honestly. The vocabulary for
   it is decision 4's `sensitive` node, which is why that decision picked
   a document-level declaration rather than a manifest number.
8. **Cross-device clocks: INDEPENDENT, WITH A DETERMINISTIC TOTAL ORDER
   (decided 2026-08-17).** Devices never discipline to the server's clock.
   Decision 5 just made that server a blind ciphertext store, and handing
   it authority over *when your own audit trail says things happened*
   would be a strange power to grant something deliberately kept from
   reading the contents. Each device keeps its monotonic counter and
   writes `Sync` events pairing it to the wall time it believes; the merge
   corrects with those.

   **The merge sorts by `(corrected_time, device_fingerprint)`** — one
   ordered list, each span labelled with the machine it came from, exact
   ties broken by fingerprint. the call, and the property that decides
   it is **determinism**: any two people merging the same segments get an
   identical timeline. For a record whose job is to be argued from, a
   reproducible answer is worth more than an annotation about concurrency,
   and the fingerprint is already a stable globally-unique key so the
   tiebreak needs no naming convention. Rejected: a partial order that
   refuses to sequence close events (a second concept in the replay UI,
   paid for in every session, to be right about a case that barely
   arises).

   **The honest limit, recorded so nothing later overclaims.** The
   tiebreak covers exact equality, which is rare. The real hazard is two
   events 50 ms apart on machines whose clocks are 200 ms out: those are
   not ties, they sort confidently the wrong way, and the device label
   says "different machines" without saying "so this ordering is
   unreliable". Accepted because cross-device causality at sub-second
   scale is close to nonexistent in practice — but **a merged timeline
   must never be presented as proving the order of events from different
   devices.** Within one device the order is exact; across devices it is
   a best-effort presentation. Anything needing true cross-device
   causality would need a real logical clock, which is a different design
   and not worth its weight for this.
9. **Agent scope: STANDING T0 TAIL, GRANTS FOR THE REST (decided
   2026-08-11).** The agent continuously reads a rolling recent window
   (default 30 min, configurable) of **T0 transcript only** — no
   prompt, so "what was I just doing" is instant. Everything else is a
   brokered, logged grant: older T0 ranges, any T1 content, and frames.
   T2 is structurally unreachable regardless of grants. The standing
   window is declared in policy and its bounds are recorded, so "what
   can the agent see" has a precise, checkable answer rather than a
   vibe. The tier model is what makes standing access tolerable: it is
   standing access to the *routine* stream, not to your history.

## Observation boundary — who writes history (decided 2026-08-11)

The gap this closes: the compositor is the only party that sees every
window and frame, which is why the recorder lives there — but it
**cannot observe** the verbs an app invoked (`Action`), capability
grants (`Capability`), or an event's sensitivity tier. Those are
client-side knowledge. Tier is the urgent one: contagion requires the
tier to be known *when the frame arrives*, not retroactively.

**Decision: one writer, clients declare over the stream protocol.**

```text
compositor observes (unforgeable, no client can suppress it):
    presence, geometry, stacking order, frames, clicks/drags/scrolls
client declares (about ITSELF only, over rill_stream_v1):
    set_tier(n)     latched with the next frame, like attach
    semantic(blob)  Action{verb, category} | Capability{...} | Scope
owner policy:
    may always RAISE a declared tier (the ratchet); only the owner
    lowers, and the lowering is recorded
```

Why self-reporting is acceptable here — the trust analysis, stated so
it is not re-litigated later: a lying client can misclassify only **its
own content**, which it authored and already fully controls. It cannot
touch another window's records, and it cannot hide, because presence,
geometry and frames are observed independently — a client that declares
nothing still appears in the timeline with its frames. This is the same
split the rest of Rill uses: the server authorizes, the client renders,
neither is trusted with the other's job.

Rejected: **per-process logs merged at read** (N writers, N keys, N
indexes; a client crash loses its own tail; frames and actions would be
correlated by wall clock *across processes* — fragile exactly where the
audit story must be solid) and a **history daemon** (a new process and
new IPC for verification value that only matters once multi-party
attribution is a product requirement).

Protocol work this implies (rill_stream_v1, additive):

```text
request set_tier(uint tier)        latched at commit with the frame
request semantic(fd, uint size)    an encoded semantic-event blob,
                                   same memfd discipline as attach
```

Pixel clients (alacritty and friends) declare nothing and stay
presence-only — which is what they already are, and remains the honest
rule: *what the semantic layer cannot see, history does not keep.*

**Sequencing note:** slice 1 may ship the compositor-observed subset
first (frames, windows, clicks; everything T0 except compositor-side
app exclusions, which work because the compositor knows `app_id`). The
storage topology is settled by this decision, so adding the declared
channel later is additive — no reformat, no rewrite.

## First slice (demo-gated, proposed)

**"Durable history + grep":** segmented writer (rotate + zstd + seal +
signal handler), always-available service mode, seal-time text index, and
`rill history` CLI: `list`, `grep <text>` (→ timestamp + window title),
`replay <t>` (opens rill-vector --replay seeked). Demo: type a phrase you
saw yesterday, be looking at the moment it appeared in under a second.
Everything downstream (agent context, compliance export, teacher view)
consumes this substrate.
