#!/bin/sh
# On-device screenshot helper for the Poco F1 / Phosh debug loop.
#
# Why this is non-trivial: phosh's wlr-screencopy only delivers a frame when the
# output has fresh *damage* (a repaint). A static screen (idle, or the lockscreen)
# makes `grim` block forever. So we: wake the screen, disable idle-blank + auto-lock
# for the session, then drive tiny virtual-pointer moves (wlrctl) to force repaints
# while grim captures. Run as root on the device; it su's to the phosh user.
#
# Usage: phone-shot.sh [/path/out.png]   (default /tmp/shot.png)

U=antonix
ENV="WAYLAND_DISPLAY=wayland-0 XDG_RUNTIME_DIR=/run/user/10000 DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/10000/bus"
run() { su "$U" -c "env $ENV $*" 2>/dev/null; }

OUT="${1:-/tmp/shot.png}"

# Wake the screen just for this capture (does NOT change persistent idle/lock
# settings — a 12 s grab finishes well within any idle-blank timeout).
run gdbus call --session -d org.gnome.ScreenSaver -o /org/gnome/ScreenSaver \
    -m org.gnome.ScreenSaver.SetActive false >/dev/null 2>&1

rm -f "$OUT"
run timeout 12 grim "$OUT" &
GPID=$!
i=0
while [ $i -lt 24 ]; do
    kill -0 "$GPID" 2>/dev/null || break
    run wlrctl pointer move 6 0 >/dev/null 2>&1
    run wlrctl pointer move -6 0 >/dev/null 2>&1
    sleep 0.25
    i=$((i + 1))
done
wait "$GPID"
echo "grim_size=$(stat -c %s "$OUT" 2>/dev/null) lock=$(run gdbus call --session -d org.gnome.ScreenSaver -o /org/gnome/ScreenSaver -m org.gnome.ScreenSaver.GetActive 2>/dev/null)"
