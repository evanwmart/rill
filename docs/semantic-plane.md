# The semantic plane — facts, views, and the cost budget

Status: **direction and one decision record. Nothing built.** Written
2026-08-13, out of a design conversation about what Rill is underneath the
desktop. Companion to [risks.md](risks.md) (how not to die),
[resource-envelope.md](resource-envelope.md) (what it costs today) and
`specs/appliance.md` (where the hardware story goes).

Two things live here, and they are here together on purpose:

1. **One foundational boundary** — the split between *facts* and *views* —
   which has to be decided before typed resources are implemented, because
   everything built afterwards calcifies around it.
2. **A cost budget** with gates, because the entire value of this direction
   evaporates if adding a semantic layer quietly adds bandwidth, client
   weight, or server state. The budget is the constraint that makes the
   feature list safe to pursue.

Nothing below is a spec. Where something is genuinely undecided it says so
rather than picking by implication.

## Why this document exists now

The direction under discussion is roughly: *a Rill server should expose not
just pages, but a small, typed, capability-scoped semantic surface —
resources, queries, actions, streams.* That would underpin native-feeling
apps, collaboration, agents, corporate tooling, remote systems, plugins and
odd physical devices without putting a line of foreign code on the client.

The reason to write it down before building is that reading the tree shows
**most of this is formalization, not greenfield**:

* `live target="/ascii/fit/{w}x{h}"` is already a client substituting
  parameters into a server-declared template. That is a parametric resource
  with no types on it.
* `ActionValue` (`Str | Num | Bool`) with `MAX_ACTION_FIELDS`,
  `MAX_FIELD_NAME` and `MAX_FIELD_STRING` is already the type vocabulary and
  its bounds.
* `AppHandler` (`get(path, identity)` / `action(path, fields, identity)`,
  authorized *before* the handler runs) is already the connector interface —
  "plugins are servers" is the existing architecture, not a proposal.
* `When { state, invert, child }` over typed state slots is already
  conditional rendering against client-held state.
* `GET_IF` already carries a hash for conditional reads. Pointed the other
  way, that is compare-and-swap.
* The wire already has a version byte, a critical/ignorable flag split that
  rejects unknown *critical* flags, and an append-only `METADATA` struct.
  Extension is designed for.

Formalizing something already half-present is cheap. Deciding its
foundational boundary *after* four consumers depend on it is not.

## The three planes

```text
                 semantic plane
        ┌────────────────────────────┐
        │ resources / facts          │   authoritative
        │ queries                    │   externally observable
        │ actions                    │   principal-scoped
        │ streams                    │
        └─────────────┬──────────────┘
                      │
                 view plane
        ┌─────────────▼──────────────┐
        │ Rill documents             │   presentation + interaction
        │ ephemeral view state       │   per client/session
        │ staged (proposed) facts    │
        └─────────────┬──────────────┘
                      │
              presentation sinks
       ┌──────────────┼──────────────┐
     screen          CLI           agent
     e-ink          speech       automation
```

The invariant the whole direction rests on:

> **Facts are authoritative. Documents are views of facts. Not every
> consumer renders a document.**

An agent, a CLI, or an automation consumes the semantic plane directly. A
screen consumes a document. Neither is privileged, and there is no separate
agent protocol — an agent is an ordinary capability-scoped principal.

## The decision: facts, views, and the thing in between

The split is not two categories. It is three, and the third is the one that
gets missed.

### Facts — resource state

Authoritative, externally observable, owned by a server, versioned by hash.

```text
task.status          build.result        incident.owner
machine.temperature  document.revision   session.participants
```

Any principal may read these (subject to policy), and they mean the same
thing to every consumer. If two consumers disagree about a fact, that is a
bug.

### View state — ephemeral, per session

Belongs to one client. Never leaves it. Never becomes part of the semantic
world.

```text
selected_tab      expanded_section    modal_open
table_sort        scroll_position     hover target
caret / selection focus ring
```

The existing `When` / `SetState` / `Toggle` machinery and the viewport's
caret and selection tracking belong here, unchanged. Resources arriving does
not mean ripping any of it out.

### Staged facts — the middle category

