# Getting Rill onto a Raspberry Pi

Status: **done — the desktop ran and a bundle exists (2026-08-15).**
Idle measured at 27.8 MiB on the board; see
[memory-footprint.md](memory-footprint.md). What follows is the route that
worked, corrections included.

Previous status: hardware verified — the go/no-go passed. Written
2026-08-14 from reading the tree; the workstation half (§"From this machine")
added 2026-08-15; the facts below now come from the actual board rather than
from guesses.

## MEASURED: the board, 2026-08-15

The Pi on this bench, read over SSH. Everything in this block is observed
output, not expectation.

```text
board        Raspberry Pi 5 Model B Rev 1.1   (revision a04171)
memory       total_mem=1024 — a 1 GB Pi 5      ← see the correction below
cores        4          governor: ondemand
disk         20 G free of 29 G
OS           Debian GNU/Linux 13 (trixie), kernel 6.18.34+rpt-rpi-2712
glibc        2.41
session      graphical.target, labwc, /run/user/1000/wayland-0   ✓ nestable
vulkan       V3D 7.1.10.2 — V3DV Mesa 25.0.7-2+rpt4, Vulkan 1.3.305
thermal      throttled=0x0, 53.2 °C idle
toolchain    git present; NO rustc
```

**The five dmabuf import extensions: all present.** This was "the single
highest-risk unknown in the whole plan", and it is now settled in the
favourable direction — `rill-compositor` should start on this board.

```text
VK_KHR_external_memory              present
VK_KHR_external_memory_fd           present
VK_EXT_external_memory_dma_buf      present
VK_EXT_image_drm_format_modifier    present
VK_EXT_queue_family_foreign         present
```

Three corrections to what this document previously asserted:

* **A 1 GB Pi 5 exists.** This document said "the Pi 5 does not ship in 1 GB"
  and used that to infer a 1 GB board must be a Pi 4. The board in hand is a
  Pi 5 reporting `total_mem=1024`. The inference was wrong, and it mattered:
  it pointed at "build on the Pi" when the RAM says otherwise.
* **The ICD is `broadcom_icd.json`**, not `broadcom_icd.aarch64.json` as the
  pinning advice below said. The wrong path silently does nothing, which is
  the worst way for that advice to be wrong.
* **lavapipe is installed by default** (`lvp_icd.json`), and `vulkaninfo`
  lists it as a second device. The pin is therefore not optional hygiene —
  it is the difference between measuring a GPU and measuring a CPU
  rasterizer. `graphics.software_renderer` in the bundle is the backstop.

And Debian 13, not Bookworm: newer than this document assumed. labwc is the
session, glibc is 2.41.

The goal is narrow and worth stating first, because it is easy to
accidentally aim at something four times larger:

> **Run the existing desktop on a Pi, nested in the Pi's own compositor, and
> get one `bench-device.sh` bundle off it.**

That is the measurement every projection in
[resource-envelope.md](resource-envelope.md) is currently standing on, and
the one thing that would turn "PROJECTED 120–200 MiB idle on Mesa" into a
number. It is *not* the appliance (ladder rung 3), it is not boot-to-desktop,
and it does not need either.

## Wiring it up (the part before any of this works)

**You cannot drive the Pi over a single USB-C cable from the workstation.**
Worth stating because it is the obvious thing to try:

* **Power.** A Pi 4 wants 5V/3A, a Pi 5 wants 5V/5A for full performance
  (5V/3A with reduced peripheral current). A PC USB-A port supplies 0.5A
  (USB 2) or 0.9A (USB 3); a motherboard USB-C port rarely negotiates more
  than a couple of amps. Underpowering does not usually look like a failure
  — it looks like a *slow Pi*, because the firmware throttles. That is the
  single most effective way to poison every number in the bundle. Use the
  official supply (or an equivalent PD brick). `bench-device.sh` now records
  `vcgencmd get_throttled` before and after load, and bit 0 / bit 16 of that
  word mean exactly "under-voltage"; if you see it, stop and fix the supply
  rather than recording the run.
