# Dependency & supply-chain audit — 2026-08-21

Scope: the workspace's 493-crate lock, checked against the RustSec advisory
database (cloned at HEAD, 1,205 advisories over 908 crates), the crates.io
sparse index, and the crates.io API. Method notes are at the bottom so the
numbers can be re-derived rather than trusted.

**Verdict: no vulnerabilities, no compromise, and one very close call that
the project won yesterday by accident of good practice.** Everything below
is hygiene, cohesion, or hardening.

**Actions 1–6 were applied on 2026-08-21; this section records the result.**

```text
✓1 cargo update -p blake3          arrayref GONE from the tree              §1
✓2 --locked in gate + 3 scripts    the pin is now binding                   §3.1
✓3 cargo audit wired, two ways     cached in the gate, fetching in cron     §3.2
✓4 narrowed the image codecs       rav1e/exr/avif-serialize gone            §3.3
✓5 dropped rustls-pemfile          a dependency removed, not swapped        §3.4
✓6 rustup 1.94.0 → 1.98.0 + pin    rust-toolchain.toml added                §3.6
 7 decide a date for wgpu          OPEN — 4 majors behind, and parses WGSL  §6, §7
   THIRD-PARTY notice for MPL      OPEN — release paperwork, not urgent     §5
```

Measured effect, whole workspace:

```text
                    before   after    note
lock                  493     454     −39 crates
rill-vector           258     209     −49 (codecs)
rill-compositor       246     219     −27 (codecs; +rill-history since)
rill-server            64      62
cargo audit          8 adv    0 vuln / 7 warnings, all listed with reasons
                              in .cargo/audit.toml so a NEW one fails loudly
tests                          59 suites green on 1.98.0
```

The toolchain bump was not free and is worth recording: 1.94 → 1.98 added
four clippy lints that failed the `-D warnings` gate across **eleven** sites
(`chunks_exact_to_as_chunks` ×9, `question_mark`, `manual_checked_division`,
`unnecessary_sort_by`). All were fixed rather than allowed — each was a
genuine simplification, and two dropped a `try_into().unwrap()` outright.
That experience is why `rust-toolchain.toml` now exists: the next bump should
be a deliberate act, not a surprise arriving on unrelated work.

Two findings in this document were **wrong in an earlier draft and are
corrected in place**: `ring` is not unmaintained (both advisories are
withdrawn or exempt our version), and upgrading `cosmic-text` clears one
advisory rather than three. Both corrections are stated where they occur
rather than quietly edited out, because the retraction is the useful part.

---

## 1. The headline: a live supply-chain attack, one day old

**RUSTSEC-2026-0260** — `arrayref` 0.3.10 was published on **2026-08-20**
with a dependency on `proc-macro1` (a typosquat of `proc-macro2`) whose
build script executed malicious code. crates.io removed it after roughly 86
minutes; it was downloaded 2,285 times.

**Rill is unaffected, and it is worth being precise about why.**

```text
locked version      arrayref 0.3.9      advisory says unaffected = "<= 0.3.9"
proc-macro1         absent from Cargo.lock and fuzz/Cargo.lock
                    never present in ~/.cargo/registry (never downloaded)
```

The advisory's own explanation of why most people escaped is the lesson:
*"most users had older versions of `arrayref` in their lockfiles."* A
committed `Cargo.lock` is what stood between this project and a malicious
build script — and `arrayref` is not peripheral here. It arrives via
`blake3`, which means it is in the dependency graph of **every Rill binary**,
underneath the content-addressing that the whole security model rests on:

```text
arrayref → blake3 → rill-store  → rill-app, notes-app, rill, …
                  → rill-history → rill-compositor, rill
```

**It was broader than one crate.** The maintainer account (`droundy`) was
compromised, and three crates it solely owns were published with the same
one-line change: `arrayref` 0.3.10, `internment` 0.8.7, and
`append-only-vec` 0.1.9 (RUSTSEC-2026-0260, -0262, -0264, -0265, -0266).
Only `arrayref` is in our tree. Two details worth carrying:

