# Rill Application Model — Working Doc

Status: **draft / working doc**. Covers Application Phase 4 (milestone 10):
manifests, install, identity, the launcher, and updates. Declarative
actions (Phase 5) and permissions enforcement (Phase 6) will extend this
document.

---

## 1. What an application is

```text
manifest   tiny, mutable pointer: metadata + current pack path + pack hash
pack       immutable .rillpack: the app's entire content, atomically versioned
```

* **The pack hash is the version.** There is no separate version string to
  drift from reality; you either hold `blake3:ab12…` of the app or you don't.
* Updates swap whole packs — no per-resource skew, ever. The update *check*
  is one conditional GET of the manifest (NOT_MODIFIED when unchanged).
* Install is by manifest URL (`rill app install rill://host/apps/notes/manifest`);
  no well-known path is required, so one server hosts many apps under
  prefixes. Manifest and pack are ordinary resources: TLS, authorization,
  compression, and caching all apply unchanged.

## 2. Manifest format

TOML, strict (unknown keys reject; `manifest_version` newer than the client
understands → "requires a newer viewer"):

```toml
manifest_version = 1
app_id = "notes"                 # [a-z0-9-]{1,32}
name = "Notes"
entry = "/app/index"             # path inside the pack
pack = "/apps/notes/app.rillpack" # server path of the current pack
pack_hash = "blake3:ab12…"       # integrity + version

[window]                         # optional
width = 960
height = 700
titlebar = "#26263a"             # window-chrome theming (idea from rill-view)

[permissions]                    # optional; parsed + displayed now,
clipboard_write = true           # enforced in Phase 6
```

Permission names are a closed set: `files` (gates the broker file pick —
enforced today) and `clipboard_write` (declared, enforcement is Phase 6).
An unknown name is a parse error, not an ignored key — a silently-kept
grant that nothing enforces would read as security the app does not have.
Growing the set is a spec change: add the name here and to the allowlist
in `rill-app`'s manifest parser together.

## 3. Application identity

```text
identity = (authenticated server certificate fingerprint, app_id)
```

The app ID alone is never trusted (plan § Application Phase 4). Two servers
publishing `notes` are two distinct applications with isolated install dirs
and (Phase 5) isolated state. The install key is derived from both.

## 4. Install store

`~/.local/share/rill` (`RILL_DATA` env override) — data, not cache: cache
clearing never uninstalls. Human-readable like all Rill config:

```text
~/.local/share/rill/
├── installed.toml               # index: key → display fields + hashes
└── apps/
    └── notes-a1b2c3d4/          # <app_id>-<hash(identity)[..8]>
        ├── manifest.toml        # pinned manifest (current)
        ├── manifest.staged.toml # downloaded update, applies next launch
        ├── packs/
        │   ├── <current-hash>.rillpack
        │   └── <previous-hash>.rillpack   # kept for rollback
        └── state/               # reserved for Phase 5 app state
```

Uninstall = remove the directory (and index entry). Install verifies:
manifest parses strictly, pack bytes hash to `pack_hash`, and the pack
passes full `.rillpack` verification, before anything is recorded.

## 5. The launcher

`rill-view` with no arguments renders a **locally generated Rill document**
built from `installed.toml` — same format, same pipeline as every remote
page. Entries link to `/~launch/<key>`, an internal navigation form the
viewer resolves to the app's cached pack. CLI plumbing exists alongside:

```bash
rill app install rill://host/apps/notes/manifest
rill app list
rill app update [key]        # force a check/stage now
rill app remove <key>
```

## 6. Launch and updates

```text
launch:
  promote staged update if one is fully downloaded   (atomic swap)
  open current pack from disk                        (instant, offline-safe)
  spawn background check:
      conditional GET manifest → changed?
      → fetch new pack, verify hash, stage for NEXT launch
```

A running app never changes underfoot — the atomicity packs were chosen
for. Offline launches simply skip the check.

### Resource resolution (pack-then-server)

A running app resolves each requested path **pack first, then its origin
server**: bundled assets (entry, styles, images) load locally and offline;
dynamic endpoints and ACTION submissions fall through to the app's server
over TLS. This is how `apps/notes-app` serves a static entry page from
the pack while its live note list and editors come from the dynamic
`/notes` prefix (Phase 5). A path present in neither is NOT_FOUND.

## 7. Window integration

`[window]` drives rill-view: `width`/`height` apply when the app is opened
directly (`rill-view --app <key>`); `titlebar` colors the client-side
chrome while the app is active (the theming idea recorded during Phase 3).
Translucency and chromeless mode remain future `[window]` fields.

## 8. Decisions (resolved 2026-08)

1. **App payload: manifest → pack.** Atomic versions; version ≡ pack hash;
   install by manifest URL; loose-resource apps rejected for deploy skew.
2. **Install store: readable per-app dirs + installed.toml index** under
   `~/.local/share/rill`; identity key = server fingerprint + app_id;
   previous pack retained for rollback.
3. **Launcher: a locally generated Rill document** in rill-view (no args),
   with `rill app` CLI plumbing alongside.
4. **Updates: check on launch, stage, apply next launch**; `rill app
   update` forces a check. No in-session hot swaps.

## 9. Deferred / ideas

### Text editing (rill-ui text input)

Current inputs support: focus, type/backspace, single- and multi-line, caret
at the insertion point (end of text), Enter = submit (single-line) or newline
(multi-line). **Not yet:** click-to-position the caret, text selection /
highlighting, shift-navigation, and copy/cut/paste. These need the text
shaper to expose per-glyph x-positions (map a click to a character index) plus
a selection range on the input and clipboard capability wiring. A "proper text
widget" work item; orthogonal to the app/capability model.


* In-session "update available — restart" affordance (with future chrome
  work). Minimize/maximize buttons, double-click-maximize.
* Per-app state isolation semantics (Phase 5, `state/`).
* Permissions enforcement + trusted prompts (Phase 6). The capability
  broker built here is the prerequisite for local-compute app tiers —
  see [compute-apps.md](compute-apps.md) (idea log: sandboxed WASM apps
  that render through rill-ui, with statically-confined capabilities).
* Client-side decorations remain the committed posture (compositors that
  claim SSD don't reliably draw it; rill-shell will own chrome eventually).