* **Data.** The Pi 5's USB-C port is **power only** — no USB device/gadget
  mode. (The Pi 4's *does* support OTG, so `dtoverlay=dwc2` +
  `g_ether` gives a USB-C network link to a host machine. It is a USB 2.0
  link, it does not solve power, and it is a fiddly extra variable in a
  measurement run. Not worth it here.)

What the Pi actually needs plugged into it:

```text
power     official USB-C supply (15W for a Pi 4, 27W for a Pi 5)
network   Ethernet to the same switch/router as this machine  ← simplest
          (Wi-Fi is fine; configure it at flash time, below)
display   micro-HDMI to a monitor — or enable VNC and go headless (§3)
storage   an SD card that has been *imaged*, not a blank one
cooling   a heatsink or active cooler on a Pi 5 — see below
```

### If it is a Pi 5 specifically

Four things that differ from the Pi 4 and all of them touch the measurement:

* **The supply is 5V/5A (27W), not 5V/3A.** A Pi 5 boots happily on a 3A
  phone charger, caps downstream USB current at 600mA, and warns — and then
  throttles under exactly the sustained load this benchmark applies. If the
  supply is not the official 27W one (or a real PD brick that negotiates 5A),
  that is the first thing to suspect when the numbers look bad.
* **Cooling is not optional for a benchmark.** A bare Pi 5 under all-core
  load reaches its thermal limit in minutes, and the run is `settle + 60s
  idle + 60s busy + a scaling sweep`. A passive heatsink is the minimum; the
  official Active Cooler is better. A throttled run is still a valid
  measurement *if it is labelled*, which is what `graphics.throttled` in the
  bundle is for — but a thermally-limited Pi is not the number anyone wants.
* **Check the RAM, do not assume it from the model.** An earlier draft here
  claimed there is no 1 GB Pi 5 and concluded you could always build on the
  board; the Pi on this bench is a 1 GB Pi 5, which puts it squarely in the
  cross-compile case below.
* **Bookworm or newer only.** A Bullseye image will not boot a Pi 5.

Also, on card size: building this workspace on the Pi wants room for a full
`target/` — plan on a 32 GB card and check `df -h ~` before starting, rather
than discovering it two hours into a release build.

### Imaging the card so first boot is already reachable

Raspberry Pi Imager, on this machine. Choose **Raspberry Pi OS (64-bit)
with desktop** — 64-bit is not optional (§2), and the desktop session is the
thing our compositor nests in. Then open the settings/gear before writing and
preconfigure:

* **hostname** — this is what `raspberrypi.local` resolves to in §1;
* **username and password**;
* **SSH: allow public-key authentication**, pasting `~/.ssh/id_*.pub` from
  this machine, so §1's `ssh-copy-id` is already done;
* **Wi-Fi SSID/password and country**, if it is not going on Ethernet.

Do that and the Pi is reachable from this machine the first time it boots,
with no keyboard ever attached to it. Skip it and you will need a monitor and
keyboard on the Pi to enable SSH by hand.

## From this machine

Everything below happens from the workstation until the Pi is building. This
section is written against what this box actually has: `ssh`, `rsync`,
`docker` (active, and you are in the group), `avahi-daemon` running with
`mdns_minimal` in `/etc/nsswitch.conf`, LAN `192.168.0.0/24` on `enp8s0`, and
a tailnet. No `nmap`, no `arp-scan`, no `avahi-browse` — the commands here
need none of them.

### 1. Find it

In order of how likely each is to just work:

```bash
# mDNS. Pi OS advertises its hostname; nsswitch here resolves .local already.
getent hosts raspberrypi.local || ping -c1 raspberrypi.local

# If it is on the tailnet, this beats everything — it works off-LAN too.
tailscale status | grep -iE 'pi|rasp'

# Otherwise: warm the ARP cache with a parallel ping sweep, then list
# everything that answered. No scanner needed.
for i in $(seq 1 254); do ping -c1 -W1 192.168.0.$i >/dev/null 2>&1 & done; wait
ip neigh show dev enp8s0 | grep -v -e INCOMPLETE -e FAILED

# ...and, as a *hint only*, the known Raspberry Pi OUIs:
ip neigh | grep -Ei 'b8:27:eb|dc:a6:32|e4:5f:01|28:cd:c1|d8:3a:dd|2c:cf:67|88:a2:9e'
```

