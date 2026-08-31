# Lineage — the philosophy Rill keeps rediscovering

Status: **reference essay**, 2026-08-17, systems shelf added 2026-08-30.
Not a spec and not load-bearing;
this exists so the project can state its intellectual position precisely,
learn from how each ancestor failed, and recognize the pattern when it
appears outside computing. Companion to
[architecture-advantages.md](../specs/architecture-advantages.md) (what
the architecture buys) and risks.md #6 (the failure mode every ancestor
shares).

## The idea, stated once

**Factor communication into (a) a small, shared, declarative vocabulary
and (b) receiver-local interpretation.** The sender transmits statements
about content — not pixels, not programs, not prose — and every receiver
renders those statements at its own fidelity, for its own purpose.

There is no single canonical name for this. The closest is Tim
Berners-Lee's **Principle of Least Power**: use the least expressive
language adequate to the task, because the less expressive the
representation, the more different consumers can do with it — render it,
diff it, index it, replay it, read it as a machine. Rill's tier boundary
is that principle with structural enforcement instead of good intentions.

## The computing ancestry (and how each one failed)

**The web's founding thesis.** Semantic HTML was exactly this: markup
says what content *is*; the client — graphical browser, Lynx, screen
reader — decides presentation. It eroded the way risks.md #6 predicts:
one more dynamic behavior at a time, until the declaration became a
program-delivery vehicle and semantics became something scrapers
reconstruct. Rill is in large part a restoration of the original web
thesis with the erosion structurally prevented.

**REST / HATEOAS.** Fielding's dissertation is the *actions* half stated
academically: representations carry the legal next actions (hypermedia as
the engine of application state). The most cited and least followed idea
in networking — nothing enforced it, so every client hardcoded endpoints
and every API became RPC. Rill's declared-action vocabulary is HATEOAS
with teeth: the client structurally cannot act outside the document's
declaration.

**The block-mode terminal — the philosophy's greatest commercial
success.** The IBM 3270 (1971): server sends a declarative screen
definition (fields, attributes, protected regions); the terminal renders
locally, edits locally, submits structured data back. No downloaded code,
tiny wire cost, dumb-cheap endpoints, decades of bank and airline
deployments. Proof the model carries serious production workloads. Its
living descendant in spirit is the Gemini protocol / small-web movement —
deliberately under-expressive markup, client-owned presentation.

