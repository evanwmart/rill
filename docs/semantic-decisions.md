# Semantic decisions — the path to AI-assisted computing on Rill

Status: **direction and a phased path. Nothing built. Written
2026-09-24** from the Semantic Decision Layer proposal (Laya as first
backend) after red-teaming it against the position, the ladder in
[risks.md](risks.md), and the cost budget in
[semantic-plane.md](semantic-plane.md). Companion to
[specs/history.md](../specs/history.md) (the substrate most of the
differentiation rests on) and [specs/knowledge.md](../specs/knowledge.md)
(where the first slice ships).

Numbers are labelled MEASURED (dated, box named), PROJECTED, TARGET, or
VENDOR. The proposal itself is filed beside this doc as the reference
architecture; this doc is about what to build, in what order, and why a
stranger would care.

## 1. The position this serves

Set 2026-09-24. The ladder is unchanged.

* **The person:** a developer-student who uses the machine to develop and
  study in a seamless, freeing way, free from big tech and from AI that
  acts on their behalf.
* **The differentiator:** smaller and cheaper hardware, greater
  performance and energy efficiency, one wire reaching many screens.
* **The ladder:** 1 Pi works · 2 week-long run · 3 bridge feeds a useful
  display · 4 trivial second-device enrollment · 5 a stranger sets one up
  · 6 they keep using it · 7 they want another · 8 someone pays.

"AI-assisted" therefore has one narrow meaning here: **a small local
model interprets; Rill decides; the model never acts.** It answers
bounded questions — which of these, how much, is this true — with typed
probabilities. Deterministic code and the capability broker decide what
runs. No model generates a command, names an identifier it was not
handed, or holds authority over a device. Anything generative is a
separate, user-invoked tool, not a router's decision.

## 2. What Rill has that nobody else does

Every other desktop that wants an assistant has to *look at pixels*:
screenshot, OCR, guess the UI, click coordinates. That is why Recall was
a scandal and why agents on macOS and Windows are brittle. Rill's
substrate makes the semantic layer read **typed state the system already
has**, and that is the whole differentiation. Five properties, all
shipped or specified:

1. **The window is a document, not a bitmap.** A vector-native window is
   a DrawCommand stream with hit-regions (`LinkArea`, `ActionArea`,
   `InputArea`) and an ordered `focusables` list. What is on screen, what
   is clickable, what it is called and what it does are all in the stream
   the compositor renders. A decision model reads *that*. No OCR. The
   north star already says it: agent interface == accessibility tree.

2. **History is a transcript, not screenshots.** The recorder writes
   `Text` events beside frames — the visible text of each window when it
   changes — and the seal builds a token index with bloom filters per
   segment (history.md, "Index design"). "The terminal where the build
   failed yesterday" is a bloom scan and a posting lookup that yields
   *candidates*; the model only picks among them. Frames can replay the
   exact window. Nothing is inferred from pixels because the pixels were
   never the record.

3. **Sensitivity is declared, tiered and keyed.** A document says
   `sensitive tier=N`; the history stores tiers as a `u8` and the accessor
   table (history.md, "Accessors") gives an *agent* only the transcript,
   scoped to a granted time/app window, under a brokered, logged grant.
   The decision layer is an accessor with a tier ceiling. A competitor
   cannot offer "the model never saw your banking window" as a property
   of the log; Rill can, because the app declared it and the key path
   enforces it.

4. **Apps declare their verbs.** Actions are typed (`ActionValue`, 32
   fields, named paths) and authorised *before* the handler runs. Intent
   routing is therefore a **closed choice per app**: the model picks
   among the verbs the app declared, and nothing else is expressible.
   "It cannot do what the app didn't declare" is not a policy; it is the
   protocol.

5. **The hub decides, the glass shows.** The model does not have to run
   on the device that displays the result. A decision service is an app
   server on the one capable box; its outputs are documents; the wire
   carries kilobytes to a Pi, an e-ink panel, a kiosk. This is what
   reconciles a 400M-parameter model with the cheap-hardware
   differentiator: the appliance never hosts the model and still gets the
   behaviour.

And one property that falls out of "everything is a document":
**every decision can render as a page.** Candidates, probabilities, the
policy that gated it, the tier that hid something. Auditable assistance,
by construction, on the same viewer as everything else.

## 3. What would make a stranger look

The proposal's demos are desktop conveniences ("move the terminal to the
left monitor"). Useful, not shareable. The demos below are the ones that
use §2 and that a stranger would forward, in the order they become
buildable.

| Demo | One line the stranger repeats | What it rests on | Rung |
|---|---|---|---|
| **Recall done right** | "Ask your computer what you were doing, and it never took a screenshot." | history transcript + bloom index + tiers + replay | 3, 5 |
| **The explain page** | "Every AI decision on this desktop is a page you can open." | decisions as documents | 5 |
| **Study from what you read** | "It made flashcards from the pages I actually had open this week." | history `Text` + knowledge chunks + judge | 3 |
| **Talk to the glass** | "One sentence, and the kitchen screen changed." | hub decides, kiosk shows, wire | 3, 4 |
| **Better answers** | "Search that knows whether the passage answers the question." | rerank stage in knowledge-app | 3 (weak) |

