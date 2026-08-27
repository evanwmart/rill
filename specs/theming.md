# Rill Theming & Rendering — Idea Log

Status: **prototype slice implemented** (Aug 2026). The token model (§1), the
`theme.toml` dotfile (§2), and the cooperative/enforced precedence (§3) now
exist end-to-end in `rill-doc` / `rill-ui` / `rill-viewport` / `rill-shell`.
The rendering-backend directions (§4) remain forward-looking.

**Built so far:**

* Colors in a style are a `ColorRef` — a literal *or* a semantic token
  (`color=accent`, `background=surface`); bare identifiers compile to tokens,
  `#hex` stays literal.
* `rill-ui::Defaults` grew into the token table (`color_tokens`,
  `font_tokens`, `enforce`); `resolve()` does the one render-time lookup — no
  cascade. `font=ui`/`font=mono` resolve as font tokens too.
* `rill-viewport::theme` reads `~/.config/rill/theme.toml` → `[colors]`,
  `[fonts]`, `[desktop]` (wallpaper, focus glow, bundled `fonts_dir`).
* The shell applies one theme to every surface (dock + windows), paints an
  image wallpaper and an accent focus glow (via the new `DrawCommand::Shadow`
  primitive), registers bundled `.ttf`s, and toggles the enforced override
  live from the dock. `rill-view` shares the same theming path.

**Still open:** the manifest `[window] theme = follow|own|fixed` per-app
preference (§3) — today precedence is driven by tokens-vs-literals plus the
global toggle; light/dark variants in one file; and everything in §4.

Related: [compute-apps.md](compute-apps.md), [application-model.md](application-model.md).

---

## 1. Theming — tokens, not cascade

Rill's styling is deliberately anti-cascade (flattened, local, no
inheritance). Theming is cross-cutting ("all apps use the system accent").
The reconciliation: resolve theming by **named-token lookup at render time**,
not by cascade.

* Apps reference **semantic tokens** (`color=accent`, `background=surface`,
  `color=text-muted`, `font=ui`) instead of hardcoded values.
* A token is not inherited down a tree — it is a **lookup against one active
  theme table**, resolved by the client (viewer/shell). No cascade machinery;
  "flattened, local" is preserved.
* Swap the theme table → every token-based node re-renders in the new theme
  instantly. Apps that hardcode colors keep their own look (their choice).
* `rill-ui`'s `Defaults` (already holds page-bg/text/link) grows into the
  full token table; `resolve()` maps token refs to values.

## 2. The theme is a dotfile

`~/.config/rill/theme.toml` — declarative TOML like every Rill config: light/
dark variants, accent, surface ramp, fonts, corner radius, wallpaper. The
shell/viewer reads it; editing re-themes everything token-based. Version it
in dotfiles; share it like a Hyprland config or a base16 scheme. Rill's
ethos (declarative, reproducible, config-as-text, no code) *is* the dotfiles
ethos.

Divergence from the Lua-config vibe: themes are **data, not scripts**
(token→value tables, closer to base16 / Nix / GTK CSS variables). Dynamic
themes (wallpaper-derived accent, time-of-day) are handled by a program that
**generates** `theme.toml` from live state — exactly like the shell dock is a
generated document — never a scripting language embedded in the theme format.
Declarative by default, generation for dynamism.

## 3. App theme preference & precedence

* **System → app** is the default: the user's theme flows into every app's
  token resolution; a token-based app is automatically theme-correct.
* **App declares a preference** in the manifest `[window]`: `theme = "follow"`
  (default), `"own"` (use its declared colors/chrome), or a fixed light/dark.
* **Precedence (deny-by-default spirit):** apps follow the system unless they
  opt out; the shell config can force `override = true` to impose a uniform
  desktop; **the user's override always wins.** Because apps are inert
  token-referencing documents (not code), an app *cannot* defeat the user's
  theme the way web CSS/JS can — theming-you-control falls out of "no code,"
  same as the security story.

## 4. Rendering backend & user shaders

gpui is a framework with a **closed render pipeline** (its own baked shaders;
no public hook for custom WGSL or a post-process pass). So "user shader on a
view" is not cleanly possible through gpui. Two flavors of the dream, both at
layers *other than* the declarative document pipeline:

* **Effect/post-process shaders** (shader wallpapers, blur-behind, window
  transitions, color grading) — a shell/renderer concern; needs our own
  render target + passes.
* **App-renders-with-a-shader** (shadertoy, game, GPU visualizer) — the
  Tier-2 "surface app" capability from compute-apps.md: a sandboxed app gets a
  raw GPU surface + runs its WGSL; the compositor composites it.

**The unlock is a Rill-owned `wgpu` backend** — which `rill-compositor`
(Group Four) needs anyway. DrawCommands were kept backend-agnostic exactly so
gpui can be swapped for it later. In that world the frame composes cleanly:
DrawCommands (documents) are one layer, shader passes another, app GPU
surfaces composite in, and Rill owns every pixel. **The shader dream and the
compositor are the same milestone.**

Safety: shaders are *code* (WGSL) → same discipline as compute apps —
`naga`/`wgpu` validate and compile (no arbitrary memory), GPU timeouts bound
runaway shaders, and the capability is gated. A shader wallpaper is a
shell/theme feature; a shader app is a Tier-2 capability; neither pollutes the
document model. Worst case of a malicious shader stays "burn its own GPU
budget, get killed" — confinement holds.

Caveat: a wgpu backend is substantial renderer engineering (glyph atlases,
batching, surfaces, compositing). gpui-first was correct — you don't build a
compositor to render a notes app. It's the endgame, not the next step.