Here is the part that is easy to get wrong. `Submit { endpoint, fields }`
draws its fields **from state slots**. So a slot holding a half-typed form
field is view state right up to the moment it becomes the payload of a
mutation — at which point it was always a *proposed fact*.

Calling that "ephemeral view state" would be wrong in three places:

* **Offline queuing.** A queued action is queued staged slots. They need
  durability semantics; `scroll_position` does not.
* **Agent parity.** An agent should inspect and fill the same staging area a
  human does. If staging is invisible to the semantic plane there are two
  input paths, and they will diverge.
* **Compare-and-swap.** The expected revision should be captured when
  staging *begins*, not when Submit is pressed — otherwise a form filled
  over five minutes silently overwrites five minutes of other people's work.

So: `selected_tab` is ephemeral. A half-filled form is not. Name the
difference in the model rather than discovering it in a bug.

### The bonus this earns

If documents are views over facts, the render cache key becomes

```text
hash(template) + hash(facts) + viewport
```

which is a cleaner derivation of the deterministic-memoization thesis than
"same document, same theme, same size." The split makes the caching argument
*more* rigorous, not less. Worth remembering when the split looks like extra
work.

## The ontology — and why not "everything is a resource"

Plan 9's uniformity was its beauty and its ceiling: things that were not
file-shaped got awkward encodings. "Everything is a semantic resource" will
feel exactly the same pressure, and a terminal is where it starts — a
terminal is a stream with an interpreter, not a record with fields.

Three shapes, which is still a very small budget:

| Shape | Is | Interaction |
|---|---|---|
| **Resource** | current state, hash-versioned | snapshot; act on it |
| **Stream** | ordered temporal values | subscribe from position N; append |
| **Blob** | opaque or externally interpreted bytes | fetch, ranged, streamed |

A terminal stops being a contortion:

```text
terminal/session/42            Resource   cwd, title, dimensions, status
terminal/session/42/output     Stream<Bytes>
terminal/session/42/input      Action(write bytes)
                               Action(resize cols, rows)
```

Video likewise: a Blob for bytes, a Resource for metadata, Actions for
transport controls. **Media never travels through the semantic plane.** The
document declares a media region with a content hash; the compositor —
trusted, native, ours — fetches and decodes. The codec lives in our stack and
is never downloaded, so the inert-client property survives contact with
video. The dmabuf milestone is already the plumbing for it.

Queries and Actions are operations over these shapes, not a fourth and fifth
shape. **Intents are not a concept** — an intent that eventually invokes
something is a discoverable Action with a description. Three concepts is the
budget; a fourth needs to fight for its place.

## The cost budget

This is the section that matters most, and the one most likely to rot,
because bloat never arrives as a decision. It arrives as fifteen reasonable
additions.

The premise: **every capability below must be added without raising idle
bandwidth, per-app memory, or per-connection server state.** If a feature
cannot be built inside the budget, the feature is wrong, not the budget.

### Where we are today (MEASURED, 2026-08-13, release)

```text
per extra app window      3.6–4.6 MiB PSS
app server process        7.4 MiB PSS, 3.1 MiB binary
idle desktop CPU          0.97% of one core, 1.13 fps
idle frames               41 of 48 were the 1 Hz heartbeat
content cache growth      0.00 MiB/min (bounded, swept)
wire bytes                NOT MEASURED — the outstanding gap
```

### Proposed budgets (TARGET — ratify once the wire harness exists)

These are engineering goals, not predictions, and deliberately stated before
the numbers exist so they cannot be retrofitted to whatever we happen to get.

```text
NETWORK
  idle client, nothing changing      < 1 KiB/min      (keepalive only)
  one user interaction → new view    < 8 KiB
  live value at 1 Hz, steady state   < 200 B/s        (delta, not snapshot)
  schema, per capability set         fetched once, immutable, cached forever

LOCAL
  per additional app window          ≤ 5 MiB PSS      (hold the measured 3.6–4.6)
  client cache                       ≤ 64 MiB, swept  (already enforced)
  per subscription, client side      bounded queue, O(1) in resource size

SERVER
  base process                       ≤ 10 MiB PSS     (hold the measured 7.4)
  per subscription                   < 4 KiB of state, O(1) in resource size
  per connection                     bounded; sheddable under pressure
```

