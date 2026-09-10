#!/usr/bin/env python3
"""Minimal /dev/uinput injector for 15b input verification — no deps beyond
the stdlib. Creates a virtual keyboard + relative pointer, waits for the
compositor's libinput to notice the hotplug, then plays a scripted sequence
so a screenshot can show the desktop responding. Needs write access to
/dev/uinput (the 99-rill-uinput.rules opens it to the `input` group).

  uinput-inject.py move DX DY        relative pointer motion
  uinput-inject.py click             left button press+release at current spot
  uinput-inject.py key NAME[,NAME2]  tap keys together (e.g. LEFTCTRL,LEFTSHIFT,R)
  uinput-inject.py demo              a move, a dock-logo click, then Ctrl+Shift+R
"""
import ctypes, fcntl, struct, sys, time

UINPUT = "/dev/uinput"
EV_SYN, EV_KEY, EV_REL = 0x00, 0x01, 0x02
SYN_REPORT = 0
REL_X, REL_Y = 0x00, 0x01
BTN_LEFT = 0x110
UI_SET_EVBIT, UI_SET_KEYBIT, UI_SET_RELBIT = 0x40045564, 0x40045565, 0x40045566
UI_DEV_CREATE, UI_DEV_DESTROY = 0x5501, 0x5502
# A slice of the evdev keycodes we might tap (linux/input-event-codes.h).
KEYS = {"LEFTCTRL":29,"LEFTSHIFT":42,"LEFTALT":56,"R":19,"A":30,"F11":87,"ENTER":28,"ESC":1}

def _emit(fd, typ, code, val):
    # struct input_event: timeval(16) + type(2) + code(2) + value(4)
    fd.write(struct.pack("@llHHi", 0, 0, typ, code, val))
    fd.flush()

def _syn(fd):
    _emit(fd, EV_SYN, SYN_REPORT, 0)

def open_device():
    fd = open(UINPUT, "wb", buffering=0)
    for code in (EV_KEY, EV_REL, EV_SYN):
        fcntl.ioctl(fd, UI_SET_EVBIT, code)
    for kc in list(KEYS.values()) + [BTN_LEFT]:
        fcntl.ioctl(fd, UI_SET_KEYBIT, kc)
    for r in (REL_X, REL_Y):
        fcntl.ioctl(fd, UI_SET_RELBIT, r)
    # struct uinput_user_dev: name[80] + id(8) + ff_effects_max(4) + absmax/min/fuzz/flat[4*64*4]
    name = b"rill-15b-virtual".ljust(80, b"\0")
    idbytes = struct.pack("@HHHH", 0x03, 0x1234, 0x5678, 1)  # BUS_USB
    body = name + idbytes + struct.pack("@i", 0) + b"\0" * (4 * 64 * 4)
    fd.write(body); fd.flush()
    fcntl.ioctl(fd, UI_DEV_CREATE)
    time.sleep(1.2)  # let udev announce it and libinput add it
    return fd

def close_device(fd):
    try: fcntl.ioctl(fd, UI_DEV_DESTROY)
    except OSError: pass
    fd.close()

def move(fd, dx, dy):
    _emit(fd, EV_REL, REL_X, dx); _emit(fd, EV_REL, REL_Y, dy); _syn(fd)

def click(fd):
    _emit(fd, EV_KEY, BTN_LEFT, 1); _syn(fd); time.sleep(0.05)
    _emit(fd, EV_KEY, BTN_LEFT, 0); _syn(fd)

def tap(fd, names):
    codes = [KEYS[n] for n in names]
    for c in codes: _emit(fd, EV_KEY, c, 1); _syn(fd); time.sleep(0.02)
    time.sleep(0.05)
    for c in reversed(codes): _emit(fd, EV_KEY, c, 0); _syn(fd); time.sleep(0.02)

def main():
    if len(sys.argv) < 2:
        print(__doc__); return 2
    cmd = sys.argv[1]
    fd = open_device()
    try:
        if cmd == "move":
            move(fd, int(sys.argv[2]), int(sys.argv[3]))
        elif cmd == "click":
            click(fd)
        elif cmd == "key":
            tap(fd, sys.argv[2].split(","))
        elif cmd == "demo":
            # From wherever the cursor sits, walk to the top-left dock logo,
            # click it (opens the launcher menu), then cycle the rice.
            for _ in range(40): move(fd, -40, -40); time.sleep(0.01)
            time.sleep(0.3); click(fd); time.sleep(0.5)
            tap(fd, ["LEFTCTRL","LEFTSHIFT","R"]); time.sleep(0.3)
        else:
            print(__doc__); return 2
        time.sleep(0.3)
    finally:
        close_device(fd)
    return 0

if __name__ == "__main__":
    sys.exit(main())
