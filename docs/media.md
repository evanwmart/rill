# What a frame carries — the visible set, and media

Status: **positions taken, four things built, the rest ordered but unbuilt.**
Written 2026-08-20 out of a design conversation that started from a single
audit finding (images never rendered) and ended somewhere larger. Companion
to [risks.md](risks.md) (how not to die), [resource-envelope.md](resource-envelope.md)
(what it costs today), `specs/wgpu-renderer.md` (the command stream) and
`specs/history.md` (what gets kept).

Nothing below is a spec. Where something is genuinely undecided it says so
rather than picking by implication.

## Why this document exists

Rill's command stream makes user interface nearly free. A window is drawing
instructions rather than pixels, so a full screen of text, controls and
chrome costs about a kilobyte and an idle desktop costs nothing at all.

The consequence is arithmetic: **once everything else approaches zero, media
is essentially the entire cost.** A photograph is a photograph — no
architecture compresses it away. In a pixel-based system that photo is noise
against a budget already being paid every frame; here it is the budget.

This is not a flaw and not a regression. The frugality is what buys the
headroom that makes media affordable on a 1 GB machine at all. But it does
mean media is the axis along which this system will be judged, and it will
not improve by accident.

## The finding that reframed this

The document began as a media plan. Measuring it turned up something wider,
and media is better understood as its first symptom.

**A frame described the whole document rather than the part on screen.**
Nothing culled to the visible region — not layout, not the client, not the
compositor. A two-hundred-line document in a two-hundred-pixel window emitted
all two hundred text commands.

That is free while a page fits and linear in the document once it does not.
Measured at 1280x800, before it was fixed:

| | 1,000 rows | 10,000 rows | on screen |
|---|---|---|---|
| log lines | 92.8 KiB/frame | 927.8 KiB/frame | 0.4% |
| file rows | 660.2 KiB/frame | **would not encode** | 0.2% |

The last cell is the point. A ten-thousand-file directory exceeded the
frame's path-point budget — icons are fill paths — so the window could not be
drawn at all. Not slow: impossible. And that is a directory people have.

So the thousand-to-one advantage the command stream claims was conditional on
the page fitting the window. Past that it decayed linearly and then stopped
working.

**Fixed by culling paint commands outside the window plus a screenful of
margin either way**, which makes frame cost flat in document length:

| | 1,000 rows | 10,000 rows |
|---|---|---|
| log lines | 6.5 KiB | 6.5 KiB |
| file rows | 24.5 KiB | 24.5 KiB |

Paint only, deliberately: clip commands must stay or the push/pop stack stops
balancing, and interaction commands must stay because the host reads them out
of the frame — dropping an off-screen field would quietly change what Tab
reaches.

**Why this belongs in a media document.** Culling is the general form of the
problem media made visible. An image nobody draws is an image nobody sends,
so the visible set is the same idea at a different scale, and it subsumes
part of the residency work below. The ordering in this document changed
because of it.

### The same finding one level down

The frame went flat and the pixels behind it did not. Measured at 1280x800,
before the residency half was fixed — a roll of 1600x1200 photographs, two of
them on screen:

| photos in the document | client RAM | offered to the host |
|---|---|---|
| 4 | 29.3 MiB | 14.6 MiB |
| 12 | 87.9 MiB | 14.6 MiB |
| 24 | 175.8 MiB | 14.6 MiB |

7.3 MiB per photograph, held for the life of the page whether or not anything
drew it, which is 732 MiB for a hundred of them on a machine with 1024. The
frame in the same measurement was 0.1 KiB and did not move.

**Fixed by keeping what is off screen at a floor** — reduced to about 64 px on
the long edge, roughly 30 KiB — and offering the host only the visible set:

| photos in the document | client RAM | offered |
|---|---|---|
| 4 | 14.7 MiB | 14.6 MiB |
| 12 | 14.9 MiB | 14.6 MiB |
| 24 | 15.3 MiB | 14.6 MiB |