**Do not filter by OUI and trust the result.** Raspberry Pi Ltd registers new
prefixes, and a list written from memory goes stale silently: this document
originally omitted `88:a2:9e`, which turned out to be the Pi 5 on this very
network — the sweep had found it and the filter threw it away. Read the whole
neighbour list, and treat **the router's client list as authoritative**: it
shows DHCP hostnames (`raspberrypi`) next to MACs, which no amount of OUI
guessing can beat.

If nothing answers at all, the card is probably not imaged — an unbootable Pi
never brings up its network. If the Pi answers ping but has no open ports, see
§4a below.

Two other checks worth knowing, in rough order of how much they tell you:

```bash
# Every host on the segment, including one whose DHCP failed entirely:
ping6 -c3 -I enp8s0 ff02::1 && ip -6 neigh show dev enp8s0

# What is actually listening, which identifies a Pi far better than a MAC:
for ip in <candidates>; do
  timeout 2 bash -c "exec 3<>/dev/tcp/$ip/22 && head -c 80 <&3" 2>/dev/null \
    && echo "  ^ $ip has SSH"
done
```

An OpenSSH banner reading `Debian-...deb12...` is Bookworm, and settles it.

Then make it painless:

```bash
ssh-copy-id pi@raspberrypi.local        # or whatever user the image was flashed with
ssh pi@raspberrypi.local 'echo ok'
```

### 2. Confirm the hardware in one round trip

```bash
ssh pi@raspberrypi.local '
  cat /proc/device-tree/model; echo
  uname -srm
  free -m | awk "/^Mem:/ {print \"RAM MiB total:\", \$2}"
  . /etc/os-release; echo "OS: $PRETTY_NAME"
  ldd --version | head -1
'
```

What the answers have to be:

* **model** — Pi 4 or Pi 5. A Pi 3 or Zero 2 W is VideoCore IV, which V3DV
  does not support, and the trip stops here.
* **`uname -srm`** — must say **`aarch64`**. `armv7l` means a 32-bit image;
  reflash. The workspace is edition 2024 and every number you will compare
  against is 64-bit.
* **RAM** — decides the build strategy below (1 GB changes everything).
* **glibc** — Bookworm is 2.36. This box is 2.43, which is precisely why you
  cannot point a plain `aarch64-unknown-linux-gnu` target at the host
  sysroot and expect the result to run: a binary linked against 2.43 fails
  on the Pi with a `GLIBC_2.4x not found` message. `cross` exists to solve
  this and its images are built against an older glibc.

### 3. Confirm it is not bare metal — i.e. that there is a session to nest in

`rill-compositor` builds smithay with `backend_winit` only (see below), so it
is a Wayland *client*: it needs a running Wayland session on the Pi to nest
inside. A Pi sitting at a console, or running an X11 session, or booted
headless with no display attached, has nothing for it to nest in — and the
failure mode is a confusing one, because the binary starts and then cannot
find a compositor.

```bash
ssh pi@raspberrypi.local '
  systemctl get-default                       # want graphical.target
  loginctl list-sessions
  loginctl show-session $(loginctl list-sessions --no-legend | awk "NR==1{print \$1}") \
      -p Type -p State -p Active -p Remote     # want Type=wayland, Active=yes
  pgrep -a -x "labwc|wayfire|Xorg" || echo "NO COMPOSITOR RUNNING"
  ls -l /run/user/$(id -u)/wayland-* 2>/dev/null || echo "NO WAYLAND SOCKET"
'
```

You want `graphical.target`, one session with `Type=wayland`, `labwc` (Pi 5 /
newer images) or `wayfire` (earlier Bookworm) in the process list, and at
least one `wayland-N` socket. Then, in order of what usually goes wrong:

* **`NO WAYLAND SOCKET` and no compositor, headless.** The commonest case
  when driving a Pi from a workstation: with no HDMI connected, the desktop
  session may never start. Three fixes, best first — enable VNC
  (`sudo raspi-config` → Interface Options → VNC), which on Bookworm starts
  `wayvnc` against a real session and also gives you eyes on the nested
  window; plug in a cheap HDMI dummy load; or force a mode in
  `/boot/firmware/cmdline.txt`. **You need eyes on that session anyway** —
  the desktop we launch appears on the Pi's screen, never on the
  workstation.
