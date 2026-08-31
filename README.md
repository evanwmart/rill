<div align="center">

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="assets/brand/rill-logo-dark.svg">
  <img src="assets/brand/rill-logo-light.svg" width="88" alt="Rill">
</picture>

# Rill

**Windows are documents. The wire carries meaning, not pixels and not foreign code.**

[![License: MIT OR Apache-2.0](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-4c6ef5)](LICENSE-MIT)
[![Rust 1.98](https://img.shields.io/badge/rust-1.98-b7410e)](rust-toolchain.toml)
[![Status: pre-alpha](https://img.shields.io/badge/status-pre--alpha-c9a227)](#status)
[![Idle footprint: 28–34 MiB measured](https://img.shields.io/badge/idle_desktop-28–34_MiB_measured-2f9e44)](docs/memory-footprint.md)

</div>

An app tells your device what is on screen (text, inputs, layout, a fixed set of allowed actions) and your device does the drawing. The idea is older than the web ([docs/lineage.md](docs/lineage.md)); Rill's bet is that one format can carry the apps, the desktop itself, and everything downstream of them: theming, remoting, recording, replay.

Two rules hold everywhere. **No foreign code**: a client never executes an algorithm it cannot inspect. And deny-by-default so complete that what a device is not granted does not appear to exist.

## In practice

- An app is a server. It sends a compiled binary document (layout, text, inputs, and a fixed vocabulary of allowed actions) and the client does the drawing. Nothing executable crosses the wire, so there is nothing to sandbox.
- Trust is pairwise. Devices are enrolled by certificate fingerprint over TLS 1.3; there are no certificate authorities. Access is deny-by-default, and what a device is not granted, it cannot observe the existence of.
- Content is addressed by hash, not location. An app ships as one deterministic bundle, cached once, verified on every read.
- The desktop speaks the same format as the apps. Windows, dock, and chrome flatten into one draw-command stream (which is also what remoting, session recording, and replay are made of).

## Numbers

All measured, methodology and raw data in [docs/](docs/):

- The whole desktop (compositor, server, dock, live widgets) idles at **28–34 MiB PSS** on a 1 GB Raspberry Pi 5, and runs a 30 fps widget workload at 36–41 MiB.
- A window update travels as **0.4–1.2 KB** of draw commands. An idle desktop draws essentially no frames.
- Core binaries, release stripped: **26.5 MiB**.
- As this is written, the reference Pi is days into an unattended endurance run, fanless, never throttled. The run has already caught and fixed one real memory bug, which is what it is for.

## Lineage

None of the ideas here are new; the bet is the combination. The systems Rill most resembles, and where it parts from each ([docs/lineage.md](docs/lineage.md) has the fuller genealogy):

- **X11** (1984) proved the display could be a network protocol. Then toolkits stopped using its drawing vocabulary and shipped pixels, and the network transparency rotted. Rill's wire carries a retained semantic scene, and vector-native apps have no pixel path to fall back to.
- **HyperCard** (1987) proved documents can behave like software. It had no network, and its scripts had no limits. Rill keeps the documents-not-programs thesis and adds the network, the identity model, and the capability floor; the authoring slope HyperCard nailed is still ahead of us.
- **Plan 9** (1990) proved one small protocol could carry a whole distributed system. It unified at the file: bytes any program could read and no program could understand. Rill unifies one level up, at the scene, where the wire says "text, field, action" and every consumer understands it.
- **VNC** (1998) proved you can remote everything if you understand nothing: pixels are universal and meaningless. A Rill session travels as the same kilobyte-scale command stream the local GPU renders (reflowable, rethemable, searchable), with the honest caveat that only vector-native windows travel that way.
- **Sun Ray** (1999) proved the device could be just glass: a session followed a smartcard between stateless terminals. But its glass was dumb and its wire was pixels, so it lived and died on the LAN. Rill's glass is a renderer: state lives on your server, drawing happens on your device, and what crosses between them is thin enough for any network.

## On foreign code

The intention behind "no foreign code" is not that local computation is bad. It is that a client should never execute an algorithm it cannot inspect. Most platform security exists to compensate for shipping opaque code to strangers; Rill's base tier removes the thing being compensated for. A document can declare what to show and which of a fixed set of actions it offers, and that is all it can say.

Extensibility is planned as a ladder above that floor, not a hole through it ([specs/compute-apps.md](specs/compute-apps.md): direction recorded, not yet designed):

```text
Tier 0  Document app   inert documents, server authority, provably no code   (today)
Tier 1  Compute app    sandboxed WASM, renders through the same draw stream,
                       zero ambient authority, capabilities via broker       (planned)
Tier 2  Surface app    sandboxed WASM with an explicitly granted GPU surface (planned)
```

A compute app is the same signed, hash-addressed pack, with a WASM module in place of documents. The module arrives at install, not over the wire: what crosses at runtime is still only the draw stream it renders. What keeps it non-opaque:

- The manifest declares the app's tier and every permission, shown at install.
- A WASM module's import section declares every host function it can ever call, in the bytes, before execution. Confinement is checked statically at install, not hoped for at runtime.
- Reproducible builds mean the shipped hash can be verified against source.
- Admission is at the lowest tier that suffices; a notes app never gains the vocabulary to ask for compute.

Nothing an app runs, reaches, or calls is hidden from the client. The security model is confinement-first: a hostile module runs in a box where hostility is inert. Detection-first is the model that keeps failing everywhere else.

## Status

Pre-alpha. The protocol, content-addressed store, packaging, document format, compositor, and the first applications are built and running, but wire formats still change weekly, nothing has had outside security review, and there is no stability promise of any kind. Don't build on it expecting the ground to hold still. Exploring is welcome; depending is currently premature.

## Layout

- [specs/](specs/): design documents and recorded decisions. The project is spec-first: the intent is that another engineer can go spec, to invariant, to feature, without a walkthrough.
- [docs/](docs/): evidence. Measured memory attribution, endurance protocols with raw CSVs, a supply-chain audit, design lineage. Every performance claim is labeled measured, projected, or target.

Some documents cite internal planning files (`risks.md`, `TODO.md`) that aren't published. The citations are kept for honesty about how decisions were made, not as links to follow.

## License

Dual-licensed under [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE),
at your option. Creative assets and their provenance are covered in
[CREDITS.md](CREDITS.md).

Unless you explicitly state otherwise, any contribution intentionally
submitted for inclusion in this project by you, as defined in the
Apache-2.0 license, shall be dual licensed as above, without any
additional terms or conditions.
