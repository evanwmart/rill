# Apps

Rill applications. Each is a manifest plus KDL pages — no code — built into
a `.rillpack` that `rill-server` serves and `rill app install` fetches.

```sh
cargo build -p rill
apps/showcase/build.sh out/          # -> out/apps/<id>/{app.rillpack,manifest}
apps/showcase/build.sh --repin       # accept new hashes after a format change
```

`build.sh` compiles every `src/NAME.kdl` to `/app/NAME` inside the pack and
ships the KDL alongside it at `/NAME.kdl`, so a published app always carries
its own source. (`apps/notes-app` is a Rust server app and builds
itself.)

## Reproducibility is checked, not assumed

Packs are deterministic: same sources in, same bytes out, same hash. Where a
manifest pins `pack_hash`, the build **verifies** it and fails on a mismatch,
so drift in the document codec or the pack format surfaces here rather than at
install time. `--repin` accepts the new hashes — correct only when you know the
sources did not move.

## What lives here

* **files** — the file explorer's landing page. The app itself is
  `apps/files-app`, which serves `/files` dynamically.

## What used to live here

`aurora`, `brandbook`, `console` and `glass` were removed. They were built to
demo *palettes and glass* rather than to be used, and holding them up as the
reference made for a closed loop — styling features judged against pages built
to show off styling features. Real applications drive the design now; what
they make awkward is what gets built next.

They are in git history if a palette needs re-checking:
`git log --diff-filter=D --name-only -- apps/showcase`.