* **`Type=x11`.** Pi OS can still run an X11 session. Switch it:
  `sudo raspi-config` → Advanced Options → Wayland → labwc, then reboot.
* **`systemctl get-default` says `multi-user.target`.** Booted to console.
  `sudo raspi-config` → System Options → Boot / Auto Login → Desktop
  Autologin, then reboot.

### 4. Run something in that session, over SSH

This is the part that is easy to get wrong and produces a nonsense error when
you do. An SSH shell has no Wayland environment; you have to point it at the
session that is already running, **as the same user that owns it**:

```bash
ssh -t pi@raspberrypi.local '
  export XDG_RUNTIME_DIR=/run/user/$(id -u)
  export WAYLAND_DISPLAY=$(basename $(ls -1 $XDG_RUNTIME_DIR/wayland-[0-9] | head -1))
  echo "$WAYLAND_DISPLAY in $XDG_RUNTIME_DIR"
  # anything Wayland here now finds the session
'
```

Put those two exports in `~/.bashrc` on the Pi for the duration of the trip;
every later step assumes them. `bench-device.sh` will otherwise flag the run
as nested against nothing and the compositor will exit at startup.

### 4a. It pings, but nothing is listening

A Pi that answers ICMP with **no open port 22, no VNC, and no `.local`
resolution** has booted far enough for DHCP and no further in any way that
helps you. It is not a network problem, and no amount of scanning from here
will change it. Almost always one of:

* **The image was written without customisation.** Bookworm creates no
  default user, so the first boot sits in the setup wizard waiting for a
  keyboard, with SSH off. This is the common case.
* **SSH was never enabled** — Imager's gear menu was skipped, or a Lite image
  was flashed with defaults.

Three ways out, and the first is the one to prefer because it is work you
have to do anyway:

1. **Attach the monitor and keyboard.** You need a display for this project
   regardless (§3 — the compositor nests in the Pi's Wayland session, and a
   headless Pi is extra work). Finish the wizard, then
   `sudo raspi-config` → Interface Options → SSH → enable. Five minutes and
   the Pi is in a state you want anyway.
2. **Fix it from the card.** Put the SD card in this machine and write to the
   FAT32 `bootfs` partition:

   ```bash
   # enable SSH on next boot
   touch /run/media/$USER/bootfs/ssh
   # and create a user, if the wizard never ran (Bookworm has no default one)
   echo "evan:$(openssl passwd -6 'your-password')" \
       > /run/media/$USER/bootfs/userconf.txt
   ```

   Eject, boot, and it is reachable. Note the paths: Bookworm's boot
   partition is `bootfs` and lives at `/boot/firmware` once running.
3. **Reflash with Imager**, this time setting hostname, user, SSH public key
   and Wi-Fi in the gear menu. Slowest, but it leaves the cleanest state and
   is what §"Imaging the card" describes.

### 5. Get the source and the binaries there

The scripts need the **repo**, not just binaries: `bench-device.sh` shells out
to `demo-desktop.sh`, which builds the showcase packs from
`apps/showcase/` and copies rices and shaders out of `assets/`.

```bash
rsync -a --delete \
  --exclude target --exclude .git --exclude bench-results --exclude showcase-out \
  ~/rill/ pi@raspberrypi.local:~/rill/
```

**Where the binaries have to live: `~/rill/target/release/`.** `bench-device.sh`
resolves them as `$repo/target/$profile` and has no flag to look elsewhere —
that is deliberate (a run labelled `release` that measured `debug` is worse
than no run), but it means "rsync to `~/rill-bin` and point the scripts at
them", as an earlier draft of this document suggested, does not work. There
is nothing to point.

Then pick a build route by RAM:

**4 GB+ — build on the Pi.** Slower, no surprises, and the route to prefer.

```bash
ssh pi@raspberrypi.local '
  curl --proto "=https" --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
  source $HOME/.cargo/env
  cd ~/rill && cargo build --release -p rill -p files-app -p rill-compositor -p rill-vector
'
```

