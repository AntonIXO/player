#!/bin/sh
# Deploy the freshly on-device-built player-gtk and restart it in the phosh
# session. Run as root on the device after `cargo build --release -p player-gtk`
# in /root/player. Pairs with phone-shot.sh for the design debug loop.
BIN=/root/player/target/release/player-gtk
# Hard-kill by PID (single-instance GtkApplication; pkill -x was unreliable here).
for P in $(pgrep -x player-gtk); do kill -9 "$P" 2>/dev/null; done
sleep 2
install -m755 "$BIN" /usr/bin/player-gtk
su antonix -c 'setsid sh -c "exec env WAYLAND_DISPLAY=wayland-0 XDG_RUNTIME_DIR=/run/user/10000 DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/10000/bus GSK_RENDERER=vulkan player-gtk" >/tmp/pg.log 2>&1 </dev/null &'
sleep 8
echo "pid=$(pgrep -x player-gtk) css_parser_errors=$(grep -c 'No property named' /tmp/pg.log 2>/dev/null)"
