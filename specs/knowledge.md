# Rill Knowledge — Packs and Retrieval Working Doc

Status: **draft / working doc** (Sep 2026). A side proof of concept: a
files-only retrieval substrate whose durable form is a Rill resource tree,
distributed as a `.rillpack`, queried by a native engine beside the server,
shown on a Tier-0 document. Nothing in the wire format changes.

Related: [protocol.md](protocol.md) (untouched), [resource-format.md](resource-format.md)
§9 (the pack), [connection.md](connection.md) §8 (root jail),
[compute-apps.md](compute-apps.md) (the later `knowledge.query` capability).

---

## 1. Decisions (resolved 2026-09-16 … 21)

1. **Placement.** The engine (`rill-knowledge`) runs inside a knowledge
   *app* that links `rill-server` like every other app. Displays are Tier 0:
   they GET a page, submit an ACTION, render a document. No search frames,
   no device-side model, no offline query in v0.
2. **Layering.** `rill-protocol` knows bytes and paths; `rill-store` and
   `rill-pack` know resources; `rill-knowledge` knows lexical and vector
   semantics; `rill-ui` knows presentation. A knowledge pack is a
   collection of ordinary resources.
3. **Distribution.** A knowledge pack is a `.rillpack`. Install verifies the
   whole pack, then **extracts** it into the served root under its content
   hash, with a `current` symlink (allowed by the root jail: it resolves
   inside the root). Old packs stay for rollback. Every resource stays under
   the 32 MiB decoded cap (`rill-pack::MAX_DECODED_SIZE`), which sizes the
   shards (§4). The engine reads plain files with seek and offset math.
4. **No directory enumeration.** The server answers NOT_FOUND for a
   directory, so nothing in a pack may need `read_dir` to be interpreted.
   Every semantic node is an explicit `node` resource listing its children.
   The same tree is a local directory, an installed pack, and a remotely
   GET-able resource tree with one representation.
5. **Corpus (v0).** Simple English Wikipedia, the top third of articles by
   CirrusSearch `popularity_score` (~92.8k articles, ~35M words). Bodies
   are walked from the Kiwix ZIM's Parsoid HTML (sections, paragraphs,
   lists survive there — the search dump's `text` is flat); metadata
   (popularity, Wikidata item, opening text) comes from the CirrusSearch
   dump, joined by title. No accounts, no wikitext parser.
6. **Embedder.** `bge-small-en-v1.5` (384-d BERT, CLS pooling, L2-normalised)
   on **burn 0.20**, pinned so its CubeCL backend shares Rill's wgpu 26.
   The backend is a type parameter: wgpu for the build, ndarray CPU on a
   serving box without a GPU. Probe 2026-09-21: worst cosine 0.999999 vs
   the Python model; 13.6 ms/query on the RTX 5070, 33.7 ms on CPU
   (measured). The model contract lives in the manifest; a pack refuses a
   mismatched query embedder and degrades to lexical.
7. **Fusion.** Reciprocal rank fusion over the semantic, lexical and entity
   runs; boosts and penalties as a final pass. Postings stay id lists (no
   term frequencies). Weighted-sum fusion is revisited only if the gold set
   shows it winning, which would mean BM25 fields in the postings.
8. **Transport.** The search field submits an ACTION (the only way a state
   slot reaches the server). The results page also exists at a GET path,
   `/knowledge/q/<query>`, so a result is a resource: cacheable, GET_IF-
   conditional via the app revision stamp (pack hash + query), sealable,
   linkable. The ACTION response is that same page.

## 2. Pipeline

```text
BUILD (rill-knowledge-build; heavy allowed)        SERVE (knowledge-app)
zim + cirrus ─ join ─ walk ─ chunk ─ dedupe        GET /knowledge      page
  ─ text shards ─ embed ─ quantise ─ vectors       ACTION query        results
  ─ coarse ─ postings ─ k-means tree ─ manifest    GET /knowledge/q/…  results
  ─ PackBuilder ─ verify ─ extract into root       GET /knowledge/c/…  chunk
```

Build order for the code: format core → ingest → brute-force + gold set →
postings + RRF → tree → rerank → pack/install → app → profile. Brute force
is the recall ceiling every later stage is measured against.

