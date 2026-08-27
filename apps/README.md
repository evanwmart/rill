# Apps — things that run ON Rill

Each app is a server speaking the doc protocol; the platform binaries
that host and render them live in `platform/`. Rough maturity split:

| app | what it is | grade |
|---|---|---|
| files-app | file manager + the demo desktop's app server | product |
| term-app | terminal (pty → grid → live document) | product |
| studio-app | Theme Studio — ricing, widgets, showroom | product |
| music-app | local music player (server-side playback) | product |
| meter-app | system-metrics widget | product |
| notes-app | minimal CRUD notes — **the tutorial app**: read this first | example |
| ascii-app | animated ASCII-art widget | example |
| showcase/ | KDL sources for the showcase pack (aurora, brandbook, console, glass) | demo content |

Future product apps (settings) are born in `platform/`'s sibling — here —
unless they are platform infrastructure.

## Why some apps are `lib.rs` and some are `main.rs`

Both are servers; the difference is who runs the process.

`notes-app` and `files-app` have a `main.rs` because each is a standalone
server you can start on its own. Everything else is a library, because
`files-app` is also *the demo desktop's app server*: it links the other five
in-process and serves them all from one port, which is why it depends on
them. A library app exports the same handler a binary would register, so
promoting one to standalone is adding a `main.rs`, not a rewrite.