### The rules that keep the network budget

1. **Idle must cost nothing.** The single most important property. Today it
   is impossible — a live widget polls ~12×/s — and SUBSCRIBE is what makes
   it achievable. Any addition whose steady-state cost is nonzero *when
   nothing has changed* must justify itself explicitly.
2. **Every new frame type states its idle cost** in its spec text. If the
   answer is not "zero bytes when nothing changes," say why.
3. **Deltas against a known hash, never full snapshots.** Content addressing
   gives the baseline for free; re-sending a whole document because one field
   moved is the failure mode.
4. **Bandwidth scales with presentation, not with data.** Generalize
   `/fit/{w}x{h}`: a chart 300px wide requests ~300 aggregated points, not
   100k rows. The client declares its constraint; the server aggregates. Most
   dashboard systems get this backwards and ship the dataset.
5. **One round trip per user intent.** X11 died over the WAN on chattiness,
   not on bytes. A request that requires a follow-up request to be useful is
   a design error.
6. **Schemas are immutable and hash-addressed**, so their recurring cost is
   zero after first fetch.
7. **SUBSCRIBE must reduce total bytes, and be measured doing so.** If it
   lands and idle traffic goes *up*, it was implemented wrong.

### The rules that keep the local budget

1. **The client stays inert.** No expression language, no scripting, no
   downloaded code. This is not negotiable; it is the security claim.
2. **The local view algebra, if it happens at all, is closed, shallow and
   non-composing.** A fixed set of about ten operations over one collection —
   `sort`, `filter_field`/`filter_value`, `limit` — not generic operators
   that nest. Composition is what turns a config format into XSLT.
3. **The server must be able to produce the identical result.** Local
   evaluation is then a *performance optimization*, never a semantic. This is
   the safety valve: without it, a Braille renderer, a CLI, or an agent that
   does not implement the algebra sees different content than the human, and
   the one-semantic-reality thesis — the best idea in the whole direction —
   is dead. It is also testable:
   `server result hash == locally derived result hash`.
4. **A renderer must be correct without implementing the optional parts.**
   That is what keeps e-ink, speech, Braille and CLI sinks cheap.
5. **No unbounded client state.** Subscriptions get bounded queues and a
   defined behaviour when they overflow.

### The rules that keep the server budget

1. **Per-subscription state is O(1) in resource size** and small in absolute
   terms. A subscription is a cursor plus a filter, not a materialized view.
2. **No unbounded maps.** The existing `HashMemo` — a `HashMap<PathBuf, …>`
   with no eviction — is bounded only by how many distinct files get served.
   That is acceptable today and a bad precedent to copy. Any dedup or
   subscription table gets a bound and an eviction rule from day one.
3. **Re-authorize on every emitted event.** `Policy::authorize` is a linear
   walk over a small parsed `Vec` — nanoseconds, entirely in memory. That
   buys *immediate* revocation with no lease lifecycle, no renewal traffic,
   and no per-subscription auth cache. Leases are the answer when
   authorization is expensive (a DB or a network hop); if enterprise identity
   bridging ever makes it so, revisit then. Not before.
4. **The server may always refuse.** A valid request is not an admissible
   request; authorization and admission are separate checks (see below).

### The gate

A budget with no gate is a wish. Same discipline as
`docs/memory-footprint.md`:

* **Extend `scripts/bench-stack.sh` to record wire bytes** before any of this
  is built. It is already the single highest-value missing measurement, and
  it is the only thing that can enforce the network budget.
* Establish a baseline table: idle bytes/min, bytes per interaction, bytes
  per live widget per minute.
* **No protocol addition merges without before/after numbers in that table.**
  Measured, labeled, appended — never rewritten.
* Same for per-app PSS and server PSS, which the harness already records.

## Hazards

Ranked by how expensive they are to fix late.

**1. Parametric queries are an information-disclosure oracle.** The path
model gives a clean property — unauthorized is indistinguishable from
nonexistent — because a path either resolves or it does not. Parameters break
it, and unlike paths they are *designed* to be enumerated. If
`get_project(id=7281)` distinguishes unknown from denied from malformed, that
is an enumeration API for the whole object graph.

