# postmarketOS package (`hifi-player`)

Builds the player as an Alpine **`.apk`** for the **Poco F1 (beryllium / SDM845,
aarch64)** running postmarketOS, bundling the bit-perfect player (`player-gtk` +
`player-cli`) with the audio-optimization config from the install guide.

`aports/hifi-player/` is the package source (APKBUILD + systemd services + greetd/Phosh
autologin + user-session autostart + udev / limits / modprobe / sysctl / kernel-cmdline
files + Phosh launcher). It targets the
postmarketOS **systemd** variant. It is built **natively
inside pmbootstrap's aarch64 chroot** (qemu-emulated) — no host cross-compile, so the
whole GTK4 workspace links against Alpine's musl libraries.

## What gets installed

| Path | Purpose |
|---|---|
| `/usr/bin/player-gtk`, `/usr/bin/player-cli` | the player + CLI |
| `/usr/share/applications/hifi-player.desktop` + icon | Phosh app-grid entry |
| `/etc/xdg/autostart/hifi-player-autostart.desktop` | launch `player-gtk` inside the authenticated Phosh user session |
| `greetd.service.d/90-hifi-player-autologin.conf` + `/usr/libexec/hifi-player-greetd-config` | greetd `initial_session` for the non-root `/etc/default_user`, with phrog retained as the fallback greeter |
| `/etc/default/hifi-player` | reversible boot policy plus battery thresholds (`HIFI_PLAYER_AUTOLOGIN` / `HIFI_PLAYER_AUTOSTART` / `HIFI_PLAYER_CHARGE_LIMIT`) |
| `/usr/bin/hifi-player-audio-setup` + `…/systemd/system/hifi-player-audio-setup.service` (+ preset) | systemd oneshot: USB host mode (OTG workaround), perf governor on CPUs 4-7, SCHED_FIFO on USB IRQ threads |
| `/usr/libexec/hifi-player-charge-limit` + `hifi-player-charge-limit.service` + udev rule | restore the standard PMI8998 battery threshold at boot, battery registration, and resume; no-op on unsupported kernels |
| `/etc/udev/rules.d/99-mojo2-nopulse.rules` | keep the sound server off the DAC |
| `/etc/udev/rules.d/99-cpu-dma-latency.rules` | audio-group access to `/dev/cpu_dma_latency` |
| `/etc/security/limits.d/99-audio.conf` | `@audio` rtprio/memlock/nice |
| `/etc/modprobe.d/audio.conf` | `snd-usb-audio nrpacks=1 low_latency=1` |
| `/etc/sysctl.d/99-audio-sysctl.conf` | swappiness / dirty ratios |
| `/etc/kernel-cmdline.d/90-audio.conf` | `threadirqs usbcore.autosuspend=-1 processor.max_cstate=1 snd-usb-audio.nrpacks=1` |
| `/usr/lib/systemd/system/var-log.mount` | volatile `/var/log` on tmpfs (16M cap) — cut flash writeback; see `docs/ARCHQ.md` |
| `/usr/share/hifi-player/cmdline-experimental.conf` | inert (OFF) A/B knob `skew_tick=1 rcu.blimit=64` — opt-in only; see `docs/ARCHQ.md` §9 |

### Fast Phosh boot

The image uses the current postmarketOS `systemd + greetd + phrog + Phosh` path.
The package does not auto-login root or bypass PAM: greetd starts a non-root
`initial_session` using `/usr/bin/phosh-session`, and `player-gtk` is launched by
Phosh's user-session autostart once Wayland and the user D-Bus are ready. The normal
phrog greeter remains in the generated config as the recovery path. Set
`HIFI_PLAYER_AUTOLOGIN=0` or `HIFI_PLAYER_AUTOSTART=0` in `/etc/default/hifi-player`
to opt out and reboot.

The audio setup unit no longer waits for `sound.target`; it only waits for udev and
is ordered before greetd. This lets it assert USB host mode before the graphical
session without introducing a fixed sleep or making the player depend on a slow
sound-device enumeration.

### Battery charge limit

The stock 7.1.6 qcom_smbx/pmi8998 drivers expose capacity and charger enable, but
not a percentage threshold. The bundled kernel patch connects those existing
controls through Linux's standard `charge_control_end_threshold` and
`charge_control_start_threshold` battery properties. The fuel-gauge SOC interrupt
enforces the end value in-kernel and resumes at the start value, avoiding a polling
daemon and keeping the USB power path available while battery charging is inhibited.

The package applies 90% with 85% resume hysteresis by default. Verify on the device:

```sh
cat /sys/class/power_supply/qcom-battery/charge_control_end_threshold
cat /sys/class/power_supply/qcom-battery/charge_control_start_threshold
```

Set `HIFI_PLAYER_CHARGE_LIMIT=0` in `/etc/default/hifi-player` and restart
`hifi-player-charge-limit.service` to restore full charging. If the files are
absent, install/rebuild the bundled `linux-postmarketos-qcom-sdm845-audio` kernel;
the helper intentionally does not guess at vendor-specific sysfs nodes.

## Build & ship

