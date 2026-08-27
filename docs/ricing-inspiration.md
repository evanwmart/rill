# Ricing: the landscape, and what to steal from it

Status: **notes**, written 2026-08-13 for the Saturday video. Reference
material, not a plan — nothing here is a commitment to build anything.

Caveat worth stating once: this is the ecosystem as I know it, not as
measured this week. Popularity in ricing moves fast and the top of the
subreddit is a fashion cycle. **Sanity-check any specific claim before
putting it in a public post**, especially "X is the most popular Y".

## Who the audience is

People who assemble a desktop out of eight separate programs and eight
config formats, and who enjoy that assembly as the hobby. Which means:

* They will recognise every component in the shot and ask about the ones
  they don't. The top comment on any post is some form of *"what's your
  bar/terminal/colourscheme?"*
* **A dotfiles link is expected.** A rice with no config to read is a
  screenshot, not a rice, and gets treated as one.
* They reward *coherence* — one palette carried through every surface — far
  more than they reward novelty.
* They are hostile to anything that smells like an ad, and very quick to
  spot a demo pretending to be a daily driver.

That last point is the risk `saturday-rice.md` already names: expect
*"what happens when you open a real app"* as the top comment.

## The stack they actually run

Useful mostly as a map of what Rill is silently replacing — each row is a
separate program with its own config file in a normal rice.

| Layer | What people run |
|---|---|
| Compositor / WM | **Hyprland** (dominant on Wayland — animations, blur, rounded corners), Niri (scrollable tiling, rising fast), Sway, river, dwl; on X11: i3, bspwm, awesome, dwm, xmonad, qtile |
| Bar | **Waybar** (Wayland default), Polybar (X11), **eww** and **AGS/Astal** for the elaborate ones, Ironbar, yambar |
| Launcher | rofi / rofi-wayland, wofi, fuzzel, tofi, anyrun |
| Notifications | dunst, mako, swaync, fnott |
| Terminal | kitty, foot, alacritty, wezterm, **Ghostty** |
| Shell prompt | starship, powerlevel10k, oh-my-posh |
| Fetch | **fastfetch** (neofetch is archived), pfetch, nitch, macchina, onefetch |
| Files | **yazi** (ascendant), ranger, lf, nnn, superfile |
| Music / audio | ncmpcpp + mpd, **cava** (the audio visualiser in half the screenshots), cmus |
| Wallpaper | **swww** (transitions), hyprpaper, swaybg, mpvpaper (video walls) |
| Effects | picom + forks (X11 blur/shadow/animation), hyprshade, Hyprland plugins (hyprbars, hyprexpo, hy3) |
| Theming | **pywal** / **wallust** (palette from wallpaper), **matugen** (Material You), base16 / tinted-theming, **Stylix** (whole-system, NixOS) |

A large slice of the audience is on **NixOS + home-manager**, because a rice
you can reproduce is the end state of the hobby. Their instinct is exactly
Rill's: declare the look once, apply it everywhere.

## The palettes

Practically a fixed set, and using one buys instant familiarity:

**Catppuccin** (especially Mocha) is the default of the era — soft pastels
on deep grey-purple, with an enormous ecosystem of ports. **Gruvbox** is the
warm retro standard. **Nord** is the cold one. Then **Tokyo Night**,
**Rosé Pine**, **Everforest**, **Kanagawa**, **Dracula**, **Solarized**.

Two things follow for us. Shipping a Catppuccin-accurate and a Gruvbox-accurate
rice would be read as fluency — *this person knows the neighbourhood* — and
costs nothing but hex values, since a rice is one file. And the
palette-from-wallpaper idea (pywal/wallust/matugen) is a natural fit for
Rill: the theme is one file of tokens, so generating it from an image is a
small program, not an integration.

## The recurring looks

* **Glassmorphism** — blur, transparency, gaps, rounded corners, a soft
  wallpaper behind everything. The dominant Hyprland look.
* **Cosy / muted** — Everforest or Kanagawa, low contrast, warm, often a
  painterly wallpaper. The anti-neon reaction.
* **Neon cyberpunk** — near-black, one or two saturated accents, glow,
  scanlines, sometimes a CRT shader.
* **Monochrome minimal** — one hue, no icons, heavy whitespace, tiny bar.
* **Terminal maximalism** — the whole screen is TUIs: fetch, cava, a file
  manager, a music player, htop. Everything is text.
* **Retro / CRT** — phosphor green, curvature, bloom, deliberate scanlines.

Our three shipped rices land on three of these — `ember` (neon/warm),
`glacier` (glassmorphism), `phosphor` (retro CRT) — which is deliberate:
cycling between them on camera has to read as *different desktops*, not as
three shades of the same one.

## What Rill is, in their terms

Worth being blunt about, because it is both the pitch and the objection.

**Rill collapses that whole table into one program and one file.** The
compositor, the bar, the widgets, the terminal, the notification surface and
the theme engine are the same binary reading the same `theme.toml`. There is
no glue between Waybar's JSON, picom's conf, kitty's ini and dunst's
dunstrc, because there are no such programs.

To this audience that reads two ways at once:

* **The good half** — no theme drift, nothing to keep in sync, live
  re-skinning without restarting anything, and a rice that is genuinely one
  file you can post. Stylix users will get it immediately.
* **The bad half** — you cannot swap the bar. Assembling from parts *is*
  the hobby, and a monolith takes that away. Expect *"so I can't use
  Waybar?"* and have an answer.

Things we can do that they measurably cannot:

* **Vector windows.** Crisp at any zoom, re-themed live, kilobytes per
  frame. Nobody's rice survives a zoom.
* **Per-window shaders that glass in front actually blurs** (item 2). In
  a normal stack, effects are a post-process over the finished frame —
  which is exactly why picom's blur cannot be occluded properly either.
* **Cycling a whole desktop with a keypress**, because a rice is one file.
* **Semantic recording** — a session replay that is text-searchable rather
  than a video.

Things we plainly lack: a browser, arbitrary apps, a music visualiser, and
any dotfiles ecosystem at all.

## Concretely worth stealing

Cheap, and each one buys recognition:

1. **A Catppuccin Mocha rice and a Gruvbox rice**, accurate to the published
   hex values. Pure token files. Say so in the post.
2. **`fastfetch` in the terminal shot** — already how the current screenshot
   reads, and it is the genre's handshake.
3. **cava-alike as an ASCII widget.** The ASCII widget already cycles frames
   on a clock; a bar-graph audio visualiser is the single most recognisable
   object in the genre. (Needs an audio source — probably not this week.)
4. **Wallpaper-derived palettes**, pywal-style. One small program, given
   that the theme is a token table.
5. **Gaps as a first-class knob.** Every tiling rice is judged on its gaps,
   and ours are currently implicit.
6. **A dotfiles-shaped repo layout** — `assets/rices/` is the seed. Post
   the link and let people read the file.

## For the video specifically

* Lead with what cannot be faked in another stack: the wallpaper reacting
  to a dragged window, and flames from one window blurring through the glass
  of another.
* Show the *same* desktop cycling rices at the end. The genre's whole value
  system is coherence, and cycling proves coherence is structural here
  rather than hand-assembled.
* Keep a terminal with `fastfetch` on screen at some point. It reads as
  "this is a real desktop" faster than any claim.
* Have the "can I use my own apps?" answer ready in the description, not
  only in the comments — the compositor runs ordinary Wayland clients.
* Link the repo and `assets/rices/`. To this audience, unreadable config
  is the tell that something is a mockup.