The fix must be structural and must precede handler invocation:

* Normalize every request to a descriptor — `{ operation, target, canonical
  parameters }` — and authorize *that*.
* **References passed as parameters are themselves protected resources.**
  `open_tasks(project=X)` must not even resolve `X` until the caller is
  permitted to name `X` in that context. Authority to invoke an operation is
  not authority over every object passed into it.
* Resist turning this into an ABAC rules engine. A closed verb —
  `Read | Action(id)` — added to `authorize(identity, path)` covers the real
  cases.

And be honest about the limit: status, response shape, size, timing and
follow-on behaviour are all observable. Perfect timing resistance over
application-defined computation is not achievable. The defensible promise is
narrow and should be stated exactly:

> Rill core does not expose authorization distinctions through protocol
> status or metadata. Application authors are responsible for semantic side
> channels.

**2. Schema discovery is itself disclosure**, and the obvious fix collides
with the cache. A schema listing `payroll` or `disable_intrusion_alarm`
discloses that those concepts exist even to a principal who cannot use them —
so schemas must be principal-scoped. But `RefIndex::ref_path` is
`Hash::of(authority + path)`: **the client cache keys on (host:port, path) and
nothing else.** That invariant holds today only because policy *denies*
rather than *varies* content. Per-principal content at a shared path would be
the first thing to break it, and silently — one principal's schema served to
another from cache.

The clean fix, which also sidesteps fingerprinting: **content that varies by
principal is addressed by hash, never by path.** The server names your schema
`blake3:…`; you fetch the object by hash. Correct caching by construction,
safe sharing between principals with identical capability sets, no
per-principal path to observe. Worth adopting as a general invariant.

**3. Action delivery has no at-most-once story.** CAS solves lost updates.
It does nothing for duplicate submission, and subscriptions make reconnects
routine. `Client::action` is documented "never retried automatically" — an
honest answer that pushes the problem to the caller. For non-idempotent
external effects (page an engineer, restart an instance, charge an account —
exactly the NOC and corporate actions this direction targets), a dropped
connection leaves the caller unable to know whether it happened. Fix: a
**client-generated action id, deduplicated server-side within a bounded
window.** Small, and it also makes safe automatic retry possible.

**4. Blob implies ranged reads, which do not exist.** `Get` and `GetIf` carry
no offset or range; `METADATA` reports size and `RESOURCE` streams with a
`more` flag, but only from the beginning. Fine for documents, forgivable for
images, disqualifying for video seek and painful for resumable transfer on a
flaky link — which is precisely the phone-tether case the remoting story
wants to win.

**5. CAS assumes a single authoritative writer per resource.** So does a
server-serialized operation stream. That is the right call for an appliance,
but the north star says "your identity plus your *servers*", plural, and
replicated writes are concurrent writes by definition. State the constraint
deliberately: *resources have a single home server; replication is read-only
caching plus turn-based handoff, never concurrent write.* That keeps offline
useful — the Lotus Notes lesson — without opening the consensus door.

**6. The client is sequential.** `Client` is documented and implemented as
one request in flight. Subscriptions therefore are not "two new frame types";
they are multiplexing, response dispatch, subscription lifecycle, bounded
queues, server fanout, reconnect state and backpressure. Sequence it so
there are intermediate verification points:

```text
sequential Client → multiplexed Client → long-lived stream responses
                  → subscription semantics
```

Request/response must keep working unchanged at every step.

**7. Long-lived state raises the soak burden.** risks.md #4 already ranks
reliability over cleverness, and the cache-sweep bug fixed on 2026-08-13 was
exactly a "connect once, live forever" defect — invisible to unit tests and
to 30-second samples. Subscriptions make sessions longer and stateful. A
subscription implementation is not done until reconnect, slow consumer,
server restart, revocation mid-stream, and a multi-day soak are all
exercised.

## Collaboration, scoped

Three levels, only the first two of which Rill core should know about:

```text
Level 0   resource + compare-and-swap        approvals, forms, ordinary apps
Level 1   server-serialized operation stream chat, kanban, live ops tools
Level 2   application-specific CRDT / OT     true offline concurrent editing
```