The first is the differentiator made visible. Recall's failure was a
privacy story; Rill's version is the opposite story told with the same
words, and the tier table is the proof. It is also the demo that needs
the least new substrate: the recorder, the transcript, the index and
replay all exist; what is missing is the query path and the judge.

## 4. The path

Seven phases. Each has a gate, a demo, and a labelled cost. Nothing in a
later phase starts before the earlier gate is measured, and every phase
keeps [semantic-plane.md](semantic-plane.md)'s three budgets: clients
never link a model, no frame leaves the box, the server-side footprint is
one process with a measured RSS.

### Phase 0 — Benchmark (one week)

Decide with numbers whether the small models are enough. Three cases,
200 examples, four backends.

* **Cases.** (a) Retrieval rerank: 120 questions over the live Simple
  Wikipedia pack, fused top-20 chunks labelled contains-answer yes/no
  (extends the `eval` command's 12 hand questions). (b) Window reference:
  80 utterances against real window lists from the compositor, each with
  a correct id or "none" and an ambiguous flag. (c) **History recall:**
  60 queries against recorded sessions ("the terminal with the build
  error", "the page about seat activation"), candidates produced by the
  transcript index, correct window+time labelled. Case (c) is the
  differentiating demo and is why this benchmark is three cases and not
  the proposal's two.
* **Backends.** Rules; bge-small cosine over option labels (33M, shipped
  on burn); a MiniLM-class cross-encoder (22M, port needed); Laya through
  its own HTTP mode as a sidecar for the week, linked by nothing.
* **Metrics, all MEASURED on the workstation and the Pi 5:** top-1,
  recall@5 after rerank vs before, calibration error, false-execute rate
  at a 0.9 gate, correct abstain on ambiguous/none, p50/p95 latency,
  resident memory.
* **Gates, set before running.** Rerank ships if any backend lifts
  recall@5 by ≥10 points at <100 ms p95 on the workstation CPU. History
  recall proceeds if top-1 ≥80% with correct abstain ≥90%. Window
  reference stays parked unless false-execute <2% at ≥80% coverage. Laya
  is adopted for a profile only if it beats the best small model on a gate
  by a margin that survives its memory on that profile.

Reference numbers today: bge-small 14 ms GPU / 34 ms CPU on the
workstation (MEASURED 2026-09-21); Laya 421M params, 33 ms (VENDOR,
hardware unstated); Laya on a Pi 5 CPU 0.5–2 s and 0.5–1 GB (PROJECTED,
~13× the embedder).

### Phase 1 — `rill-decide` and the rerank stage

* `crates/rill-decide`: sans-I/O trait in the `rill-protocol` discipline.
  Three question kinds — `Choice`, `Score`, `Probability` — typed
  responses with per-option probabilities, no backend vocabulary in the
  types. Backends are separate crates (`rill-decide-rules`,
  `rill-decide-embed`, later `-cross`, later `-laya` if it earned it),
  the way `rill-knowledge-embed` sits beside `rill-knowledge`.
* Rerank in `rill-knowledge`: the engine takes optional per-chunk
  judgements from its host exactly as it takes the query vector today,
  reorders the fused top-20 by them, keeps RRF as tiebreak, and leaves the
  order untouched when the judge is absent or slow. `knowledge-app`
  supplies judgements behind the same model-hash gate it already applies
  to vectors. The pack format, the wire and every client are unchanged.
* **Demo:** the search page on :7450 answers the 12 hand questions with
  the answer in the top 3, and the study cards built on those chunks are
  visibly better.
* **Cost:** one small model resident in knowledge-app (~100 MB,
  PROJECTED), 20 judgements per query. TARGET p95 <100 ms added.

### Phase 2 — Decisions as an app server, with explain pages

* `apps/decide-app`: the decision service as an ordinary Rill app in the
  `notes-app` shape. Requests are `ACTION`s with typed fields (a question
  is a path, its options are fields — 32 fields is the ceiling, so
  hierarchical choices are two round trips, which is what the proposal's
  §11 wanted anyway). Responses are documents. Every response is also a
  retained **explain page**: state given, candidates, probabilities,
  backend, latency, the policy that gated it.
* It is unprivileged, restartable, holds the model, and is the *only*
  process that does. Consumers (knowledge-app, history search, later the
  shell) are clients of it over the wire. This is the proposal's process
  model realised with zero new IPC.
* **Demo:** open any answer's explain page from the search results.
  Auditable assistance on the same viewer as everything else.
* **Cost:** one process; measured RSS in [memory-footprint.md](memory-footprint.md) before it is allowed to stay resident. Idle unload after N minutes is a config knob, not a promise.

### Phase 3 — Recall done right

* Query path over the history corpus: bloom scan → segment transcript
  postings → candidates (window, title, app, time, text delta) →
  decide-app picks or abstains → the frame replays in a vector window at
  that timestamp. Deterministic candidates, model choice, exact replay.
* Tier enforcement is not new code: decide-app is registered as an
  *agent accessor* with a tier ceiling (T0 by default; T1 only under a
  brokered grant the user sees; T2 never). What the document declared
  sensitive is not in the candidate set, and the explain page says so.
* Runs on the hub; the result page shows on any screen. The Pi glass can
  ask "what was on the build terminal at 3 pm" and show it.
* **Demo:** the one in §3. The pitch line writes itself and the tier
  table is the receipt.
* **Gate:** Phase 0 case (c) numbers, plus a written threat note in
  [security.md](../specs/security.md) terms: what the accessor can and
  cannot see, enforced by which key.

### Phase 4 — Reference resolution, read-only first

* "Focus the terminal with the build", "raise the browser." Candidates
  from the compositor's window list, choice from decide-app, action only
  from the closed set the target declares. Read-only verbs (focus, raise,
  scroll-to) first; move/close only after a second measured pass.
* **Gate:** Phase 0 case (b) false-execute <2% at the 0.9 gate; anything
  below the gate asks the user, and asking *is* the interaction, rendered
  as a document.
* No voice. Text intent from a launcher field. Voice adds a second model
  and a second energy budget and waits for rung 5.

### Phase 5 — Talk to the glass

* Multi-screen intent: "put the standby list on the kitchen screen."
  Targets are kiosk surfaces the hub already knows (they are its
  clients); the verb set is tiny (show, clear, rotate). Hub decides, glass
  shows. This is rung 3 and 4 with a sentence instead of a config edit.
* **Gate:** a second enrolled display exists (rung 4). Until then this is
  a north-star line.

### Phase 6 — Only if the numbers ask for it

* A burn port of ModernBERT for a native Laya backend, if Laya won a
  profile in Phase 0. One stack, no Python, no ONNX.
* A Rill-specific checkpoint, once decide-app's explain pages have
  accumulated thousands of *local, consented* decisions with outcomes —
  the dataset the proposal's §25 wants, collected as a side effect of use
  rather than written by hand.
* Generative escalation as a user-summoned local tool, never a
  router-summoned one, and only if the position changes to allow it.

## 5. Budgets and gates (extends semantic-plane.md)

| Budget | Rule | How it is checked |
|---|---|---|
| Client | No client links a model or an inference runtime. Clients render documents. | `cargo tree` in the pre-push hook: no ML crate under any `platform/` or `apps/` binary except `decide-app` and `knowledge-app`. |
| Network | No frame, transcript or context leaves the box. Decisions cross the wire as documents; requests as ACTIONs with the 1 MiB cap already on them. | decide-app binds loopback by default; a remote profile is an explicit later decision. |
| Server | One model-holding process. RSS and idle CPU measured before it may stay resident; unload is a knob. | HUD per-process sampler already exists; entry in memory-footprint.md per phase. |
| Energy | A decision costs less than the action it saves. TARGET: p95 <100 ms workstation, <500 ms Pi for anything that reaches an appliance. | Phase 0 harness on both boxes; re-run per backend change. |
| Safety | Confidence is never authority. Every user-facing action has a declared gate and an explain page. | false-execute rate is the release metric; a phase does not ship without it measured. |

## 6. What this deliberately is not

* Not screenshots, not OCR, not pixel-clicking. If a future feature needs
  any of them, the substrate has failed and that is the bug to fix.
* Not a cloud call. Hybrid profiles are a later, explicit decision.
* Not a generative default. Nothing writes prose or code on the user's
  behalf unless the user opened that tool.
* Not a new IPC, a new runtime, or a Python service in the product.
  Phase 0's sidecar is a measurement instrument and is deleted after.
* Not autonomous. No persistent agent, no background planning, no action
  without a declared verb, a gate, and a page that says why.

## 7. Open decisions (Evan's)

1. **What "free from AI" permits.** A local classifier that only
   interprets, or no model in the loop at all. Everything above assumes
   the first.
2. **Three benchmark cases or fewer.** Rerank alone is three days and
   answers the shipping question; window reference adds the safety
   number; history recall adds the differentiating demo. All three is a
   week.
3. **Laya as a sidecar for the benchmark week, or excluded.** Its HTTP
   mode keeps Python out of the workspace but not off the box.
4. **The hub.** Which box is "the one capable box" in the appliance
   story — the workstation, a mini-PC from the reference-hardware list, or
   a Pi 5 running the 22M model only. This decides what Phase 3 demos on.
5. **Where escalation sits, if anywhere.** User-summoned local, or
   nothing.

Until 1 and 2 are answered nothing here is on the roadmap. When they
are, Phase 0 starts the same day, because its harness is the `eval`
command plus labels.