**The road not taken: mobile code.** NeWS, Display PostScript, Java
applets, and ultimately JavaScript answered the same problem ("the wire
should carry something richer than pixels") by sending *programs*. This
is the deepest fork in the space: expressive-and-unanalyzable versus
inert-and-legible. Mobile code won on capability and paid in security and
semantic opacity; declarative systems won on legibility and got eroded by
capability pressure. Rill bets the eroding force can be contained by a
defended boundary (conspicuous Tier-1 crossings) rather than by
discipline — which no prior system managed.

**The data-side twins.** Codd's relational model is the same move in
databases — "data independence": one logical declarative representation,
many access paths and views. Event sourcing ("turning the database inside
out") is Rill's "one stream feeds every consumer": a single semantic log
from which rendering, recording, diffing, and agent-reading are all
materialized views.

**The Semantic Web — the cautionary maximum.** RDF and ontologies tried
shared machine-readable semantics for *everything*, and failed on the
coordination problem: open-world semantics require everyone to agree on
an ontology, and nobody does. Rill sidesteps the trap deliberately: its
semantics are **closed-world and small** — the vocabulary of UI (text,
rects, fields, actions), not the vocabulary of meaning itself. The
vocabularies that survive history are closed and stewarded (SQL, 3270,
musical notation); the open ambitious ones die of committee.

**Accessibility — the bolted-on version, inverted.** ARIA, AT-SPI, and
screen-reader infrastructure form an industry that *derives* a semantic
representation from UIs after the fact, lossily, forever fighting the
renderer. Rill's identity "agent surface == accessibility tree == the
wire format" makes the derived artifact the primary artifact. Screen
recording, automation, and agent frameworks are the same story: five
industries reconstructing semantics the source had and discarded at the
pixel boundary.

## The systems shelf (what Rill will be compared to)

The section above traces the *philosophy*; this one takes the five
concrete systems a skeptical reader will reach for, and states the
inheritance and the divergence precisely. Each pairing follows the same
shape: what the ancestor proved, how its implementation betrayed its
goal, and which structural change Rill makes so the same betrayal is
unavailable.

**X11 (1984) — drawing intent on the wire, at the wrong altitude, with
no policy.** X proved a display could be a network protocol, then made
two choices that undid it. "Mechanism, not policy" pushed every semantic
— widgets, text, even what a titlebar is — out to toolkits, so the
server understood nothing it displayed; and its immediate-mode drawing
vocabulary was optional, so toolkits eventually rendered client-side and
shipped pixels through it (`XPutImage`), quietly killing the network
transparency X existed for. Add a trust model where any client can read
any other's input and forty years of extension accretion, and the lesson
is exact: *an optional semantic vocabulary always rots to pixels.* Rill
inverts all three choices — the scene is retained and semantic, policy
(theming, focus, chrome) is the platform's, and for vector-native apps
the vocabulary is not optional because there is no pixel path beside it.
The isolation comes from the Wayland substrate X never had.

**HyperCard (1987) — documents as software, with no network and no
floor.** Atkinson erased the document/program boundary from the
authoring side: a stack was the thing you used and the thing you opened
up and changed, with a graded slope (browse → type → paint → author →
script) that turned users into makers by degrees. It lacked everything
Rill leads with — networking, identity, any limit on what HyperTalk
could do to the machine — and it died anyway of corporate neglect, not
of its model. Rill keeps the erased boundary and supplies the missing
half: pairwise trust, content addressing, and a capability floor in
place of an unlimited scripting ceiling. The honest asymmetry runs the
other way: HyperCard's authoring slope — the part that mattered most —
is the part Rill has not yet built, and `rill preview` is only its first
rung.

**Plan 9 (circa 1990) — one small protocol, one level too low.** Plan 9
proved a single spare protocol (9P) plus per-process namespaces could
carry an entire distributed system, and it remains the most coherent
answer ever shipped to "the network is part of the computer." But it
unified at the *file*: bytes any program could read and no program
could understand — the display itself (`/dev/draw`) was still a pixel
channel wearing a file mask, and names were locations, not content.
Rill takes the "one small vocabulary, uniformly applied" discipline and
moves it up to the scene, where the tokens carry meaning every consumer
shares; naming moves from paths to hashes, and trust from lab-machine
assumptions to per-device identity. Plan 9's fate teaches the other
lesson Rill acts on: a coherent system with an empty desktop loses to an
incoherent one with software — which is what the foreign-window path is
for. Coherence for the natives, a ford for everyone else.

**VNC (1998) — interpretation collapsed all the way.** RFB is the
limit case of "collapse early" from the section below: the wire is a
framebuffer, deliberately semantics-free, which is precisely why it
works with everything and understands nothing. No reflow, no retheme,
no search, no accessibility, bandwidth proportional to pixels — those
aren't bugs, they're the price of universality, paid in full. Rill
occupies the opposite corner and keeps the receipt visible: a remoted
vector window is the same sub-kilobyte command stream the local GPU
renders, with every downstream freedom intact — and a remoted *pixel*
window is VNC again, because on that path Rill has no more semantics
than RFB does. The comparison is a boundary marker, not a conquest.

**Sun Ray (1999) — the right dream on the wrong wire.** Stateless
terminals, sessions that followed a smartcard, nothing local worth
stealing or syncing: Sun Ray is the closest ancestor to "your computer
is an identity plus streams; devices are just glass," and it worked —
inside a corporate LAN. Its protocol carried server-rendered pixels, so
the glass was dumb, the link had to be fat and near, and the economics
only closed for enterprise seat management; Oracle shut it down in
2014. Rill keeps the statelessness where it belongs (authority and
state on your server) and moves *rendering* into the glass: the wire
carries meaning measured in kilobytes, so the same session-mobility
dream survives weak links, caching makes the glass tolerant of a bad
network rather than paralyzed by it, and the owner is a person, not an
IT department.

One sentence of synthesis, since the five converge: X11 and Plan 9 had
the right wire discipline at the wrong semantic altitude; VNC and Sun
Ray had the right endpoint dream with interpretation collapsed too
early; HyperCard had the right content model with no network and no
floor. Rill is the claim that one format at the scene altitude —
declarative, closed, content-addressed, pairwise-trusted — satisfies
all five ambitions at once, and the burden of that claim is exactly the
stewardship problem the rest of this essay describes.

## The generalization: this is not a computing pattern

The same factoring appears wherever one sender must serve many
heterogeneous interpreters. The clearest instances:

**Teaching.** A teacher cannot transmit understanding — only
representations. Pedagogy's whole discipline is packaging knowledge into
discrete, recognizable terms (didactic transposition), which each student
reconstructs into their own comprehension (constructivism: Piaget,
Vygotsky). Bruner's spiral curriculum — the same concept honestly
representable at every level of sophistication — is fidelity tiering:
glass classes for minds. Even assessment fits: students respond through a
constrained shared vocabulary (the answer format), the round-trip
structured action. Crucially, education theory treats per-student
interpretation as *the mechanism of learning*, not noise to eliminate —
the strongest available argument that client-side rendering is a feature
of communication, not a compromise.

**The musical score.** Perhaps the single best analogy Rill has. A score
is a declarative semantic representation; every performer renders it
uniquely; a piano reduction is the same work at lower fidelity; and the
score-versus-recording distinction *is* the vector-versus-pixel
distinction — a MIDI file to an MP3 as a DrawCommand stream to a
framebuffer. Notation also demonstrates the erosion pressure: centuries
of increasingly prescriptive markings (dynamics, articulation, metronome
marks) — the vocabulary always wants to grow toward controlling the
interpreter — and yet it survived by staying declarative: a score states,
it never executes.

**Data pipelines.** Schema-on-read, the modern semantic layer
(LookML-class), fact tables and marts: atomic declarative facts at the
center; each consumer — dashboard, notebook, ML feature store —
materializes its own interpretation. The data world converged on this
shape for the same reason Rill did: it is the only architecture whose
cost scales with (senders + receivers) rather than (senders ×
receivers). Point-to-point bespoke integration is the pixel buffer of
data engineering.

**Ledgers, laws, measurements.** Double-entry bookkeeping: atomic
declarative entries; balance sheet and cash-flow statement are
materialized views. Law: declarative rules, court-local interpretation —
including the erosion (case-law accretion as vocabulary creep). SI units:
a tiny closed vocabulary that lets every lab interpret every paper.
Same shape, everywhere coordination at scale succeeded.

## The formal spine (information theory's abandoned agenda)

Shannon 1948 excluded meaning on page one ("the semantic aspects of
communication are irrelevant to the engineering problem"), but Weaver's
companion essay named three levels: **A** — transmit symbols accurately
(solved); **B** — how precisely symbols convey meaning; **C** — how
*effectively* received meaning affects conduct. This project's design
questions are Levels B and C, and the scattered formal tools map onto
them directly:

* **What atoms? → minimal sufficient statistics / the Information
  Bottleneck.** Atoms are never intrinsically right — only relative to a
  declared family of downstream tasks. Rill's family: {render at any
  fidelity, interact, record/replay, diff, audit, agent-read}. This
  turns the vocabulary stewardship gate from taste into criterion: **a
  new atom is justified iff some declared consumer needs information the
  existing atoms cannot carry** — rejected if it merely re-encodes the
  expressible or serves a consumer not committed to.
* **What ablations? → rate–distortion theory with semantic distortion
  measures.** Every glass class is a rate–distortion operating point:
  Tier B ablates effects but keeps structure; Tier A keeps only named
  values and actions; a pixel window ablates meaning itself and keeps
  appearance (.rillrec already records them as labeled placeholders —
  the ablation declared, not hidden). Design rule: **ablation is
  declared, never incidental** — capability negotiation per glass class
  is, formally, the receiver announcing its distortion measure.
* **What is effective? → Weaver Level C / relevance theory.** Relevance
  = cognitive effect per unit of processing cost (Sperber & Wilson).
  Measurable here as Δ(understanding or action) per byte, joule, and
  millisecond of compositor work. The 12.5 Hz ASCII widget was a
  relevance failure by this definition — information at 12× the rate it
  produced effect — which is why widget cadence is the dominant power
  knob: cadence *is* the effectiveness dial.
* **Interaction → speech acts and affordances.** Declared actions are
  Gibson's affordances made explicit and Austin/Searle speech acts with
  a closed illocutionary vocabulary — the return channel under the same
  discipline as the broadcast channel.
* **Client-specific interpretation → channel theory.** Barwise &
  Seligman's classifications-and-infomorphisms ("Information Flow",
  1997) is a worked mathematics of the same tokens classified
  differently by differently-typed agents. Glass classes are
  infomorphisms.

The definition that falls out, worth keeping: **information is semantic
precisely to the extent that it is invariant under the group of
legitimate client interpretations.** Presentation is what varies;
semantics is the equivalence class that survives theme, zoom, fidelity,
and device. That is the pixels-vs-vectors invariant stated formally —
DrawCommands in logical units are app state quotiented by device, and
the known caret `.round()` violation is, in this language, a semantic
leak: device-specific information contaminating the invariant.

Field note: this is becoming live engineering under the name **semantic
communication** (6G research — transmit task-relevant meaning, not
bits), built there on learned neural codecs whose vocabulary is an
opaque latent space. Rill is a semantic-communication system with a
hand-designed, inspectable, closed codebook — the same relationship to
that field that the security story has to the web's: legibility as the
differentiator.

## Where interpretation collapses

The deepest way to state the pixel-versus-vector distinction: **every
communication is receiver-interpreted eventually; the design choice is
where interpretation collapses.** A sender can collapse it early —
pixel buffers, an MP3, verbatim lecture notes, WYSIWYG — buying
guaranteed uniformity and paying in bulk and rigidity. Or it can
delegate — DrawCommands, a score, concepts, semantic markup — buying
reflow, retheming, fidelity tiers, and machine-readability, and paying
in interpretive variance.

Neither side is free, and the choice is legitimate in both directions
(a legal contract collapses early *on purpose*). Rill's bet is that for
personal computing the variance is the point: reflow, live retheming,
agents, and seven-segment glass are not four features but one — the
same delegation, exercised four ways. A teacher who could force
pixel-identical understanding into every head would not be a better
teacher; they would have prevented comprehension from happening. The
uniformity-guaranteeing strategy and the comprehension-enabling
strategy are different strategies, and a platform must know which one
it is.

## The invariants (what every surviving instance shares)

1. **A small, closed, stewarded vocabulary.** Lexicon, notation, schema,
   DrawCommands. Open or committee-owned vocabularies die (Semantic Web);
   stewarded ones survive (SQL, SI, scores). Someone must own the
   boundary and say no.
2. **Declarative, inert content.** Statements, not instructions. A score
   does not execute. The moment content computes, receivers can no longer
   analyze it — only run it.
3. **Receiver-local rendering, matched to receiver capability.** The
   fidelity tier is chosen by the glass, not dictated by the sender —
   monitor or e-paper, orchestra or piano, expert or novice.
4. **Round-trips through the same vocabulary.** Declared actions,
   assessment formats, SQL writes. The response channel is as constrained
   as the broadcast channel, which is what makes the whole loop
   analyzable and auditable.
5. **Permanent expressive pressure on the boundary, and survival by
   stewardship.** Every instance records the same failure mode: jargon
   creep, notation creep, schema sprawl, HTML→JS. The boundary is not a
   design decision made once; it is a product maintained forever.

## When algorithms enter (semantics + capabilities + constrained compute)

Declarative semantics eventually meet needs that data cannot serve, and
this is where every ancestor died (HTML→JS). The general model that
survives has three parts: **atomic semantics + explicitly granted
authority + purpose-bound algorithms** — which is Rill's tier ladder
(compute-apps.md) stated abstractly, and it has a name: the
**object-capability model**. Code holds no ambient authority; it affects
the world only through references explicitly handed to it. WASM imports
are ocap made decidable — the permission manifest is in the artifact,
checkable before execution, unforgeable at runtime.

The exam analogy states it exactly: a cheat sheet of numeric answers is
precomputed data — sufficient when the question set is closed. An
algorithm plus a *declared toolset* (calculator, protractor, no phone)
handles open question sets, and grading remains possible precisely
because the toolset was enumerated — an answer only producible with an
unlisted tool is detectable. The invigilator is the broker; the
enumerated desk is the import section; the gradeable answer is the
capability audit log.

Two orthogonal constraint axes, and a ladder that climbs them in order
(**admission always at the lowest rung that suffices**):

1. inert data (Tier 0 documents)
2. parameterized presets (shader params, tokens — data selecting among
   reviewed code)
3. sub-Turing expressions (bridge selectors/formatters — analyzable
   interior, zero authority)
4. Tier 1 WASM (opaque interior, closed decidable boundary, fuel-metered)
5. Tier 2 granted surface (last, scariest)

Rungs 1–3 constrain *expressiveness*; rungs 4–5 constrain *authority*.
The web's tragedy in this language: it had no ladder — its second rung
was already Turing-complete with ambient authority, so every need,
however small, paid the maximum price.

Admission test for any algorithm (extends the vocabulary test above): an
algorithm is justified only when a declared purpose faces an **open
question set or a latency bound** — inputs unenumerable server-side, or
loops a round-trip cannot serve. Never for convenience.

The governing precedent: **spreadsheets** — atomic semantics (cells),
small pure algorithms (formulas), authority only via explicit references
(named ranges). A billion users, no security apocalypse — until VBA
bolted ambient-authority mobile code onto the same product and made it
the world's leading malware vector. One product, both halves of the
argument. Shaders are the success half retold: pure functions over
declared uniforms, no I/O — and Rill already ships that rung.

(Precision note: `.rillrec` records emitted frames verbatim, so replay
tolerates nondeterministic Tier-1 apps; purity is a strong preference —
for the render cache and agent predictability — not a structural
requirement.)

## The one honest disanalogy

Human vocabularies are never truly closed: word meanings drift, and
humans repair ambiguity interactively (conversational grounding).
Machine systems can do what human ones cannot — enforce the closed
vocabulary exactly (strict decode, connection cut on malformed input) —
but they also lack the human escape hatch of negotiating meaning on the
fly. Rill's strictness is therefore both its advantage over human
systems and the reason its vocabulary evolution (versioning, capability
negotiation per glass class) has to be designed rather than left to
drift: a machine protocol has no repair channel. When a term confuses a
classroom, the room stops and renegotiates; when a codec meets a tag it
does not know, the connection dies. Everything a human vocabulary
handles by conversation, a wire vocabulary must handle by explicit
version and capability design — which is why that work is
load-bearing, not plumbing.

## Why this matters practically

Two working conclusions, so this essay earns its place in docs/:

* **When explaining Rill, the strongest analogies are non-computing
  ones.** "A window is a score, not a recording" lands with people whom
  "DrawCommand stream" does not. The teacher and the ledger make the
  same point for different audiences.
* **When evaluating a proposed addition, ask which side of the pattern
  it lives on.** Anything that grows the shared vocabulary must clear
  the stewardship bar (risks.md #6, #12); anything receiver-local is
  cheap. Most feature pressure can be satisfied on the interpretation
  side — that is the pattern's entire gift.
