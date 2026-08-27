# What this architecture can do that others cannot

Status: **notes, nothing built.** Parked while the file explorer is made
convincing — none of this fixes an app that feels janky, and chasing novelty
on an unconvincing foundation is how a system ends up impressive and unusable.

## The actual unfair advantage

It is not the compute shaders. It is that **the compositor receives meaning,
not pixels.** Every window arrives as a `DrawCommand` stream: the compositor
knows where the text is, what it says, which regions are links, which are
inputs, what a frame's structure is. Nothing built on pixel buffers can
recover that — it can only guess or OCR.

Two consequences worth naming: the accessibility tree and the agent surface
are not features to bolt on, they are the wire format; and anything that
wants to reason about "what is on screen" already has it.

## Candidates

**Desktop-wide live text search.** The compositor holds every window's
command list, so a Ctrl+F could match and highlight inside *every open
window* at once. A pixel compositor would need OCR per frame. Mostly
bookkeeping for us — the highest striking-per-unit-of-work item on this list.

**Crisp semantic zoom.** `scale_commands` re-rasterises rather than scaling,
so zooming the desktop out keeps text as text. An overview you can still
read, unlike every Exposé. It can also be *semantic* LOD — drop body text,
keep titles — because font size and hit regions are in the stream.

**Content-aware window management.** The compositor can see that a window is
mostly empty, or that its content sits top-left, and tile by content rather
than by rectangle.

**Structural damage.** Diff command lists instead of pixels: cheaper damage
tracking, and "what changed" falls out for free for recording and for agents.

## On compute shaders

They are underused as general parallel compute rather than as visuals, and
they need not be elaborate — the boid flock runs 512 agents with live window
rects as obstacles on a trivial kernel, on this box, alongside everything
else.

But most of the above is small-N work where a CPU pass is fine. Compute earns
its place when N is genuinely large: search across many windows × many glyph
runs, occlusion over many rects, thumbnailing. The boids proved the pipeline
exists and is cheap; they did not prove everything belongs in it.

## Sequencing

Make the file explorer feel like something a person would choose to use.
Confidence there is what makes the styling and the specialities worth
touching — and it is also the honest test of whether the document model can
carry a real application at all.