Six times the document for 0.6 MiB more. What is on screen dominates, which is
the shape the whole design claims. The high-water mark on the way there is
15–37 MiB — bounded by the window, not the document — and most of it is the
copy still being painted while its replacement is prepared.

A floor rather than nothing, because the two failure modes are not symmetric:
releasing an off-screen picture entirely means a scroll arrives at an empty box
and waits on a fetch, while a floor means it arrives blurry and sharpens. The
mechanism is one that already existed — the reduced copy is a mip level, and
the refine-on-zoom path is already streaming-in.

Four details that are load-bearing rather than incidental:

* **Every first decode goes to the floor**, and the full-size decode never
  leaves the worker. Before a picture is decoded nobody knows how big it is, so
  layout gives it a placeholder box — and a screenful of placeholders is a
  dozen pictures where the real sizes turn out to be two. Trusting that guess
  held 80.9 MiB on the way to a settled 15.3. Layout knows the real size a
  frame later and asks for detail then.
* **Sharpening waits for the scroll to stop.** Flicking through a
  forty-picture roll and back cost 35 refetches each way — one per picture
  passed, each a source read and a decode, for a scroll that showed a blur.
  Refining only when the view is not travelling brings that to 5 each way (the
  pictures it actually came to rest on). It is a rule about intent rather than
  a timer, so it holds at any scroll speed.
* **Neither the decode nor the rescale happens on the frame path.** Both are
  hundreds of milliseconds for a photograph, and both used to run inside `poll`
  and `layout` — so a window drag that crossed a halving boundary stopped to
  rescale everything on screen. Measured over a 120-step drag of a
  sixty-photograph roll, debug build: 12,166 ms of layout before culling, 1,154
  ms after it with a 401 ms frame inside, and 10 ms with no frame over 1 ms
  now. Neither job is urgent — a coarsening has no visible effect, and a
  picture shown larger keeps painting what it has — so both go to a worker.

  The worker's queue has two ends, which turned out to matter as much as the
  thread did. One queue in arrival order put the photograph the reader is
  looking at behind the fifty-nine they are not: the top of the roll stayed
  coarse for 879 ms in a release build and had not sharpened after four seconds
  in a debug one. Detail for what the window shows now goes to the head of the
  queue, first decodes and coarsenings wait their turn, and the first paint is
  coarse for 26 ms in release.

* **Detail also waits for the window's shape to stop.** The scroll rule reads
  intent from distance-to-target; a resize has no target, so a 150 ms clock
  is the honest version — a drag refreshes it continuously, and the moment
  the hand stops it runs out and the settle repaint asks for what was
  deferred. Without it, a drag wobbling across a halving boundary re-read and
  re-decoded every visible photograph per crossing, and re-sent it —
  measured at 5 refetches for 20 crossings without the rule and 0–2 with,
  and re-send traffic across a 40-crossing storm went from a re-send per
  crossing to none.

The client also marks coarse pixels *provisional*, and a forwarding host does
not send those over a sharper copy the compositor still holds — otherwise
scrolling back to a picture would downgrade it for as long as the refetch
takes. A deliberate reduction, a window dragged narrower, is not provisional
and is sent, so the compositor does stop holding pixels nobody needs.

### The transport under stress — swept 2026-08-20

Rapid interactive resize killed the window, reproducibly:

```text
request could not be marshaled: can't send file descriptor
Error sending request for rill_stream_v1.attach: Broken pipe
```