* The typosquat was **version-matched** — `proc-macro1 1.0.107` against the
  real `proc-macro2 1.0.107` that is in our lock — so in a lockfile diff the
  two sit side by side looking like a pair.
* The attacker **yanked the legitimate prior versions** to push resolution
  upward. Pinning is what defeated that; `--locked` is what makes pinning
  binding.

**There is now a one-command fix, and it is worth taking.** `blake3` 1.8.7
(published 2026-08-20, about two hours after the attack) **drops `arrayref`
entirely** — verified against the crates.io dependency API: 1.8.5 and 1.8.6
require `arrayref ^0.3.5`, 1.8.7 has no such dependency. The manifest says
`blake3 = "1"`, so:

```bash
cargo update -p blake3        # 1.8.5 → 1.8.7, removes arrayref from the tree
```

No manifest edit, and it removes the crate rather than trusting a version of
it.

---

## 2. Advisory status: 8 applicable, none of them vulnerabilities

After filtering the advisory DB to entries our exact versions do **not**
satisfy (and discarding withdrawn advisories):

```text
severity        crate            version   advisory            reachable via
──────────────────────────────────────────────────────────────────────────────
unsound         memmap2          0.8.0     RUSTSEC-2026-0186   sctk 0.19 → xkbcommon 0.7
unsound         cgmath           0.18.0    RUSTSEC-2026-0197   smithay 0.7
unmaintained    cgmath           0.18.0    RUSTSEC-2026-0196   smithay 0.7
unmaintained    paste            1.0.15    RUSTSEC-2024-0436   image defaults → exr → pulp
unmaintained    rustls-pemfile   2.2.0     RUSTSEC-2025-0134   rill-auth (DIRECT)
unmaintained    rustybuzz        0.14.1    RUSTSEC-2026-0206   cosmic-text 0.14
unmaintained    ttf-parser       0.20.0    RUSTSEC-2026-0192   cosmic-text → fontdb
unmaintained    ttf-parser       0.21.1    RUSTSEC-2026-0192   cosmic-text 0.14
```

`cgmath`'s unsoundness (RUSTSEC-2026-0197) is **not reachable in this
build**: it is confined to `Matrix{2,3,4}::swap_columns` called with two
identical indices, and `smithay` 0.7.0 never calls `swap_columns`. There is
no fixed version (`patched = []`) and `smithay` is already at its latest, so
it is un-actionable as well as unreachable. Note and move on.

**Zero vulnerabilities.** Everything with a CVE-class advisory in this tree
(tokio, rustls, rustls-webpki, ring, image, bytes, slab, time, zerocopy,
crossbeam, quick-xml, rand, hashbrown, …) is on a patched version — in
several cases *exactly* at the patch boundary, which suggests deps have been
kept current deliberately rather than by luck.

Two things my first pass got wrong and I want recorded so nobody re-derives
the scare: **ring's two "unmaintained" advisories do not apply.**
RUSTSEC-2025-0007 was **withdrawn** on 2025-02-22 (the rustls team took over
security maintenance), and RUSTSEC-2025-0010 marks `>= 0.17` unaffected — we
are on 0.17.14. `ring` is our TLS crypto backend, so a false alarm there is
worth explicitly retracting.

---

## 3. What needs addressing, in priority order

### 3.1 Build with `--locked` — the highest-value change here

`--locked` (or `--frozen`) appears **nowhere**: not in the pre-push hook, not
in `demo-desktop.sh`, `cross-build-pi.sh`, `bench-device.sh`, or `fuzz.sh`.
Every build is therefore permitted to re-resolve and silently rewrite
`Cargo.lock`.

For 86 minutes yesterday, a re-resolution touching `arrayref` would have
selected the malicious 0.3.10. The lock is the control that saved this
project; not enforcing it means the control is advisory.