**1 GB — cross-compile here.** This box has docker active and you are in the
group, which is all `cross` needs. It is not installed yet, and neither is
the target or an aarch64 linker — `cross` supplies both inside its image, so
install just it:

```bash
cargo install cross --git https://github.com/cross-rs/cross
cross build --release --target aarch64-unknown-linux-gnu \
    -p rill -p files-app -p rill-compositor -p rill-vector

rsync -a target/aarch64-unknown-linux-gnu/release/{rill,files-app,rill-compositor,rill-vector} \
    pi@raspberrypi.local:~/rill/target/release/
```

Then tell the setup script not to rebuild them, or it will start a full cargo
build on the Pi and undo the whole point:

```bash
ssh pi@raspberrypi.local 'echo "export RILL_SKIP_BUILD=1" >> ~/.bashrc'
```

Smoke-test the binaries before anything else — a wrong-architecture or
wrong-glibc binary says so immediately, and `bench-device.sh` records
`file -b` output for each one in `environment.txt` so a bad build is visible
in the bundle afterwards too:

```bash
ssh pi@raspberrypi.local '~/rill/target/release/rill 2>&1 | head -2'
```

## Read this before you buy time on it

Two facts from the source decide most of the plan, and both are cheap to
check before committing an evening.

### 1. The compositor cannot run bare metal yet

`platform/rill-compositor/Cargo.toml` builds smithay with **`backend_winit`
only** — no DRM, no udev, no libinput. Its own comment says the bare-metal
backends "come with milestone 15". So on the Pi, `rill-compositor` must run
**nested inside another Wayland compositor**, exactly as it does on the
development machine.

That is fine for measurement — the nesting is recorded as a flag by
`bench-device.sh`, and the numbers are still real — but it means:

* you need a Pi OS desktop session (labwc or wayfire) running underneath;
* the host compositor's vsync paces our present, so frame rate is partly
  its number, not ours;
* "boot to Rill in N seconds" is **not** measurable on this path. That needs
  milestone 15, and pretending otherwise would put an unearned figure in the
  log.

### 2. The compositor refuses to start without dmabuf import

`main.rs` does:

```rust
let gpu = DmabufDevice::new_on(&instance, Some(&wgpu_surface))
    .ok_or("no dmabuf-capable Vulkan device")?;
```

and `DmabufDevice::try_build` requires **all five** of these device
extensions to be present:

```text
VK_KHR_external_memory
VK_KHR_external_memory_fd
VK_EXT_external_memory_dma_buf
VK_EXT_image_drm_format_modifier
VK_EXT_queue_family_foreign
```

If Mesa's **V3DV** (the Pi's Vulkan driver) does not offer all five, the
compositor exits immediately with that message and nothing else works. This
is the single highest-risk unknown in the whole plan, and it costs one
command to settle.

**Do this first, on the Pi, before building anything:**

```bash
sudo apt install -y mesa-vulkan-drivers vulkan-tools
vulkaninfo 2>/dev/null | grep -Ei 'deviceName|apiVersion'
for e in VK_KHR_external_memory VK_KHR_external_memory_fd \
         VK_EXT_external_memory_dma_buf VK_EXT_image_drm_format_modifier \
         VK_EXT_queue_family_foreign; do
    printf '%-40s %s\n' "$e" \
        "$(vulkaninfo 2>/dev/null | grep -qF "$e" && echo present || echo MISSING)"
done
```

Outcomes:

* **All five present** → proceed, the plan works as written.
* **Any missing** → stop and decide. The fallback exists in the code:
  `upload_shm` already uploads client buffers the slow way, so a
  shm-only compositor is a small change (make `DmabufDevice` optional and
  skip the import path) rather than a rewrite. But it is a code change, it
  changes what is being measured, and it should be a deliberate decision
  recorded in the run's notes — not something discovered at 1 a.m.

Also confirm you are on hardware that has Vulkan at all: **Pi 4 or Pi 5
only.** Pi 3 and Zero 2 W are VideoCore IV, which V3DV does not support.

```bash
cat /proc/device-tree/model   # e.g. "Raspberry Pi 4 Model B Rev 1.5"
free -m                       # how much RAM you actually have
```