Level 2 lives *behind an application server*. Rill receives revisions and
semantic data like it does from anything else. CRDTs genuinely solve
concurrent offline editing, and their complexity is genuinely real; keeping
them out of the protocol is what stops "Rill collaboration" from becoming a
second project.

Compare-and-swap is the near-term win and reuses `GET_IF`'s machinery:

```text
ACTION { target, expected_hash, fields }
    current_hash != expected_hash  →  CONFLICT(current_hash)
    otherwise                      →  apply
```

Its limitation is what makes it good: **it detects conflict, it does not
resolve conflict.** The application decides what happens next.

It also gives the agent story a concrete safety property that is not a
promise about model behaviour:

> Every mutation can be conditional on the exact state the agent observed.

A stale agent cannot silently act on a world that has moved. That is
differentiated, and it falls out of a primitive that already half exists.

## Sequencing

```text
0. Decide facts vs views vs staging          (this document, ratified)
1. Typed resource / query schema             — authorization model designed
                                               here, even if implemented at 4
2. Headless CLI, against a mock adapter      — concurrent with 0–1
3. Compare-and-swap actions
4. Policy: operation + parameter authorization
5. Wire-byte instrumentation in bench-stack  — before 6, to gate it
6. Multiplexed client → subscriptions
7. One real adapter (CI is the best candidate) and a project room
```

Two notes on the ordering. The authorization *model* belongs to step 1 even
though it lands at step 4 — the oracle problem is a property of the schema's
shape, and a schema whose errors cannot be made uniform cannot be fixed
later. And the CLI is the forcing function for whether the semantics are
presentation-shaped, so it wants to run while the schema is still soft.

The tests that decide whether this direction is real:

1. Can the CLI discover what is available without knowing the integration?
2. Can it validate parameters entirely from the schema, before sending?
3. Do denied objects remain semantically invisible — no status, shape or size
   distinction?
4. Can an action require the revision the caller actually observed?
5. Can a mutation be conditional on state observed three commands ago — not
   just within one interactive session? (If CAS only works inside a session
   it will not survive agents or scripts.)
6. **Does the adapter need protocol additions specific to the system it
   wraps?** If yes, repeatedly, the model is not general and the answer is to
   stop, not to add frames.

Then point a document at the same adapter. If it renders without changing the
semantic interface, the facts/views split is doing its job.

## Strategic note: headless first

The strongest argument for this direction is not the desktop. It is that a
Rill server becomes useful with **zero Rill screens** — an adapter over CI,
GitHub, Home Assistant, a NAS, a calendar, consumed by a CLI and an agent.
That inverts the project's worst risk (risks.md #2: never finding the first
thing Rill is uniquely best at), because adoption stops requiring anyone to
replace their desktop first.

```text
existing service → Rill adapter → typed semantic interface → CLI
                                                           → agent
                                                           → later, a screen
```

The renderer becomes the nicest client rather than the only reason the system
exists.

## Open questions

Genuinely undecided; listed so they are not settled by implication.

* Does a document **bind** to query results client-side, or does the server
  render the document from facts? The first is where native feel comes from
  and where the algebra hazard lives; the second is simpler and slower.
* Is there a standard core vocabulary of resource types (task, incident,
  build, metric), or is everything app-specific with only the *shapes*
  standard? Standardize only where interoperability clearly pays.
* How does staging interact with offline? Which actions may queue
  (`add_comment`) and which must never (`transfer_money`) — declared per
  action, presumably, but by whom and validated how?
* What is the conflict *presentation*? CAS returns CONFLICT; what a document
  is supposed to do with that is a UX question with no obvious default.
* Where does presence live? It is neither durable fact nor pure view state —
  a transient subscribed resource is the obvious answer, and it is the first
  thing that will test the per-subscription server budget.

## The thesis, restated

> Rill may not fundamentally be a replacement for the web, or a thin-client
> desktop. It may be a small protocol for exposing trusted semantic state and
> permitted actions, in which screens, agents, automations and physical
> devices are all peers consuming the same world.

The desktop is then the spectacular demonstration of that architecture rather
than the boundary of it — and every measurement in
[resource-envelope.md](resource-envelope.md) supports that framing better
than it supports "web replacement."