## 3. Conventions

* Every derived file is UTF-8 text; `grep -R` over `text/` is a supported
  interface. Binary never appears; vectors are Base64URL.
* **Base64URL**: `A–Z a–z 0–9 - _`, no padding, fixed width for fixed input
  (n bytes → `ceil(4n/3)` chars). Safe in paths (protocol §7.1) and on
  every filesystem.
* Integers in text files are lowercase hex, fixed width where a file is
  seekable by record (§4.2, §5).
* Hashes are `rill_store::Hash`, written `blake3:<64 hex>`.
* Paths obey protocol §7.1 (UTF-8, no `..`, no empty segment, ≤ 1024
  bytes); shard names are zero-padded so they sort.
* The manifest is written last; a tree without one is incomplete. Build
  into a sibling temp dir, rename on success.
* No timestamps anywhere: the pack must be byte-identical for identical
  inputs (resource-format.md §9 determinism). Provenance is by hash.

## 4. Layout

```text
knowledge/
├── manifest                  key=value, §9
├── doc/NNNN                  document table shards, §4.1
├── text/NNNN                 chunk text shards, §4.2
├── text/NNNN.off             fixed-width byte offsets into text/NNNN
├── vector/full/NNNN          384-d int8 rows, fixed width, §5
├── vector/coarse/NNNN        64-d int8 rows, fixed width, §6
├── semantic/root/node        tree, §7 (children are numbered dirs)
├── semantic/root/3/node
├── semantic/root/3/0/members
├── lexical/aa … lexical/zz   term postings, §8
└── entity/aa …               alias / Q-id postings, §8
```

`NNNN` is the shard number in 4 hex digits. **SHARD = 8192** chunks per
shard, fixed for the pack (manifest `shard`). Shard of chunk `c` is
`c / SHARD`, row is `c % SHARD`. Chunk text is hard-capped at
`MAX_CHUNK_BYTES = 2400` (§4.2) so a text shard is < 20 MiB, and a full
vector shard is 8192 × 513 B ≈ 4.2 MiB — every resource is well under the
32 MiB pack cap.

### 4.1 Documents — `doc/NNNN`

Dense `DocId = u32`, assigned in build order. One line per document,
8192 per shard, tab-separated, fields escaped as §4.2:

```text
<doc_id hex8>\t<page_id dec>\t<qid or ->\t<first_chunk hex16>\t<chunk_count hex4>\t<popularity>\t<title>
```