1 GB changes the build strategy completely — and note that a 1 GB board is
**not** necessarily a Pi 4: the bench board here is a 1 GB Pi 5 (see the
MEASURED block above). Read the RAM, not the model.

## Which Pi, and what to install

Reference target for this document:

```text
Raspberry Pi 4 or 5, 64-bit
Raspberry Pi OS Bookworm (or newer), 64-bit, with the desktop session
Mesa V3DV for Vulkan
a real SSD/NVMe over USB if you have one; an SD card if you do not
```

64-bit is not optional: the workspace is `edition = "2024"`, and you want
the same pointer width as the numbers you will compare against.

```bash
sudo apt update
sudo apt install -y \
    build-essential pkg-config git \
    libwayland-dev libxkbcommon-dev libxkbcommon-x11-dev \
    libegl1-mesa-dev libgles2-mesa-dev libgbm-dev libdrm-dev \
    libinput-dev libudev-dev libseat-dev \
    mesa-vulkan-drivers vulkan-tools libvulkan-dev \
    fontconfig libasound2-dev
```

(`libasound2-dev` verified missing the hard way, 2026-08-19: a clean
`debian:stable` container following this exact list fails on `alsa-sys` —
the core app set pulls ALSA via the music app — the chain is
files-app → music-app → rodio → cpal → alsa-sys, whose build.rs needs
the dev package's alsa.pc at compile time even on machines that play
audio fine. With it added, the whole workspace builds and its test
suite passes in the container, including rill-gpu's pixel-asserting
tests on Mesa's software Vulkan — the suite is CI-able on GPU-less
runners. Timed cold build, same container, MEASURED: **29 s** for all
343 crates → the four release binaries, **41 s** more for the full
workspace test suite, on a Ryzen 9 9950X / 32 threads — scale
expectations down for laptops, and note target/ costs **7.8 GB**.)

The `libshim` dance in `demo-desktop.sh` is an **openSUSE** problem on the
development box (missing unversioned `.so` symlinks). Debian's `-dev`
packages ship those symlinks, so none of that applies here — and the scripts
now only put `/usr/lib64` on the loader path when that directory exists, so
they run unchanged.

## Building: the real decision

The workspace pulls in wgpu, smithay, cosmic-text and naga. That is a heavy
dependency graph, and this is where a 1 GB Pi will hurt.

### Option A — build on the Pi (simplest, slow)

Honest expectation: **hours**, and on 1 GB it will need help not to be
OOM-killed.

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh   # Pi OS rustc is too old for edition 2024
source "$HOME/.cargo/env"

# Swap is not optional at 1 GB. 2 GB of it, on the fastest disk you have.
sudo dphys-swapfile swapoff
sudo sed -i 's/^CONF_SWAPSIZE=.*/CONF_SWAPSIZE=2048/' /etc/dphys-swapfile
sudo dphys-swapfile setup && sudo dphys-swapfile swapon

git clone <your remote> rill && cd rill
# One codegen unit at a time: parallel rustc is what actually blows the box up.
CARGO_BUILD_JOBS=1 cargo build --release --workspace
```

Release only. A debug build of this tree is enormous (405 MiB for the
compositor on x86) and is not what you want to measure anyway.

If it still OOMs, build the crates you need rather than the workspace:

```bash
CARGO_BUILD_JOBS=1 cargo build --release \
    -p rill -p rill-compositor -p rill-vector -p files-app