That string is libwayland's: its outgoing fd ring holds roughly a thousand
descriptors, every queued fd-carrying request (each frame's memfd, each
image's memfd) parks one there until the socket drains, and when the ring is
full the next `attach` kills the connection. Three of our behaviours filled
it, in a stack:

1. **The configure handler drew unconditionally.** An interactive resize
   delivers a configure per pointer motion; a fast mouse made that hundreds
   of full layout–encode–memfd frames a second, bypassing frame-callback
   pacing (`frame_pending` was explicitly reset). Fixed: configure reflows at
   the frame callback's pace — the drawn frame uses the latest size, the
   sizes in between were never needed — with a 100 ms lost-callback guard so
   the first configure still maps the window.
2. **A full socket was treated as permission to keep producing.**
   `WouldBlock` on flush was "not fatal, carry on", so a stalled compositor
   turned the queue into unbounded fd parking. Fixed: congestion stops frame
   and image production (the frame is deferred, not lost), the event loop
   polls for writability, and the deferred frame goes out when the pipe
   drains. Input keeps flowing throughout.
3. **Every halving crossing re-sent megabytes.** The same drag that produced
   the configure storm also produced a ~15 MiB image re-send per boundary
   crossing — the traffic the compositor was stalling on. Fixed by the
   shape-settle rule above, plus its transport half: the send decision is a
   pure function (`plan_image_send`) whose clauses are the flow control — a
   picture the compositor lacks always goes (a hole is worse than traffic),
   a stand-in never downgrades a sharper copy, and size changes wait for the
   shape to settle. Mid-drag the compositor scales the copy it holds into
   the new rect, which nobody can see at drag speed.

Weaknesses surveyed and left open, in rough order of value:

* **The compositor reads image payloads synchronously in the dispatch
  handler** — up to 64 MiB of `read_exact_at` between input events. The seals
  on the memfd make mmap safe; mapping and deferring the copy to texture
  upload would move the cost off the dispatch path.
* **A coarser copy should never need the wire at all.** The compositor holds
  the sharper texture; halving is something it could do itself. That would
  delete the deliberate-reduction re-send entirely — wire traffic would only
  ever *add* detail — without the client learning anything about the GPU.
* **One memfd per frame at 60 Hz** is fd churn that congestion now bounds
  but reuse would eliminate; needs a buffer-release ack the protocol does
  not have. Cheap to live with, so low priority.
* **`sent_images` assumes delivery.** Same-socket ordering plus
  `image_released` keeps it honest today; any future out-of-band transport
  breaks that silently.

## Statline — the image system at a glance

Measured 2026-08-21 on the x86 dev box, debug builds, unless labelled
**cap** (a design constant) or **Pi** (measured on target). The Pi column of
truth is thin until the weekend session; treat every x86 latency as a shape,
not a promise.

**By size** — shown size decides shipped size:

| the picture | what moves / stays resident |
|---|---|
| 4000x3000 photo in a 240px slot | 250x188 sent — 183.6 KiB against 45.8 MiB decoded at source |
| 1600x1200 photo shown full-width (1232px) | 7.3 MiB, the dominant unit cost |
| any picture off screen | floor copy, ~64px long edge, ≈29 KiB |
| any picture, in the frame itself | ~85 bytes — a source string and a rectangle |
| reductions | powers of two only; never upscaled; refetch on growth needs the shape/scroll still |

**By type:**

| kind | handling |
|---|---|
| raster in documents | decoded by `image` 0.25 defaults — PNG, JPEG, GIF, WebP, BMP, TIFF, QOI, TGA, EXR — all to RGBA8 |
| vector | native in the command stream: no decode, no residency, survives recording |
| GPU texture compression | none yet — RGBA8 textures; format waits on the V3DV probe (**Pi, open**) |
| video | undecided (promise-or-boundary, open question) |

**By count** — cost follows the window, not the document:

| | 60 photos as a roll | same 60 as a 240x180 grid |
|---|---|---|
| frame, per frame | 0.1 KiB | 1.2 KiB |
| client RAM, settled | 37.4 MiB | 6.2 MiB |
| sent to compositor | 14.6 MiB (2 in view) | 6.0 MiB (15 in view) |
| load peak | bounded by window, 15–37 MiB | same bound |

The roll's 37 MiB is the design, not a leak: visible plus one screenful each
way resident at display size, the other ~55 pictures at the floor.

**Churn under interaction** (count, not time — time is machine-dependent):

| | refetches |
|---|---|
| flick over 40 pictures and back | ~5 each way, the ones it rested on |
| resize storm, 20 boundary crossings | 0–2 |
| coming to rest, any of these | sharpens; release build ~26 ms coarse |

**Floors and caps:**

| | value | kind |
|---|---|---|
| frame size | 4 MiB | cap |
| frame path budget | 65,536 points | cap |
| compositor images, per surface | 64 MiB, evict + recall beyond | cap |
| resident floor | 64 px long edge (viewport constant — becomes a device number in build item 1) | cap |
| shape settle | 150 ms | cap |
| idle desktop, whole stack | 28–34 MiB PSS | **Pi**, release, 2026-08-15 |
| client with the 60-photo roll | ~72 MiB RSS | x86 debug — shape only |

## What is built

**Pixels take a second door.** A frame names an image by source string and
never carries it (`rill_stream_v1::attach_image`, protocol v2). Pixels travel
out of band as raw RGBA over a sealed memfd, once per source, and the
compositor uploads them to a per-surface texture.

**The client resolves images, not the compositor.** It already must — the
layout box comes from the image's natural size — and it is the only party
with an identity to resolve them under. A compositor fetching on a client's
behalf would be a desktop-wide deputy any client could aim at any path by
naming it in a document, and the render-or-placeholder outcome would report
whether that path exists. Full reasoning in `specs/wgpu-renderer.md` W4.

**Images are reduced to the size they are shown at** before they are sent, by
successive exact halvings (powers of two, so window-drag does not re-scale
every frame, and an exact halving is a 2×2 average rather than a point
sample). Never upscales.

**What the window is not showing falls to a floor, and is not sent.** The
client keeps a coarse copy of everything in the document and a display-size
copy only of what is in the band, so RAM follows the window rather than the
document. Reproduce with `cargo test -p rill-viewport --test image_residency --
--ignored --nocapture`.

**The client stops holding the original** once the reduction exists. Layout
only ever asks how big a picture is — two integers — and the compressed source
is on disk already. Shown larger later, it fetches the source again from its
own cache and keeps painting the coarse copy until the finer one lands.

**Reaching the compositor's per-surface budget evicts rather than refuses.**
What the current frame does not name goes first, least recently shown, and
`image_released` tells the client so the next frame that needs it re-attaches.
The budget bounds the working set, not the window's lifetime — it previously
bounded the lifetime, so a window that browsed enough pictures stopped showing
new ones for good.

### Measured, 2026-08-20, this machine

Per-frame wire cost, 1280×800 viewport:

| page | per frame |
|---|---|
| text only | 0.1 KiB |
| gallery, 12 thumbnails | 1.1 KiB |
| gallery, 48 thumbnails | 4.1 KiB |
| *the same window as a pixel buffer* | *3.9 MiB* |

About 85 bytes per image — a source string and a rectangle. Pictures do not
enter the recurring cost, which is the property the whole design rests on and
is now pinned by a test.

One-time payload, gallery built from phone photos (4000×3000):

| | at source | as sent |
|---|---|---|
| hero | 45.8 MiB | 11.4 MiB |
| one thumbnail | 45.8 MiB | 183.6 KiB |
| hero + 48 thumbs | 2243.0 MiB | 20.1 MiB |

Reproduce with `cargo test -p rill-viewport --test image_cost -- --ignored --nocapture`.

Compressed-texture support, probed on this machine (**not** the target):

```
RTX 5070      (Discrete,   Vulkan)   BC yes   ETC2 no    ASTC no
Ryzen iGPU    (Integrated, Vulkan)   BC yes   ETC2 no    ASTC no
RTX 5070      (same GPU,   GL)       BC yes   ETC2 yes   ASTC no
```

The third row is the trap: OpenGL 4.3 *mandates* ETC2, so desktop drivers
accept it and decompress in software. "Supported" through GL can mean the
format works and the memory saving does not. Vulkan reports hardware; GL
reports the API contract.

## Positions

### 1. Vector assets have gravity, and it is structural

Vector content gets properties raster cannot have here. It responds to the
theme (a vector mark takes a colour token; a PNG is stuck with the colours it
was saved with). It is resolution-free, so no downscale step, no re-send on
zoom, no format negotiation. It is searchable, being structure rather than
pixels. And it **survives recording and history**, because it lives in the
command stream, where raster by construction does not.

That last asymmetry is worth more than any amount of encouragement in
documentation: a session replay shows vector assets perfectly and photographs
as grey boxes.

The consequence is a pipeline, not a preference. **Converting raster assets
to vector at ingest removes them from the media problem entirely** — a traced
logo stops being an image and becomes part of the frame. Tracing works well
on logos, icons, line art, diagrams, charts, UI screenshots, scanned text,
maps. It fails badly on photographs, and the failure mode matters: a tracer
on a photo either posterises it or emits tens of thousands of paths that are
*larger than the JPEG*. So this is not a compression strategy for photos. It
is a way to shrink the set of things that are photos.

The decision must be measured, not judged: trace it, keep the vector only if
its path data beats the compressed raster *and* the path count is under a
bound. And it belongs at ingest or pack-build time — tracing costs hundreds
of milliseconds to seconds and has no business on a request path.

This is also what makes low-power displays plausible. E-ink is 1-bit or 4-bit
with slow refresh; vector content rasterises to the device's native depth at
render time and looks correct, where a photograph looks bad however it is
transported.

### 2. Residency: hashes everywhere, pixels only at the ends

A live image currently exists in up to four places at once. It should exist
as *bytes* in two:

| | holds |
|---|---|
| origin server | bytes + hash (authoritative) |
| client disk cache | compressed bytes + hash (already content-addressed) |
| client RAM | **hash + natural dimensions only** |
| compositor | GPU texture + hash |

The client should be a pass-through rather than a cache: fetch, decode,
reduce, send, drop the pixels, keep about forty bytes. It already stores the
compressed original on disk, so retaining decoded RGBA in RAM is pure
redundancy; a later zoom re-reads its own cache, which is local.

Hash matching is the transfer rule — "I already have that one" costs nothing
— and it dedupes across windows for free. Key on the **pair** of (source
hash, reduction step), because the compositor holds a reduction and two
windows showing one photo at different sizes genuinely need different
textures.

### 3. Change frequency is the taxonomy, and most of it is not a transport problem

| kind | answer |
|---|---|
| static raster | what is built today |
| periodic raster (a chart image that redraws) | make it vector — then the diff is free |
| continuous vector | already solved; the stream only ever sends the frame |
| mixed (static base, moving part) | layered composition, which already exists |
| continuous raster (video) | a codec and hardware decode, or a stated boundary |

Deliberately **not** building a diff or motion-estimation scheme. For
continuously-changing raster that is H.264/AV1, with decades of prior art and
hardware decoders that exist precisely because software is too slow. For
everything above it, the command stream is already the diff.

### 4. A long-running process must have a bounded working set

Bounded by what is *visible*, not by what has been *visited*. This is a
general principle rather than a media one, and media is where it bites first.

It requires eviction and a recall path together: eviction without recall is
an image that silently never comes back.

### 5. Recording is declarative-first; media is the exception

Storing screenshots for agent reference is the expensive, lossy, unsearchable
thing this project's history design exists to reject. Declarative steps are
the differentiator. So media in recordings is opt-in, and the current
accidental behaviour (raster absent from replays) is closer to right than
wrong — it needs to become a stated policy rather than an artifact.

The tier mechanism already exists (`T0_ROUTINE`, `T1_SENSITIVE`, `T2_SEALED`,
with a test proving sealed content stays out of the routine index). The
question is who sets the tier, not what to build.

**A server's "do not record" is advisory and must be labelled as such.** The
client decoded those pixels to display them; it holds them; a hostile client
ignores the flag and a camera defeats it regardless. It is useful in the same
way `Cache-Control: no-store` is useful — with a cooperating client — and
worthless as enforcement.

It should stay advisory even if it could be enforced. **A server that can
forbid recording can hide what it showed you.** The history is a record the
user keeps about their own machine; a remote party erasing itself from it is
a worse property than a remote party asking for discretion. Server hints,
honoured by default, user holds the override, override is visible.

## Prior art — where this stands against the software people actually use

Written 2026-08-21. Rill's numbers are measured; the comparison figures are
from public architecture documentation and experience, **not measured here** —
right order of magnitude, not data. Latency comparisons stay unfair until the
Pi session: ours are debug x86, theirs are release builds tuned for a decade.
The RAM comparison is fair.

The sixty-photograph gallery:

| | Rill (measured) | Chromium-class browser | GTK/Qt photo app |
|---|---|---|---|
| decoded-image RAM | 6.2 MiB (grid) | tens to ~150 MiB, bounded by its discard cache | app-dependent; naive ones hold every decode |
| baseline footprint | 28–34 MiB PSS, *the whole desktop* (Pi) | 300–500 MiB across processes for one tab | 80–200 MiB |
| window cost per frame | 0.1–1.2 KiB of commands | display list + textures — also cheap | pixel buffers, 4 MiB × 2–3 |
| off-screen images | ~29 KiB floor, evict + recall | discards decodes, re-decodes on scroll | usually nothing |

The baseline row is the structural point: a browser's gallery *tab* costs an
order of magnitude more than this whole desktop — the price of generality
(JS, arbitrary CSS, fonts, codecs), which is exactly the price the command
stream exists to not pay.

**Convergences** — things this design arrived at by measurement that the web
spent a decade arriving at by pain, which is decent evidence for the design:
the declared image box that never reflows is the web's `width`/`height` +
`aspect-ratio` (invented to kill Cumulative Layout Shift); the resident floor
is LQIP/blur-up, live instead of a build step; contain-and-letterbox is
`object-fit: contain`, except honest by default where the web defaults to
distortion; culling plus eviction is off-screen image discarding plus
virtualized lists; detail-waits-for-stillness is scroll-debounced decoding.

**Borrowables** — the three genuine edges the incumbents hold, in value
order:

1. **Decode-to-scale.** libjpeg-turbo decodes a JPEG *at* 1/2–1/8 scale, so
   full-resolution pixels never exist; a 240px thumbnail of a 4000x3000
   photograph touches ~1/64th of the pixels we decode. Our peak-per-photo is
   the full decode. The `image` crate does not expose this; `zune-jpeg` or
   turbojpeg bindings do. Matters most on the Pi, and could reorder the build
   list after the measurement session.
2. **A persistent floor.** The freedesktop thumbnail cache keeps 128–512 px
   thumbs on disk across sessions; our floor is recomputed per session, so
   revisiting a gallery re-decodes everything once. A disk-backed floor fits
   the content-addressed cache naturally.
3. **SIMD JPEG decode.** Browsers ride libjpeg-turbo's assembly; ours is
   pure Rust, plausibly 2–5x slower — invisible on this machine, possibly
   the difference between 26 ms and 100+ ms of coarse on a Cortex-A76.

Deliberately absent from that list: GPU texture compression (browsers ship
RGBA8 like we do) and per-frame efficiency, where the command stream simply
wins against a pixel-buffer window.

## Build order

**ON HOLD pending the Pi measurement session (planned for the weekend of
2026-08-22).** Every remaining item leans on an answer only the target can
give: Mailbox support under V3DV decides whether the present-mode fix
transfers, the compressed-format probe decides item 4's format, the
Cortex-A76 decode cost decides the floor/ceiling defaults in item 1 and
whether the single scaler worker needs a second. Building any of it against
x86 numbers would be optimising the wrong intercept.

The same session should also weigh **decode-to-scale** (prior art, borrowable
1) against this list: if scaled JPEG decode lands, it cuts the decode cost
and the transient peak at once, which may buy more than several items below
for less work.

Items 1, 2, the culling above and its residency half are done. What remains,
reordered by what measuring taught:

1. **A device ceiling, alongside the display-size rule.** Display size is the
   mechanism; a ceiling is the policy. A full-screen hero on a 4K panel is
   genuinely 4K worth of display size, and a Pi driving a small screen wants
   an upper bound regardless. Floor, ceiling and display-size-between are
   three device numbers, which is also how an e-ink profile becomes
   configuration rather than a special case.
2. **Zero-copy upload on unified memory.** No format change: on shared memory
   the right Vulkan heap lets the GPU sample pixels written directly, with no
   staging copy. Measure before assuming — it may be most of what texture
   compression would buy, for much less work.
3. **Raster → vector at ingest.** Shrinks the problem instead of optimising
   it. Needs the measured decision rule above and a home in the pack builder.
4. **Compositor-side texture compression.** In the compositor, not the client:
   the client would need to know the compositor's GPU capabilities, which
   means negotiation and gives up the property that a client knows nothing
   about the display's hardware. On unified memory the transfer was never the
   expensive part — residency is — so compressing at upload captures nearly
   all of it. Requires runtime feature negotiation; `rill-gpu` currently
   requests `Features::empty()`.
5. **Content addressing and cross-window sharing.** After there is something
   to measure. See the open question below before building it.
6. **Video.** Only after deciding whether it is a promise or a boundary.

## Open questions

* **What compressed formats does VideoCore VII actually expose under V3DV?**
  Five minutes on a Pi, and it decides the format for the only target that
  matters. Everything above is probed on x86.
* **Cross-window dedup is a side channel.** Content-addressed sharing between
  mutually distrusting apps lets one app learn that another has already
  loaded a particular image, by observing that "already have it" came back
  instantly — the classic storage-deduplication leak. Scoping sharing to
  same-origin closes it and costs most of the benefit. Undecided.
* **Is a replay meant to be visually or structurally faithful?** A product
  question, not a technical one, and everything about media in history
  follows from the answer.
* **Video: promise or boundary?** `specs/appliance.md` already says the
  appliance is honest as a secure terminal before it is honest as a media
  machine. If that stands, say so louder. If it does not, the work is
  hardware decode and zero-copy dmabuf, which is driver-adjacent and
  platform-specific — a different kind of engineering from everything else
  here, and where "just enough libs" erodes.
* **Does culling change what a recording is?** The frame is also the
  recording format, so a replay now shows what was scrolled into view rather
  than the whole document. That may be *more* faithful — it is what the person
  saw — but it is a decision, and it lands on the fidelity question above.
* **~~Scroll thrash is unmeasured.~~** Measured and handled: a flick through a
  forty-picture roll cost one refetch per picture per pass, now five, by
  refining only when the view has stopped travelling. What remains unmeasured
  is the *decode* cost of that on the Pi rather than the count of it — five
  JPEG decodes arriving together at the end of a flick is a different
  proposition on a Cortex-A76 than here. They no longer land on the frame
  path, so the question is how long the picture stays coarse rather than
  whether the window stutters.
* **One worker, and no measurement of when that is not enough.** A page of
  sixty photographs is sixty decodes through a single thread, ordered so that
  what the window shows goes first. On four cores that is a deliberate choice
  to be slow rather than to double the peak — but the crossover, where a
  second worker would be worth the memory, has not been looked for, and a Pi
  is where it would be.
* **The floor is one number for every picture.** 64 px on the long edge is a
  guess that happens to cost about 30 KiB for a photograph, and nothing decides
  it per device or per kind of image — an icon and a hero are floored by the
  same rule. It belongs with the ceiling in item 1, as the third of the three
  device numbers, rather than staying a constant in the viewport.
* **~~An `image` node ignores style width/height.~~** Fixed 2026-08-21: a
  style's `width`/`height` size the box, an undeclared axis follows the
  picture's aspect, and the picture sits contained and centred inside — a 4:3
  photograph in a square slot letterboxes rather than squashes, with the
  style's background as the mat. A declared box also does not reflow when the
  picture arrives, so a gallery's scroll position survives its images
  loading. Layout-only: the wire format is untouched, styles already carried
  the fields. Measured, sixty 1600x1200 photographs at 1280x800: as a roll
  37.4 MiB held / 0.1 KiB frame; as a 240x180 grid **6.2 MiB held, 6.0 MiB
  sent, 1.2 KiB frame**. `/public/grid` in the demo shows it.
