#!/bin/sh
# Tap at logical (x,y) on the Poco F1 / Phosh via a wlroots virtual pointer.
# The seat cursor position is global, so we "home" it to the top-left corner
# (a large negative relative move clamps to 0,0) then move to the target and
# click. Coordinates are in logical pixels (540-wide at scale 2). Run as root.
# Usage: phone-tap.sh <x> <y>
U=antonix
ENV="WAYLAND_DISPLAY=wayland-0 XDG_RUNTIME_DIR=/run/user/10000 DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/10000/bus"
run() { su "$U" -c "env $ENV $*" 2>/dev/null; }
run wlrctl pointer move -4000 -6000
run wlrctl pointer move "$1" "$2"
run wlrctl pointer click
