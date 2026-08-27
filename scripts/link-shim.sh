#!/usr/bin/env bash
#
# Development link shim — source this, don't run it:  . scripts/link-shim.sh
#
# Some distributions (openSUSE among them) ship the runtime libraries this
# workspace links against but not the unversioned `.so` symlinks the linker
# looks for, and put 64-bit libraries in /usr/lib64 rather than /usr/lib. The
# missing pieces are exactly the `-devel` packages; the libraries themselves
# are present. Rather than require every contributor to install a list of
# packages, this recreates the few symlinks in a private directory and points
# the linker at it.
#
# **Every part of this is conditional.** On a distribution that has the dev
# symlinks — Debian, Arch, Fedora with -devel installed — sourcing this does
# nothing at all and exports nothing. It is not a shared build configuration;
# it is a local workaround that knows when it isn't needed. Override the
# location with $RILL_LIBSHIM.

_rill_shim_dir="${RILL_LIBSHIM:-$HOME/.local/share/rill/libshim}"

# Nothing to do unless this box actually keeps its libraries in /usr/lib64.
if [ -d /usr/lib64 ]; then
    # name -> the versioned file it should point at
    _rill_shim_libs="
libwayland-server.so:libwayland-server.so.0
libwayland-client.so:libwayland-client.so.0
libwayland-cursor.so:libwayland-cursor.so.0
libwayland-egl.so:libwayland-egl.so.1
libxkbcommon.so:libxkbcommon.so.0
libxkbcommon-x11.so:libxkbcommon-x11.so.0
libEGL.so:libEGL.so.1
libGL.so:libGL.so.1
libGLESv2.so:libGLESv2.so.2
libgbm.so:libgbm.so.1
"
    _rill_shim_made=0
    for _entry in $_rill_shim_libs; do
        _name="${_entry%%:*}"
        _real="${_entry##*:}"
        # Only when the proper symlink is genuinely absent and the versioned
        # library is genuinely there.
        if [ ! -e "/usr/lib64/$_name" ] && [ ! -e "/usr/lib/$_name" ] \
           && [ -e "/usr/lib64/$_real" ]; then
            mkdir -p "$_rill_shim_dir"
            ln -sf "/usr/lib64/$_real" "$_rill_shim_dir/$_name"
            _rill_shim_made=1
        fi
    done

    # ALSA reaches the build through rodio -> alsa-sys, which asks pkg-config
    # for `alsa.pc` and panics without it. alsa-sys ships pregenerated
    # bindings, so no headers are needed — only the .pc file and the link.
    # Installing `alsa-devel` retires this automatically.
    if ! pkg-config --exists alsa 2>/dev/null && [ -e /usr/lib64/libasound.so.2 ]; then
        mkdir -p "$_rill_shim_dir/pkgconfig"
        ln -sf /usr/lib64/libasound.so.2 "$_rill_shim_dir/libasound.so"
        cat > "$_rill_shim_dir/pkgconfig/alsa.pc" <<PC
prefix=/usr
libdir=$_rill_shim_dir
includedir=/usr/include

Name: alsa
Description: Advanced Linux Sound Architecture (rill dev shim)
Version: 1.2.14
Libs: -L\${libdir} -lasound
Cflags:
PC
        export PKG_CONFIG_PATH="$_rill_shim_dir/pkgconfig${PKG_CONFIG_PATH:+:$PKG_CONFIG_PATH}"
        _rill_shim_made=1
    fi

    if [ "$_rill_shim_made" = 1 ]; then
        export RUSTFLAGS="-L $_rill_shim_dir${RUSTFLAGS:+ $RUSTFLAGS}"
        export LD_LIBRARY_PATH="/usr/lib64${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
        echo "  (using dev link shim at $_rill_shim_dir)" >&2
    fi
    unset _entry _name _real _rill_shim_libs _rill_shim_made
fi

unset _rill_shim_dir
