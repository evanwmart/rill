# Rill Compute Apps — Idea Log (pre-design)

Status: **not yet designed** — captures a direction reasoned through in Aug
2026, per the plan's "record ideas for later phases, don't design early"
rule (§10). Nothing here is committed. Prerequisite for any of it is the
capability broker (milestone 12); the natural home is the compositor phase
(Group Four), since hosting many WASM apps *is* building a compositor.

Related: [application-model.md](application-model.md) (the document tier that
exists today is "Tier 0" below); [../docs/lineage.md](../docs/lineage.md)
§"When algorithms enter" for the theory under the tiers — the
object-capability model, the least-power ladder (data → presets →
sub-Turing expressions → WASM → surface, admit at the lowest rung that
suffices), and the algorithm-admission test (open question set or latency
bound, never convenience).

---

## The idea: a ladder of app trust tiers over one substrate

Today an app is inert documents + server authority + no client code. The
direction is to add *optional* local-compute tiers **without** losing that
property for the tiers that don't need it — fixing Electron's mistakes
rather than repeating them.

```text
Tier 0  Document app   inert docs, server authority, provably no code
                       (what exists now: notes, dashboards, settings, forms)
Tier 1  Compute app    sandboxed WASM, renders THROUGH rill-ui DrawCommands,
                       zero authority, capabilities via broker
                       (IDEs-lite, spreadsheets, editors, local-logic games)
Tier 2  Surface app    sandboxed WASM (or native) with a granted GPU surface
                       for real-time / 60fps (action games, video preview)
```

Each tier is **declared in the manifest**, attested by the app's identity
(server fingerprint + app_id), and shown to the user at install. Deny-by-
default everywhere: even a Tier-2 app starts able to do only three things —
compute, render, read input — and acquires everything else through consent.

## Why the substrate is (mostly) already built

A compute app is just a pack containing a WASM module instead of `.rill`
documents. So the entire delivery stack — manifest, `.rillpack`,
content-addressed install, fingerprint identity, staged atomic updates,
offline cache — works **unchanged**. Only the *runtime* differs. This is
what makes a multi-tier model an addition, not a rewrite; Tier 0 apps never
touch the WASM runtime.

Manifest addition (sketch): `kind = "document" | "compute" | "surface"`;
for compute/surface, `entry` points at a WASM module.

## Why WASM (honest)

WASM is **not** best on raw perf or graphics — native-in-a-sandbox beats it
there. It is best on the axis that is Rill's whole thesis: **statically
analyzable confinement**. A module's *import section* declares every host
function it can ever call, in the bytes, before execution — so what an app
can reach is a *decidable, total* check on the artifact, not a runtime hope.
Native binaries can't offer this (syscalls aren't declared; you only filter
at runtime, Electron's exact failure). Secondary substrate for trivial
scripting if ever wanted: Lua (tiny, capability-friendly). WASM stays the
primary.

## The security model: confinement-first, detection as defense-in-depth

Antivirus is detection-first, which is why it fails (Rice's theorem — you
cannot statically prove arbitrary code non-malicious). Rill inverts it: the
malicious module runs in a box where malice is *inert*, and detection is
hygiene on top.

Before a module ever runs locally:

1. **Static capability confinement (total, decidable).** Reject any module
   whose imports exceed the manifest envelope / allowed host ABI — at
   install, never run. The module *cannot* call an unimported host fn.
2. **Validation (free).** Type/memory-safety guaranteed pre-run; the
   native buffer-overflow class is structurally impossible.
3. **Attestation + hash denylist (exact).** Pack is hash- and identity-
   bound; reproducible builds let source be checked against the shipped
   hash; known-bad hashes never run.
4. **Static resource/shape checks (signal).** Reject absurd memory
   declarations; flag suspicious capability combinations for extra consent.
5. **Observation dry-run (signal).** Run zero-capability with all broker
   calls auto-denied and logged; a "notes app" that probes for sockets
   outs itself. Defeatable by patient adversaries → signal, not gate.

**Runtime backstop (why 1–5 needn't be perfect):** an undetected malicious
module can only compute pointlessly (killed by fuel/memory limits), draw in
its own window, and pop consent prompts you deny. It cannot exfiltrate,
persist, or touch the machine — every capability needs a live consent
through a broker it cannot forge or reach, and it cannot exceed its declared
imports.

**The one residual, stated honestly:** abuse of a capability you *granted*
(a text editor given one file that corrupts it) is not a confinement
failure — it's the app doing what you permitted. No sandbox solves this.
Mitigation is narrow grants (one file, not a dir; origin-only network) so
any single trusted grant's blast radius is minimal — an improvement over
Electron's all-or-nothing, not a cure. The last decision rightly sits with
the user's consent.

## gpui fit

WASM plugs into the **front** of the pipeline, not the back — it's another
*producer* of `DrawCommand`s, sibling to the document resolver, and gpui (the
consumer) never knows the difference. This is the payoff of keeping rill-ui
backend-agnostic with DrawCommands as the seam.

* **Render:** host ABI gives the module a render sink; it writes an encoded
  DrawCommand buffer per frame; the host paints it in the same gpui canvas
  callback that paints document commands. Compute and document apps render
  through the identical path with the identical native look.
* **Input:** gpui events → module's exported `handle_event` → new command
  buffer. Same loop the viewer runs, WASM in the middle instead of a server.
* **Threading (important):** run app WASM on its own task, events-in/
  commands-out over channels — never the main thread. With epoch-interruption
  + memory caps, a slow/malicious app can't freeze the shell (its window goes
  stale; the compositor survives). "One app can't hang the session" falls out
  of the isolation.
* Hosting many WASM apps producing frames you composite *is* a compositor →
  this converges with `rill-compositor` (Group Four). Tier-2 raw GPU surface
  is the harder, later, scarier capability; Tier-1 DrawCommands is the clean
  first step and covers most apps.

## The disciplinary risk (the real one)

Compute is familiar ("just code"), so developers will reach for it and Tier 0
could wither the way static HTML did — Electron-but-ours. Countermeasure is
UX incentive, not architecture: document apps get the safe badge, one-click
install, no prompt; compute apps show honest friction ("runs code, wants:
[envelope]"). If installing a compute app feels heavier than a document app,
the gradient keeps declarative the default. Keep Tier 0 the one that feels
good.

## Sequencing (when, not now)

1. Capability broker (milestone 12) — the prerequisite; gives sandboxed code
   its I/O.
2. Tier-1 WASM host exposing ONLY render + input + broker; renders through
   DrawCommands.
3. Prove with one app that genuinely needs local logic (a spreadsheet or a
   code editor — the honest test).
4. Only then consider Tier-2 GPU surfaces.

Each step validated against a real app; the declarative core untouched and
the honored default throughout.
