#!/bin/sh
# Restart player-gtk (already installed at /usr/bin) optionally on a given start
# page via the temp PLAYER_START_PAGE debug hook. Run as root on the device.
# Usage: phone-launch.sh [library|playing|search|lists]
PAGE="$1"
# Hard-kill by PID: player-gtk is a single-instance GtkApplication, so a relaunch
# while one is alive just activates the old instance (pkill -x proved unreliable
# here). Kill all and confirm gone before relaunching.
for P in $(pgrep -x player-gtk); do kill -9 "$P" 2>/dev/null; done
sleep 2
su antonix -c "setsid sh -c 'exec env WAYLAND_DISPLAY=wayland-0 XDG_RUNTIME_DIR=/run/user/10000 DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/10000/bus GSK_RENDERER=vulkan PLAYER_START_PAGE=$PAGE player-gtk' >/tmp/pg.log 2>&1 </dev/null &"
sleep 9
echo "pid=$(pgrep -x player-gtk) start_page=$PAGE"