```sh
# 0. (once) select the device profile for image builds
pmbootstrap init            # xiaomi / beryllium / panel tianma|ebbg / phosh / edge / f2fs

# 1. link the package into pmaports once — builds straight from this repo and
#    survives `pmbootstrap pull` (drops a symlink into pmaports' git-ignored
#    custom-player/ dir; no copying, correct channel). Re-run after `pmbootstrap init`.
sh packaging/aports/link-into-pmaports.sh
pmbootstrap build --arch aarch64 --src="$PWD" hifi-player

# 2a. bake into the image …
pmbootstrap install --add hifi-player --filesystem f2fs     # +--fde for encryption
pmbootstrap flasher flash_kernel
pmbootstrap flasher flash_rootfs --partition userdata
fastboot reboot             # NOT the power button

# 2b. … or push just the APK to an already-running device (dev loop)
pmbootstrap build --src="$PWD" hifi-player
pmbootstrap sideload --host 172.16.42.1 --user <user> --arch aarch64 hifi-player
```

After first install, on the device: `sudo adduser <user> audio` (re-login), then
`sudo apk fix linux-postmarketos-qcom-sdm845-audio` + reflash/reboot to apply the kernel
cmdline. See `aports/hifi-player/hifi-player.post-install` for the full checklist.

> **No USB charging or MTP while booted.** The custom kernel pins the USB-C port to host
> mode and sources its own 5 V, so the port can't take charge in or act as a peripheral.
> Charge with the phone **powered off** (or in fastboot); transfer files over Wi-Fi
> (SSH/SFTP/`rsync`) or via the microSD card. See the APKBUILD and patches in `linux-postmarketos-qcom-sdm845-audio/`.

> **USB OTG is officially "Broken" on beryllium.** The whole Mojo-2-over-USB path
> depends on the custom kernel: patch `0002` forces host mode and patches `0004`/`0005`
> make the phone source its own 5 V VBUS (PMI8998 OTG boost), so the DAC enumerates over
> a **plain** OTG cable — no powered Y-cable, and do **not** also enable the Mojo 2's own
> power-output mode (two 5 V sources must not fight on the bus). Verify
> `lsusb | grep -i chord` enumerates the DAC before anything else. See the patches in `linux-postmarketos-qcom-sdm845-audio/`.

## Source of truth & `pmbootstrap pull`

This repo is canonical; pmbootstrap's `~/.local/var/pmbootstrap/cache_git/pmaports` is just
a build checkout of upstream that `pmbootstrap pull` fast-forwards — **only when its git tree
is clean.**

- **hifi-player** — handled by `aports/link-into-pmaports.sh`: it symlinks the package into
  pmaports' git-ignored `custom-player/` directory (pmaports' `.gitignore` whitelists
  `custom-*/` for exactly this). So it builds from this repo, keeps the right channel
  (e.g. `systemd-edge`, since it lives *inside* pmaports — unlike a `config aports` overlay,
  which would become pkgrepo[0] and flip channel detection), and is invisible to git →
  **never blocks or gets overwritten by a pull.** Re-run the script after a fresh `pmbootstrap init`.

- **kernel** — shipped as a self-contained, renamed fork package,
  `linux-postmarketos-qcom-sdm845-audio`, at `linux-postmarketos-qcom-sdm845-audio/` (a copy
  of the upstream aport with the `0002`–`0006` patches in `source=` and
  `REGULATOR_QCOM_USB_VBUS=y` applied to the config). `link-kernel-into-pmaports.sh` symlinks
  it into pmaports' git-ignored `custom-kernel/` dir, so it builds from this repo and is
  invisible to `pmbootstrap pull`. It keeps the upstream `_flavor` (identical boot/dtb/module
  artifacts) and `provides`/`replaces` the upstream package so `--add` swaps it in cleanly.
  The renamed pkgname is what lets it coexist in pmaports (pmbootstrap rejects two aports
  with the same `pkgname`, so a same-named `custom-*` override is impossible).

  **Kernel base: mainline stable `v7.1.6` (kernel.org), not the fork's tarball.** Upstream
  pmaports still builds `sdm845-mainline`'s own `sdm845-7.1-rc1-r0` archive, i.e. plain
  **7.1.0-rc1**; that fork has not been rebased since 2026-05-08, so it misses every 7.1.y
  stable fix (USB-audio fixes among them, plus the beryllium "Correct IPA FW path" DTS fix).
  This package therefore sources `linux-$pkgver.tar.xz` from kernel.org and carries the fork's
  device delta as `0001-sdm845-mainline-device-support-on-v7.1.6.patch` (generated by merging
  `v7.1.6` into `sdm845/7.1-dev`; the patch header records the eight conflict resolutions).
  To move to a newer 7.1.y: re-run that merge in a linux-stable clone, regenerate the `0001`
  diff, bump `pkgver`, then re-run `make ARCH=arm64 LLVM=1 olddefconfig` **with the whole patch
  series applied** (the `enable-dynamic-ftrace.patch` unlocks symbols like `HID_BPF`, so a
  config generated without it makes `syncconfig` prompt mid-build) and refresh `sha512sums`.