```

### Option B — cross-compile from the workstation (faster, fiddlier)

Commands are in §5 of "From this machine" above. Two things that are easy to
get wrong and are not obvious from the outside:

* The binaries must land in **`~/rill/target/release/`** on the Pi.
  `bench-device.sh` resolves `$repo/target/$profile` and has no override.
* Set **`RILL_SKIP_BUILD=1`**, or `demo-desktop.sh` runs `cargo build` and
  rebuilds everything on the Pi anyway. Cross-copied binaries carry no cargo
  fingerprints, so the build is never a no-op.

You still need the repo on the Pi regardless: the setup script builds the
showcase packs from `apps/showcase/` and installs rices and shaders from
`assets/`.

**Recommendation:** if the Pi is a 4 GB+ Pi 5, build on it (Option A) — it is
slower but has no cross-compilation surprises. If it is the 1 GB Pi 4, try
Option B first; a 1 GB board building wgpu is a bad evening.

## First light

The environment from §4 above has to be in place — `WAYLAND_DISPLAY` and
`XDG_RUNTIME_DIR` pointing at the Pi's own session. Check before launching,
because the failure is otherwise a puzzle:

```bash
echo "$WAYLAND_DISPLAY $XDG_RUNTIME_DIR"     # both must be set and real
ls "$XDG_RUNTIME_DIR"/wayland-*
```

Pin the driver in the same shell (see "what will probably break" below —
worth doing from the very first launch, not after a confusing result):

```bash
export VK_DRIVER_FILES=/usr/share/vulkan/icd.d/broadcom_icd.json
```

Then the same script the workstation uses:

```bash
cd ~/rill
scripts/demo-desktop.sh --launch      # RILL_SKIP_BUILD=1 too, if you cross-compiled
```

The window opens **on the Pi's display** — the monitor, or the VNC session if
that is how you gave it one. Nothing appears on the workstation.

Expected to appear: a nested window with the dock, a wallpaper, and the
demo apps installed. Watch the first lines it prints:

```text
rill-compositor: wgpu on <adapter> (Vulkan, ..., driver ...)
rill-compositor: N importable dmabuf formats
```

That adapter line is the one to copy into the run notes: it is what makes a
V3D measurement comparable to the NVIDIA ones.

## The measurement

This is the point of the trip. `bench-device.sh` is written to run
unchanged:

```bash
scripts/bench-device.sh \
    --profile release \
    --idle-seconds 60 \
    --busy-seconds 60 \
    --scale 1,5,10 \
    --notes "Pi 4 1GB, Bookworm, V3DV, nested in labwc"
```

Note `--scale 1,5,10` rather than `1,5,10,20`. Twenty vector clients on a
1 GB board may well not fit — and if it does not, that is a *result*: the
script records `reached_apps` and `limit_reason` and keeps everything it
collected rather than failing. Start at 10 and raise it if there is room.

Run it from the same shell as first light, so it inherits `WAYLAND_DISPLAY`,
`VK_DRIVER_FILES` and (if cross-compiled) `RILL_SKIP_BUILD`. Two things the
harness now does for you, added 2026-08-14:

* **`graphics.adapter` and `graphics.software_renderer`** in `summary.json`,
  parsed from the compositor's own startup line, with a loud warning in
  `summary.txt` when the renderer is llvmpipe/lavapipe. This is the single
  most likely way for a Pi run to be quietly meaningless, and it no longer
  requires you to notice a log line.
* **`graphics.throttled`**, from `vcgencmd get_throttled`, read before load
  and again after the busy phase. Absent hardware simply records nothing.

Bring the bundle home rather than reading it over SSH:

```bash
# from the workstation
rsync -a pi@raspberrypi.local:~/rill/bench-results/ ~/rill/bench-results/
python3 -m json.tool bench-results/<run>/summary.json | head -40
```

What to expect, stated so a surprise is recognisable:

* **Idle stack PSS far below the workstation's 263 MiB.** The NVIDIA driver
  is ~200 MiB of that; V3DV is a fraction. If the Pi's idle total is not
  dramatically lower, something is wrong — most likely a software Vulkan
  fallback (`lavapipe`), which the adapter line will have told you.
* **The slope should hold.** 2.88 MiB/app was measured to N=20 on x86. If
  the Pi's slope is in the same neighbourhood, the "fixed platform cost + N ×
  small per-app cost" claim survives its first hardware change, which is the
  entire architectural result.
* **The damage gate should behave identically** — idle frames almost all
  heartbeat. It is CPU-side logic and has no reason to care about the GPU.
* **CPU percentages will be much larger.** A Pi core is not a 9950X core, and
  the figure is percent-of-one-core. Do not read that as a regression.
* **Thermals will matter here in a way they do not on the workstation.**
  Check `vcgencmd get_throttled` after the run; a throttled run is still a
  valid measurement but must be labelled.

Then append the run to [memory-footprint.md](memory-footprint.md) as a dated
entry, and — only then — update the PROJECTED lines in
[resource-envelope.md](resource-envelope.md) that this replaces.

## What will probably break, and what to do

Ordered by how likely I think it is.

**The dmabuf extension check fails.** Covered above. Decide deliberately;
record the decision.

**wgpu picks the wrong adapter.** If `lavapipe` (software) is installed it
may be chosen, and every number becomes meaningless. Pin it:

```bash
export VK_DRIVER_FILES=/usr/share/vulkan/icd.d/broadcom_icd.json
```

This is also the fix for the "Mesa ICDs loaded but unused" waste noted in
memory-footprint.md, so it is worth doing on the Pi from the start.

**A compute shader fails to compile or run.** The wallpaper particle passes
(`boids`, `dust_update`) are compute, and V3D's limits are much tighter than
a desktop GPU's. If the desktop comes up but particles do not, drop them
from the theme (`[desktop] boids` / `particle_shader`) and record it — the
idle and scaling numbers are unaffected, and that is what the trip is for.

**Fonts render but slowly, or the glyph atlas thrashes.** The atlas is
1024² and shared; at 1080p it should be ample.

**The build OOMs.** See the swap and `CARGO_BUILD_JOBS=1` notes above.

**Everything works but it is visibly slow.** Check the governor
(`cat /sys/devices/system/cpu/cpu0/cpufreq/scaling_governor`) and the
throttling flags before concluding anything about Rill.

## A cheaper variant worth knowing about

If the Pi cannot build, or cannot serve, there is a shortcut that is also a
better demonstration of the architecture: **run the server on the
workstation and make the Pi pure glass.**

```bash
# workstation: serve, and let the network in
scripts/demo-desktop.sh          # note the port

