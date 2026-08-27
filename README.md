# Rill

**Rill is a private, personal replacement for the web — rebuilt from the wire up, and eventually all the way up to being the desktop itself.**

The easiest way to understand it is by contrast with how the web works today.

When you open a web app, your browser downloads a pile of code (JavaScript) from a server and *runs it*. You're trusting that code with your machine. Identity is handled by a global certificate industry, apps can phone home to anyone, and the formats involved (HTML, CSS, JS) are so loose that a huge share of security bugs come just from parsing them.

Rill's answer is to delete each of those problems rather than patch them:

- **Apps are documents, not programs.** A Rill app is a compact binary file describing what's on screen — text, buttons, inputs, layout — plus a small fixed menu of allowed behaviors (navigate, set a value, submit a form, pick a file). There is *no downloaded code to run*. Anything smarter happens on the server, which replies with a fresh document. The client can't be hijacked by app content, because app content is structurally inert.

- **Trust is personal, not global.** There are no certificate authorities. Your server and your devices know each other by cryptographic fingerprint — like exchanging keys with a friend directly. Every connection is encrypted, every device is named, and access is deny-by-default: a file is invisible unless a rule explicitly grants it to your device. To an outsider, private things don't return "forbidden" — they simply don't exist.

- **Content is named by what it *is*, not where it lives.** Every resource is identified by its hash (a fingerprint of its bytes). Your devices cache things once, verify them on every read, and never re-download something they already have. A whole app ships as one deterministic bundle — same input, byte-identical output, every time.

- **One renderer for everything.** Documents, apps, and the desktop itself (the dock, the windows, the wallpaper chrome) all flatten into the same simple list of drawing instructions — "draw this text here, this rectangle there." The window system being built underneath (a Wayland compositor) speaks the same language.

So concretely, today: you run a small server on a machine you own, enroll your laptop and desktop by fingerprint, and your private notes, dashboards, and tools open as native, themeable, offline-capable windows — over any network, visible to no one else.

## What the architecture unlocks

The magic ingredient is that **everything on screen is data, not pixels** — a list of drawing commands, produced deterministically from hash-named documents. That one property compounds into capabilities other platforms can't cheaply get.

**Windows made of vectors, not pixels.** Normal desktops shuffle megabytes of pixel buffers per window. Rill apps can hand the compositor their drawing *instructions* instead. Windows become a few kilobytes, scale perfectly to any resolution or zoom level, and can be re-themed by the system in flight — because the system sees "text and buttons," not a bitmap.

**Remote desktop that costs almost nothing.** If a window is a small list of instructions, mirroring it to another device means streaming those instructions — kilobits, not video. Text stays razor-sharp at any size. And a session *recording* is just an append-only log you can replay, seek, and even text-search ("find the moment this error appeared" is a string search, not video scrubbing).

**Instant everything, via caching that's actually safe.** Because rendering is deterministic — same document, same theme, same window size ⇒ the exact same drawing commands — results can be memoized on disk forever, keyed by hashes. Apps reopen instantly into their last state with zero layout work. Combine that with hash-based prefetching of everything one click away, and navigation feels precognitive.

**A screen that machines can read.** This is the sleeper. An AI assistant on your private server doesn't need screenshots and pixel-guessing to help you — the drawing-command stream *is* a structured, machine-readable screen ("button 'Create note' here, bound to this action"). And the agent's *hands* are the same small, validated action vocabulary humans use — it structurally cannot do anything an app didn't declare. The existing device-identity and permission system scope what it can see; the replay log is a complete audit of what it saw and did. The interface built for accessibility and the interface built for AI agents turn out to be the same artifact.

**Deep customization without danger.** Colors and fonts already resolve through a swappable token table — restyle the entire desktop, live, from one file. The planned GPU backend extends that to animated theme transitions and whole-desktop effects (blur, glow, CRT nostalgia) as *parameters*, not downloaded shader code — so heavy ricing never breaks the "no foreign code" promise.

**Time travel and trust, for free.** Content-addressing means history is diffable and restorable — scrub an app, or your whole environment, back to any past state. Deterministic builds mean you can *prove* what an app is. Signed capability logs mean you can know exactly what any app (or agent) ever touched.

The through-line: the web optimized for reaching a billion strangers, and paid for it with complexity and mistrust. Rill optimizes for *one person's* (or one household's) computing being fast, private, inspectable, and beautiful — and gets away with radical simplicity because it never has to trust anyone it hasn't met.

## Where this goes: your devices become glass

Follow the architecture to its conclusion and you get a different shape of personal computing:

> **Your computer is an identity plus a set of semantic streams. Devices are just glass.**

Every current system makes each device a full computer, then fights to synchronize them — iCloud, Dropbox, browser profiles, an entire industry of conflict resolution. Rill inverts it: there is exactly **one** locus of state (your servers, your identity), and every screen you sit down at renders the same session. Nothing to synchronize, because nothing diverged. Walk away from the desk, open the cheap board behind the TV, sign in — the *same session* is there. Not a video feed of another machine: the session itself, re-laid-out for that screen, because reflow is free when the wire carries meaning. Losing a device is a non-event; nothing local was ever the truth.

