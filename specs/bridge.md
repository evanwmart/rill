# Rill Bridge — Server-Side Connectors (pre-design idea log)

Status: **not yet designed** — captures a direction reasoned through in Aug
2026, per the plan's "record ideas, don't design early" rule. Nothing here
is committed. Related: [compute-apps.md](compute-apps.md) (the tier ladder
this reuses), [application-model.md](application-model.md).

---

## The idea

The server consumes the ugly external world; displays only ever see clean
Rill documents. A bridge layer of *connectors* fetches from external
sources (HTTP/JSON, MQTT, ICS, RSS…), extracts named fields, normalizes
them into typed snapshots, and binds them into document templates:

```text
Internet / local services
        │
        ▼
   Rill Bridge          fetch → authenticate → validate → extract → normalize
        │
   typed snapshot       (content-addressed)
        │
   Rill document        (template + tokens + preset shader params)
        │
   ┌────┼────────┐
   ▼    ▼        ▼
 wall  e-ink   kiosk     displays stay dumb: identity, cache, decoder,
 panel panel    Pi       renderer, (optional) input — nothing else
```

This is what turns weather/calendar/transit/Home-Assistant/Grafana screens
from bespoke apps into configuration-driven deployments — the missing piece
between the platform and the appliance/kiosk/mirror scenarios.

## Trust boundary

The bridge is the **only** component that talks to the untrusted internet.
Displays receive inert documents over mutual TLS; all hostile parsing
happens in one auditable place. A malicious or compromised API can, at
worst, put strange text into a template slot — data in a field, never
anything that executes.

Outbound access is **deny-by-default**: each source declares its origins,
enforced by an allowlist that mirrors `policy.toml` in the other direction.
Installing a shared pack shows its origins ("this will fetch from
api.weather.gov") as part of consent.

## The governing principle

> **Data flows freely. Code is conspicuous.**

Same rule as the shader-sharing decision (presets = platform code; shared
themes = pure data; personal shaders = local authorship) and the app tier
ladder. Applied here:

* **Extraction is selectors + a fixed formatter set** (JSONPath-style paths;
  date/unit/rounding formatters). No embedded templating or scripting
  language — Tier-0 packs must not quietly evolve into another JavaScript.
* A source too weird for selectors crosses an **explicit boundary** into a
  Tier-1 WASM connector (compute-apps ladder): code, declared as code,
  capability-enveloped, conspicuous at install.
* Dashboard packs (source config + selectors + template + theme refs) are
  therefore pure data and shareable like any theme.

## The boring parts that make it strong

Source definition needs these as first-class fields, not afterthoughts:

```text
Source
├── endpoint / allowed origins
├── credential REFERENCE (never the secret itself)
├── polling policy (interval, jitter)
├── timeout
├── max_response_bytes
├── content type
├── selectors / schema
├── formatters
├── cache policy
└── failure behavior
```

**Secrets.** A `.rillpack` must never contain credentials. Packs name a
credential (`credential = "weather-api"`); the server's own config maps
that name to the secret. Packs stay distributable; secrets stay local.

**Resource limits.** A hostile endpoint must not matter: cap response
bytes, nesting depth, field lengths; enforce timeouts. Reject, don't
truncate silently.

**Fan-out economics.** 5,000 screens showing weather means ONE connector
fetch → one snapshot hash → 5,000 displays consume the cached snapshot.
Fetch count scales with sources, never with screens.

**Failure.** Retain last-known-good snapshot, back off exponentially,
surface staleness (documents can render a "stale since …" state). A dead
API produces a yesterday's-data screen, never a blank one.

## Typed snapshots — the layer that pays twice

Selector output lands in a typed intermediate value, not directly in a
template:

```text
HTTP/JSON → selectors → Snapshot { temperature: Temperature(21.4, C),
                                   condition: "Cloudy",
                                   updated_at: Timestamp(…) }
                      → document template(s)
```

Why the extra layer: the same snapshot feeds a wall display, an e-ink
panel, a notification rule, a history/graph store — and the **agent
surface**. An assistant answering "what's the temperature in the lab?"
queries the semantic state (`rill://lab/environment.temperature`-shaped
addressing, exact scheme TBD) instead of visually scraping a rendered
dashboard. The UI becomes one presentation of semantic state, which is the
north star's "agent interface == accessibility tree" identity extended to
data. Snapshots are content-addressed like everything else.

## Displays as render endpoints

The bridge + dumb clients implies a provisioning model worth recording —
per-device profile, roughly:

```text
device:        lobby-east-02
capabilities:  1920×1080, touch=false, hdr=false, audio=false
presentation:  lobby-status        # which document/dashboard
theme:         corporate-dark
allowed server: display.company.internal
```

The server decides *what* information and *when it changes*; the device
decides *how to render it efficiently* for its panel (LCD at 60fps, e-ink
partial refresh — same document). This split is the fleet story.

## Scope decisions (provisional)

1. **v1 is pull-only.** Field-grabbing covers dashboards, signage, mirrors,
   e-ink. Write-back connectors (actions that POST to external APIs) are a
   materially bigger trust question — deferred deliberately.
2. Extraction language: selectors + fixed formatters only (above).
3. Secrets by reference only (above).

## Sequencing

Belongs after the compositor work; natural first consumers are the
dashboard/mirror scenario and the RSS-reader app (a feed reader is just a
bridge source + reading template). Prerequisite for the kiosk/fleet story
being config-driven rather than bespoke.