# pi: run only the compositor + a vector client pointed at that server
```

The Pi then needs `rill-compositor` and `rill-vector`, not `files-app`, and
the apps' documents come over the wire. This is the "devices are glass"
thesis in its literal form, and it measures the *client* cost on Pi hardware
— which is the number that decides whether cheap glass is viable — without
needing the Pi to run a server at all.

It does not replace the full measurement (no server process in the totals),
so record which variant a run used.

## What this does not get you

Worth writing down so it does not get quietly claimed later:

* **Boot-to-desktop time.** Needs milestone 15 (DRM/libinput backend). The
  benchmark has no such field — an earlier draft of this document claimed it
  reported `boot_to_shell_ms: null`, which was generous: the field does not
  exist, and the honest reading is that nothing measures this yet.
* **Power figures.** The Pi exposes no whole-board power sensor; the script
  will correctly say unavailable. A USB-C inline meter is the honest way,
  and it is a manual reading.
* **The appliance.** Ladder rung 3 in `specs/appliance.md` is a built image,
  not a desktop session with Rill nested inside it.
* **Anything about long-term reliability.** A benchmark is minutes.
  risks.md #4 wants 72-hour and 7-day soaks, and this is not that —
  the soak protocol (headless display included) is [pi-soak.md](pi-soak.md).

## The plan, condensed

```text
0. Find it (mDNS / tailnet / ping sweep + OUI), ssh-copy-id.
1. Confirm model, aarch64, RAM — and the five Vulkan extensions.  ← go/no-go
2. Confirm a Wayland session exists to nest in (graphical.target,
   Type=wayland, a wayland-N socket). Give it eyes: monitor or VNC.
3. Install the dev packages and a current rustup toolchain.
4. rsync the repo to ~/rill. Build release: on the Pi if 4 GB+,
   cross-compiled into ~/rill/target/release/ + RILL_SKIP_BUILD=1 if 1 GB.
5. Export XDG_RUNTIME_DIR / WAYLAND_DISPLAY / VK_DRIVER_FILES.
6. scripts/demo-desktop.sh --launch — first light, on the Pi's screen.
7. scripts/bench-device.sh --profile release --scale 1,5,10.
8. rsync the bundle back. Check graphics.software_renderer is false.
9. Append the run to docs/memory-footprint.md, dated.
10. Only then rewrite the PROJECTED lines it replaces.
```

Steps 1 and 2 are the whole risk, and both are answerable in one SSH round
trip each. Do them before anything else, from an SD card you already have,
even if the rest waits a week.