`popularity` is the source score as shortest-roundtrip decimal. The line is
not seekable by offset (titles vary); readers scan a shard, which is
bounded and rare (a result needs its document's title once).

### 4.2 Chunk text — `text/NNNN` + `text/NNNN.off`

Dense `ChunkId = u64`, assigned in build order; a document's chunks are
contiguous. One record per line:

```text
<chunk_id hex16>\t<doc_id hex8>\t<section>\t<text>
```

Escaping, applied to `section` and `text`: `\` → `\\`, TAB → `\t`,
LF → `\n`, CR → `\r`. Nothing else. `section` is the heading path
(`Early life` or `Career > 1990s`), empty for the lead. `text` is at most
`MAX_CHUNK_BYTES` bytes *before* escaping.

`text/NNNN.off` holds one fixed 17-byte record per chunk: 16 hex digits of
the byte offset of that chunk's line in `text/NNNN`, then LF. Row `r` is at
byte `17 r`. Lookup of a chunk is two seeks and one line read.

### 4.3 What a chunk is

Built by the chunker (`rill-knowledge-build::chunk`), stated here because
it is a format invariant, not a build detail:

* Boundaries: section, then paragraph, then sentence. Paragraphs are
  packed into a chunk up to `CHUNK_TARGET_WORDS = 300`; a paragraph over
  `CHUNK_MAX_WORDS = 400` is split at sentence ends.
* A list becomes one paragraph per `LIST_ITEMS_PER_CHUNK = 12` items,
  items joined by `; `.
* Fragments under `MIN_CHUNK_WORDS = 8` are merged into the previous
  chunk of their section when that stays under the caps; a fragment with
  no such neighbour stands (a stub article is still an article).
* Every chunk's text is prefixed for embedding with its document title and
  section (`<title> — <section>\n<text>`) but stored without the prefix;
  the prefix is reconstructible from `doc/` and the section field. The
  manifest records this (`embedding.doc_prefix`).
* Exact duplicates (BLAKE3 of the normalised text) are stored once; the
  duplicate's document simply has one chunk fewer.

## 5. Full vectors — `vector/full/NNNN`

The embedder emits `D = 384` f32, L2-normalised. Quantise each component to
int8: `q = round(x · 127)` clamped to `[-127, 127]`. 384 bytes → exactly
512 Base64URL chars (384 is divisible by 3). One row per chunk:

```text
<512 chars>\n          row width 513, row r at byte 513 r
```

Similarity is cosine over the int8 vectors; the scalar scale cancels, so no
per-row scale is stored. Row `r` of shard `s` is chunk `s·SHARD + r`,
always — a deduplicated chunk id is never skipped in the vector shards
(there are no holes: ids are assigned after dedupe).

## 6. Coarse vectors — `vector/coarse/NNNN`

A deterministic CountSketch projection `384 → 64` of the *f32* vector, then
re-normalised and quantised as §5. For component `i`:

```text
h      = splitmix64(seed ^ (i · 0x9E3779B97F4A7C15))
bucket = h mod 64
sign   = +1 if (h >> 32) & 1 == 0 else −1
coarse[bucket] += sign · full[i]
```

`splitmix64` is the standard finaliser (`z += 0x9E3779B97F4A7C15;
z = (z ^ z>>30) · 0xBF58476D1CE4E5B9; z = (z ^ z>>27) · 0x94D049BB133111EB;
z ^ z>>31`). No matrix is stored; `projection.seed` in the manifest is the
whole contract. 64 bytes → 86 chars; row width 87.

## 7. Semantic tree — `semantic/`

Spherical k-means over coarse vectors, branching `B = 16`, leaf target 4096
chunks, max depth 4. Directory names are child indices, not vectors: the
`node` file carries the vectors, so no name is ever decoded.

`node` (UTF-8):

```text
v=1
kind=inner            | kind=leaf
count=<n children>    | count=<n members>
<blank line>
<child> <coarse-b64>  … one per line (inner)
```

An inner node's children are `<child>/node`. A leaf node has
`members`: sorted chunk ids, `hex16\n` each, one per line — 17 bytes per
member, seekable. Traversal is beam search (`semantic.beam = 4`): decode
the children's centroids, score against the query's coarse vector, keep the
best `beam`, recurse; union the leaves' members as candidates.

## 8. Postings — `lexical/`, `entity/`

Terms: Unicode-lowercase, split on non-alphanumerics, no stemming. Lines
sorted bytewise by term, one file per prefix: the first two characters when
both are ASCII alphanumerics, `_` for shorter terms, and `_xx` (first byte
in hex) for anything else — so no key can name a directory or collide with
an alphanumeric file. Lookup is binary search over the file by seeking to
a midpoint and advancing to the next line.

```text
<term>\t<df hex>\t<postings>
```

`postings` = sorted chunk ids, delta-encoded, unsigned LEB128 varints,
Base64URL. `entity/` uses the same layout with the term being a Wikidata
id (`q937`) or a lowercased title/alias; its ids are the document's first
chunk.

## 9. Manifest — `knowledge/manifest`

`key=value`, `#` comments, blank lines ignored, unknown keys ignored
(forward compatible without serde). Version 1 keys:

```text
format=rill-knowledge
version=1
shard=8192
chunks=<n>
documents=<n>

embedding.model=bge-small-en-v1.5
embedding.model_hash=blake3:…          # of model.safetensors
embedding.dim=384
embedding.quant=i8
embedding.normalized=true
embedding.pooling=cls
embedding.query_prefix=Represent this sentence for searching relevant passages:
embedding.doc_prefix=title-section       # §4.3

projection=countsketch-v1
projection.dim=64
projection.seed=<hex16>

semantic.branch=16
semantic.leaf=4096
semantic.beam=4
retrieval.coarse_candidates=1024
retrieval.full_candidates=64
retrieval.results=10

source.zim=blake3:…                     # the archive file
source.cirrus=blake3:…                  # the dump file
source.cut=popularity-top-third
source.wiki=simple.wikipedia.org
```

A pack MUST NOT be queried semantically by an embedder whose model hash
differs; the engine reports lexical-only in that case.

## 10. Query (engine)

```text
classify (ids, quoted phrases, capitalised names)
embed query (prefix + text) → 384-d → coarse 64-d
semantic: beam over tree → leaf members (≈5–20k)
lexical:  terms → postings ∩/∪ (AND first, OR if empty)
entity:   qids / aliases → first chunks
coarse rank all candidates (int8 cosine, 64-d) → top 1024
full rank (int8 cosine, 384-d)               → top 64
RRF over the three runs (k = 60); lexical run weighted 0.5 (0.25 in the
  OR fallback), question/function words dropped from it; boosts: title,
  phrase, entity, section
neighbour expansion (c−1, c+1 within the document)
top N → fetch text → document
```

Every stage records what it did (route, candidate counts, per-run ranks,
final score) for the explain page.

## 11. Invariants

* Chunk and document ids never change within a pack; nothing outside a
  pack may hold one. Results carry `page_id` / `wikibase_item`.
* Vector row `r` of shard `s` is chunk `s·SHARD + r`. No holes.
* One model configuration per pack (manifest), fingerprinted.
* Semantic, lexical and entity indexes are derived: deletable, rebuildable
  from `text/` + `vector/`. Text never depends on an index.
* No query modifies a pack. No network call exists in `rill-knowledge`.
* A corrupt derived index must never corrupt source text: writers never
  open `text/` for writing after the build step that produced it.

## 12. Numbers

Measured 2026-09-21, text stage over the full cut on the build box:
92,570 documents (92,760 in the cut, 190 gone from the ZIM), 258,506
chunks after 2,299 exact duplicates, 24.5M words, words per chunk p10 17 /
p50 71 / p90 221 / max 400 — Simple English sections are short, so chunks
are smaller than the 300-word target. 64 text shards, 164 MiB on disk,
14.6 s wall, 335 MB peak RSS. Embedding this many chunks is projected at
minutes on the GPU (§1.6 measured 13.6 ms/sentence single; batched is
faster).

Vector stage, measured 2026-09-21: 258,506 chunks embedded on the RTX
5070 through burn-wgpu in 933 s (≈ 277 chunks/s at the end, batch 128,
inputs sorted by length; unoptimised), 828 MB peak RSS; `vector/full`
127 MiB, `vector/coarse` 22 MiB. Whole pack 0.35 GB.

Query, measured: engine open 0.66 s (vectors resident, ~100 MB); the
brute-force semantic run over every chunk 28–44 ms; a fused query on the
wire 30–42 ms server-side plus ~14 ms to embed the query on the GPU.

Gold set, 2026-09-21. The automatic title set is saturated (recall@10
1.000 in every mode: a title is in its own chunk). The twelve hand
questions are the signal (rank of the expected article, fused):
EPR paradox #5 (lexical alone #10), Paris #4 (lexical miss),
Photosynthesis #4, Mount Everest #2, Leonardo da Vinci #2, Season #1,
Prime number #1, American Civil War #6, Honey #3; misses: Giraffe
("longest neck" — the model puts dogs first, the article says "long") and
Charon (the corpus answers with Pluto's "Moons" section, ranked #1: a
gold-set flaw more than a retrieval one). Adrenaline is not in the cut.
The lexical weights were chosen by sweeping this set; twelve questions is
a small set and the choice is provisional.

Targets (not measured): the tree replaces the brute-force scan when a
pack outgrows a resident scan; per the FSRAG working spec, warm query
p50 < 25 ms excluding the embed.

## 13. Open

1. `wikitable` tables: skipped in v0 (count reported by the walker).
2. Entity aliases beyond title + Q-id (redirect titles are the obvious
   source; the ZIM has them).
3. Whether the results page, being a GET resource, should be history-
   sealed by the app. Not in v0.