And because interaction is commands end to end, **one stream feeds every consumer**. The same bytes render to your GPU, record to disk for perfect replay, diff for damage, and read as *structure* to an AI agent — no screenshots, no OCR, acting through the same validated verbs you click, gated by the same permissions. Today those are five separate industries (display protocols, screen recording, accessibility APIs, automation tooling, agent frameworks); here they are one format.

The demands this makes are small in the places that matter: interaction is kilobytes per frame (usable over a phone tether where video-based remoting chokes), an app's server side measures in single-digit megabytes (your "computer" could run on a router), and the glass itself needs only a modest GPU and a few hundred megabytes of disk, ever. Two honest caveats travel with the claim: the apps must live *somewhere* — this is server-anywhere, not serverless; the pitch is cheap glass everywhere and one small brain you own — and media is the exception, since images and video are irreducibly pixel-heavy. The full argument, with measured/projected/target figures kept separate, is in [specs/appliance.md](specs/appliance.md).

## Why trust a homemade protocol?

The worry is fair: "they invented their own way for computers to talk" usually means "they invented their own locks," and homemade locks are a terrible idea. **Rill didn't make its own locks.** The wire uses the exact same encryption the rest of the internet relies on (TLS 1.3, via a widely trusted implementation). What Rill invented is only what the two computers *say* to each other once they're inside that secure tunnel.

With that settled, the case rests on four ideas:

**Your devices know each other personally — no middlemen.** When you visit a website, your browser trusts it because a chain of certificate companies vouches for it — an entire industry you've never met, and if any one of them gets fooled, so do you. Rill skips all of that: your laptop and your server are introduced to each other *directly*, once, like exchanging phone numbers with a friend. From then on each side recognizes the other exactly, and if anything about the other side's identity ever changes, the connection refuses to work until you personally re-approve it. There's no middleman to trick, because there's no middleman.

**Private things don't say "no entry" — they simply don't exist.** Everything is off-limits unless a rule explicitly allows it. And if someone unauthorized asks for a private file, they don't get "sorry, forbidden" — they get the same answer as if the file never existed. A snoop probing your server learns nothing, not even *what there is* to be denied. Your private life doesn't have a locked door someone can rattle; it has no door.

**The conversation is too simple to be tricked.** Most break-ins don't come from cracking encryption — they come from confusing a machine with cleverly malformed messages, the way a con artist exploits fine print. The web's formats are enormous and ambiguous, which is why they keep getting exploited. Rill's messages have one exact, rigid shape, and anything even slightly off gets the connection cut instantly. There's no fine print to exploit — and the whole thing is small enough that one person can actually check *all* of it, which nobody can honestly claim about the web's plumbing.

**Even if something got through, there's nothing to detonate.** Websites send your computer programs to run, so a malicious page can actually *do* things to you. Rill only ever sends documents — descriptions of text and buttons, more like a letter than a package with machinery inside. Even a hostile server can only send you a page to look at.

The honest fine print: this protects the connection between your devices — it can't protect a machine that's already compromised, and it's new, so it hasn't survived decades of real-world attacks the way the web's defenses have. Rill's answer is the third point above: instead of claiming to be smarter than everyone, it makes the security-critical parts small enough to get right.

In one line: the web is secure the way an airport is — layers of checkpoints run by strangers, patched every time someone sneaks through. Rill is secure the way your home is — you personally decide who has a key, and to everyone else, your valuables aren't just locked away; they're invisible.

## Where things stand

**Pre-alpha, and honestly so.** The protocol, content-addressed storage,
packaging, document format, app runtime, Wayland compositor, and the
first eight applications are built and running; as this is written, the
reference device — a 1 GB Raspberry Pi 5 — is days into an unattended
endurance run. But the wire formats and document vocabulary still change
weekly, there is no stability promise of any kind yet, and nothing here
has had outside security review. Don't package it, don't build on it
expecting the ground to hold still, and don't run it anywhere a breakage
would matter. If that reads as an invitation to explore rather than a
warning — it's both.

Where things live:

* [specs/](specs/) — the design documents and recorded decisions. The
  project is deliberately spec-first: the intent is that another engineer
  can go spec → invariant → feature without a walkthrough.
* [docs/](docs/) — the evidence: measured memory attribution, endurance
  protocols with raw data, a supply-chain audit, and the design lineage.
  Every performance or footprint claim is labeled measured, projected,
  or target — and the measured ones ship with their methodology.

The headline numbers, all **measured** (see
[docs/memory-footprint.md](docs/memory-footprint.md) and
[docs/pi-soak.md](docs/pi-soak.md) for method and raw data): the whole
desktop — compositor, server, dock, live widgets — runs in about
**46 MiB** on a 1 GB Raspberry Pi 5 at under 50 °C with no fan; each
additional app costs ~3 MiB; a window update travels as **0.4–2 KB** of
drawing commands; and an idle desktop draws essentially zero frames.

Some documents reference internal planning files (`risks.md`, `TODO.md`,
`proj-plan.md`) that aren't published — those citations are kept for
honesty about how decisions were made, not as links to follow.

## License

Dual-licensed under [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE),
at your option. Creative assets and their provenance are covered in
[CREDITS.md](CREDITS.md).

Unless you explicitly state otherwise, any contribution intentionally
submitted for inclusion in this project by you, as defined in the
Apache-2.0 license, shall be dual licensed as above, without any
additional terms or conditions.