```bash
# pre-push hook
cargo test --workspace --locked
cargo clippy --workspace --locked -- -D warnings
```

Cost: a dependency change now requires an explicit `cargo update`, which is
the point — lock drift becomes a deliberate act with a diff to review.

**This audit observed the drift happening, which is the tidiest possible
argument for the change.** Commit `fb1d634` ("The machine's memory turns
on") added `rill-history` to `platform/rill-compositor/Cargo.toml` but
committed `Cargo.lock` without the corresponding entry. Merely running
`cargo tree` during this audit silently repaired it:

```diff
  dependencies = [
   "rill-gpu",
+  "rill-history",
   "rill-ui",
```

So the committed lock was, until that moment, not a description of what the
workspace builds. Under `--locked` that commit would have failed its own
pre-push gate and been fixed in place. (The regenerated line is correct and
should be committed.)

### 3.2 Put a dependency check in the gate

No `cargo-audit`, `cargo-deny`, `cargo-vet`, or `cargo-outdated` is installed
or wired into anything. The gate catches broken tests and clippy warnings and
would not have noticed the advisory table above.

```bash
cargo install cargo-audit
cargo audit --deny warnings     # in the pre-push hook, or in scripts/fuzz.sh's cron slot
```

`cargo-deny` additionally covers the licence question in §5 and duplicate
detection in §4 in one config. Given the deferred-CI decision, the honest
placement is the same cron line that runs the fuzzers.

### 3.3 Narrow the `image` codec set — MEASURED −47 crates

`rill-viewport` and `rill-compositor` both declare `image = "0.25"` with
default features, which enables **all fifteen codecs**: avif, bmp, dds, exr,
ff, gif, hdr, ico, jpeg, png, pnm, qoi, tga, tiff, webp.

`ascii-app` carefully declares `default-features = false, features = ["gif"]`,
and the workspace manifest has a comment explaining that `image` is
deliberately *not* centralized so that narrowness survives. Verified: that
care holds for `files-app` (gif only). It is simply absent from the two
crates that matter most.

This is the widest *remotely reachable* parser surface in the project.
`rill-viewport` decodes images referenced by documents served from a remote
app (`image "/assets/x.avif"` → `image::load_from_memory`, which sniffs the
format), so every enabled codec is attacker-selectable by any server a user
visits.

The cost is not theoretical. `image`'s defaults pull **`rav1e` 0.8.1, a full
AV1 video encoder** (49 crates on its own) and `exr` (32 crates), for
*encoding* formats nothing in Rill encodes:

```text
                       before   after (png,jpeg,gif,webp)   delta
rill-vector             258            211                  −47   (−18%)
rill-compositor         246            200                  −46   (−19%)
```

MEASURED by editing both manifests, re-resolving, and counting unique
normal-dependency crates for the linux-gnu target; both manifests and the
lock were reverted afterwards. It also removes `paste` (RUSTSEC-2024-0436),
which arrives only through `exr`.

Behavioural cost, stated plainly: both call sites use `load_from_memory` and
already handle `Err`, so a user whose wallpaper is a TIFF gets an error
instead of a wallpaper. Pick the format list against what wallpapers and app
assets actually need — `png, jpeg, gif, webp` covers it, and `bmp`/`ico` are
cheap to add back if desired.

### 3.4 Replace `rustls-pemfile` — the only *direct* advisory

`rill-auth` depends on `rustls-pemfile` 2.2.0 directly. The crate is
archived (Aug 2025) and unmaintained (RUSTSEC-2025-0134). It is already a
thin wrapper over code that now lives in `rustls-pki-types` — which is
**already in our tree** as a transitive dependency of rustls, so the
migration removes a dependency rather than trading one for another.

The replacement is the `PemObject` trait (`rustls_pki_types::pem`).
`rustls-pki-types` 1.15.1 is current. Small, contained change in one crate.

### 3.5 The unmaintained font stack — smaller win than it first looks

`rustybuzz` and `ttf-parser` (×2 versions) are all unmaintained, all reached
through `cosmic-text` 0.14.2. cosmic-text 0.19.0 has migrated to `harfrust`
and `skrifa`, which is what both advisories recommend.

**Correction to an earlier draft of this document, which claimed the upgrade
clears three advisories. It clears one.** Verified against the crates.io
dependency API:

```text
rustybuzz   RUSTSEC-2026-0206   CLEARED — 0.19 uses harfrust instead
ttf-parser  RUSTSEC-2026-0192   PERSISTS — cosmic-text 0.19 → fontdb ^0.23,
                                which still requires ttf-parser ^0.25, and
                                the advisory has patched = [] so every
                                version is flagged
memmap2     RUSTSEC-2026-0186   UNRELATED — in this tree memmap2 0.8.0 comes
                                via xkbcommon 0.7 ← sctk 0.19 (§3.6), not
                                via fontdb, whose memmap2 dep is optional
                                and not enabled here
```

The caveat that makes this non-trivial: `rill-gpu` takes `swash` as a
*direct* dependency on purpose, with a manifest comment explaining it must
match "the version cosmic-text locks", because cosmic-text 0.14 cannot drive
the `wght` variation axis and rill-gpu drives it itself. cosmic-text 0.19
replaces that whole font stack with skrifa, so the custom variable-weight
scaler needs reworking onto skrifa (or onto whatever 0.19 exposes).

Contained but real: the surface is two files, `crates/rill-gpu/src/atlas.rs`
and `src/text.rs`. So the honest trade is *rework the variable-weight glyph
path onto skrifa, to clear one unmaintained advisory* — which is worth doing
eventually and is not worth doing soon. It ranks below everything above it.

### 3.6 Update the toolchain, and pin it

```text
this machine    rustc 1.94.0 (2026-03-02) / cargo 1.94.0 (2026-01-15)
latest stable   1.98.0 (2026-08-18)
rust-toolchain.toml   absent
```

Three 2026 Cargo CVEs land in that gap. **CVE-2026-33056** is the one that
matters in principle — a malicious crate could change permissions on
arbitrary directories during extraction, fixed in 1.94.1. It is mitigated
here twice over (crates.io deployed a server-side block on 2026-03-13, and
this project resolves solely from crates.io with no alternate registries or
mirrors), as are CVE-2026-5222 and -5223, which only affect alternate
registries. So: not urgent, but `rustup update` is free.

Worth more than the CVEs: **there is no `rust-toolchain.toml`.** The
workspace declares `rust-version = "1.88"` as a floor, but nothing pins the
toolchain a build actually uses. For a project whose reproducibility story
includes cross-building an appliance image, the toolchain is an input like
any other.

### 3.7 Watch the right channel

On **2026-02-13 crates.io stopped publishing a blog post for every malicious
crate** — the stated reason being that it had become noise rather than
signal. The policy now is: a RustSec advisory *always*, a blog post only for
crates with real usage. `arrayref` got both because of its size; the next one
may not.

**The RustSec advisory feed, not blog.rust-lang.org, is now the authoritative
channel for crates.io malware.** Worth a subscription, and worth knowing
before assuming silence means safety.

### 3.8 Not actionable: the smithay duplicate cluster

Most Linux-side duplicate versions trace to one root — `smithay-client-toolkit`
0.19.2 pulling `calloop` 0.13 (→ `rustix` 0.38, `thiserror` 1.0) and
`xkbcommon` 0.7 (→ the unsound `memmap2` 0.8.0), while `smithay` 0.7 uses
calloop 0.14.

SCTK 0.21.1 exists. **Do not bump it.** `winit` 0.30.13 — which `smithay` 0.7
depends on — pins `smithay-client-toolkit ^0.19.2` and `calloop ^0.13.0`, so
raising `rill-vector`'s direct dependency would add a *second* SCTK copy
rather than removing the old one. This cluster is gated on smithay updating
winit, and is correctly left alone until then. `cgmath`'s two advisories are
inside `smithay` itself, same situation.

---

## 4. Cohesion

Structurally clean, and better than typical:

* **Every dependency comes from crates.io.** No git dependencies, no
  `[patch]`, no `[replace]`, no `.cargo/config.toml` overrides — nothing
  bypasses registry review or checksum verification.
* **Lock integrity is exact**: 493 packages, 468 with `source` + `checksum`,
  and 493 − 468 = 25 = the workspace's own path crates. No unverified entry.
* **No yanked versions** among the direct dependencies (checked against the
  sparse index).
* **Feature discipline is genuinely good.** No `tokio = { features = ["full"] }`
  anywhere; each crate names precisely what it uses (`rill-wire` takes only
  `io-util`). The workspace manifest centralizes versions with comments
  explaining *why* each is shared, and deliberately excludes `image` for the
  reason discussed in §3.3.
* **Duplicates**: 42 crates appear at more than one version, but most are
  Windows/macOS target crates that never compile on Linux. The Linux-relevant
  set is the smithay cluster (§3.6) plus `winnow` ×3 and `getrandom` ×3, both
  of which are ordinary ecosystem-transition churn.

Per-binary dependency counts (linux-gnu, normal deps):

```text
rill-server         64      lean; the security-critical surface is the small one
rill                77
files-app          125      +50 from the music app's audio stack
rill-compositor    246      wgpu/naga/smithay/winit dominate
rill-vector        258
```

`rill-server` at 64 crates is the number worth protecting — it is the process
that faces the network.

---

## 5. Licensing — one obligation, not a blocker

All 468 registry crates declare a licence; none is missing one. The
distribution is overwhelmingly MIT/Apache-2.0, compatible with the project's
own `MIT OR Apache-2.0`.

**The exception: all 12 `symphonia-*` crates are MPL-2.0**, reaching the tree
through `rodio` in the music app. MPL-2.0 is file-level weak copyleft, so it
does not infect Rill's own source, but §3.2 of the licence does create a
distribution obligation: binaries that include symphonia (the appliance
image, any released `files-app`) must carry the notice and tell recipients
how to obtain the MPL-covered source.

This matters because it is a **release-blocking paperwork item** for both the
open-source release and the appliance image, and it is invisible until
someone asks. Two remaining minor cases are non-issues: `self_cell` is
"Apache-2.0 OR GPL-2.0-only" (take Apache) and `r-efi` offers LGPL among
MIT/Apache options (take either).

Recommendation: a `CREDITS`/`THIRD-PARTY` generation step (`cargo-about` or
`cargo-deny`'s licence output) before the source release, which also settles
the separate Shadertoy-attribution debt already tracked in TODO.md.

---

## 6. Version currency

Most of the stack is current. Where it is not, the gap is usually justified:

```text
crate                    locked        latest      note
─────────────────────────────────────────────────────────────────────────────
tokio                    1.53.1        1.53.1      current
rustls                   0.23.43       0.23.43     current
image                    0.25.10       0.25.10     current
rodio                    0.22.2        0.22.2      current
smithay                  0.7.0         0.7.0       current (upstream is the constraint)
kdl                      6.5.0         6.7.1       minor, safe
cosmic-text              0.14.2        0.19.0      5 minors — see §3.5
smithay-client-toolkit   0.19.2        0.21.1      do NOT bump — see §3.6
wgpu                     26.0.1        30.0.0      4 majors behind
blake3                   1.8.5         1.8.7       §1 — drops arrayref
rcgen                    0.13.2        0.14.9      0.13 line is dead-ended
pollster                 0.4.0         1.0.1       1 major
```

**`rcgen` 0.13.3 is yanked**, so 0.13.2 is the last usable release of that
line — the 0.13 series has nowhere to go and any future fix lands on 0.14.
The yank is *not* security-related (rcgen has never had a RustSec advisory);
the reason could not be confirmed from an authoritative source. `rcgen`
generates the device and server certificates, so being stranded on a
dead-ended line is worth knowing even though nothing is wrong today.

**wgpu is the one to think about deliberately.** Four majors is a lot of
drift for the crate the entire renderer sits on, and the gap compounds: each
major skipped makes the eventual migration larger, and wgpu majors routinely
move `naga`, binding, and surface APIs. This is not urgent — nothing here is
a security matter, and the renderer works — but it is the sort of debt that
becomes a multi-week wall if left until something forces it (a new GPU, a
driver bug fixed only upstream, or a Vulkan feature the Pi needs). Worth a
deliberate decision about *when*, rather than discovering the answer.

---

## 7. Native code and build-time execution

The C surface on Linux is small and expected: `zstd-sys` (bundles zstd
1.5.7), `alsa-sys` (links libasound, from rodio), `wayland-sys`. Plus `cc`
as a build-time tool. Nothing surprising, and no crate compiles code from a
network source at build time.

**At least 66 of the resolved crates ship a `build.rs`** (counted over those
present in the local registry cache, so a floor rather than a total).
Seventeen more are proc-macros. Both categories run arbitrary code *on the
developer's machine at compile time*, with the developer's privileges and no
sandbox — which is precisely the primitive `proc-macro1` used, and before it
`rustdecimal` (2022), the `amaperf` wave (2023), and `onering` (2026-06,
maintainer account compromised, exfiltrated the victim's git diff disguised
as telemetry).

Notable build-script crates in this tree: `ring`, `rustls`, `serde`, `libc`,
`proc-macro2`, `wgpu`/`wgpu-core`/`wgpu-hal`, `naga`, `smithay`, `wayland-*`,
`zstd-sys`. All mainstream; that is exactly what the `arrayref` maintainer's
crates were too. No sandbox for build scripts exists or is on a shipped
roadmap, which is why §3.1 and §3.2 are the whole defence.

### One Rill-specific exposure worth a deliberate decision

`naga::front::wgsl::parse_str` is called on **user-supplied shader source**
(`crates/rill-gpu/src/lib.rs:1727`, `:2191`, `:2333` — a preamble is
concatenated with rice-supplied WGSL and handed to the parser). `naga` has
never had a RustSec advisory, and the shader-trust decision already recorded
in `specs/theming.md` limits *where* such shaders may come from.

But it is an untrusted-input parser inside the threat model, and it is the
component **four majors behind** (§6). That combination — a parser eating
attacker-influenceable input, on a version line that no longer receives
fixes — is the argument for treating the wgpu upgrade as a security decision
with a date on it rather than as ordinary version drift.

---

## Method

```bash
# advisory DB, matched against exact locked versions with semver ranges,
# discarding `withdrawn` advisories and honouring `unaffected`
git clone --depth 1 https://github.com/RustSec/advisory-db

# duplicates and per-binary counts, linux-gnu only, normal deps only
cargo tree -p <bin> --target x86_64-unknown-linux-gnu -e normal --prefix none

# feature resolution as actually built (not as declared)
cargo tree -p <bin> --target x86_64-unknown-linux-gnu -e features

# yanked check
curl https://index.crates.io/<prefix>/<crate>
```

Two figures here are MEASURED (the §3.3 crate counts, by editing manifests
and re-resolving; reverted afterwards). Everything else is read from the
lock, the advisory DB, or the registry. No claim in this document is an
estimate.

Not covered, and worth knowing it is not: this audit reads metadata. It does
**not** review dependency source code, and it cannot detect a malicious crate
that has no advisory filed against it yet — which is precisely the window the
`arrayref` attack lived in for 86 minutes. `cargo-vet` (§3.2) is the tool
that addresses that class, at the cost of a review ledger.
