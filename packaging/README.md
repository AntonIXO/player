# postmarketOS package (`hifi-player`)

Builds the player as an Alpine **`.apk`** for the **Poco F1 (beryllium / SDM845,
aarch64)** running postmarketOS, bundling the bit-perfect player (`player-gtk` +
`player-cli`) with the audio-optimization config from the install guide.

`aports/hifi-player/` is the package source (APKBUILD + systemd service + udev / limits
/ modprobe / sysctl / kernel-cmdline files + Phosh launcher). It targets the
postmarketOS **systemd** variant. It is built **natively
inside pmbootstrap's aarch64 chroot** (qemu-emulated) — no host cross-compile, so the
whole GTK4 workspace links against Alpine's musl libraries.

## What gets installed

| Path | Purpose |
|---|---|
| `/usr/bin/player-gtk`, `/usr/bin/player-cli` | the player + CLI |
| `/usr/share/applications/hifi-player.desktop` + icon | Phosh app-grid entry |
| `/usr/bin/hifi-player-audio-setup` + `…/systemd/system/hifi-player-audio-setup.service` (+ preset) | systemd oneshot: USB host mode (OTG workaround), perf governor on CPUs 4-7, SCHED_FIFO on USB IRQ threads |
| `/etc/udev/rules.d/99-mojo2-nopulse.rules` | keep the sound server off the DAC |
| `/etc/udev/rules.d/99-cpu-dma-latency.rules` | audio-group access to `/dev/cpu_dma_latency` |
| `/etc/security/limits.d/99-audio.conf` | `@audio` rtprio/memlock/nice |
| `/etc/modprobe.d/audio.conf` | `snd-usb-audio nrpacks=1 low_latency=1` |
| `/etc/sysctl.d/99-audio-sysctl.conf` | swappiness / dirty ratios |
| `/etc/kernel-cmdline.d/90-audio.conf` | `threadirqs usbcore.autosuspend=-1 processor.max_cstate=1 snd-usb-audio.nrpacks=1` |

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
`sudo apk fix linux-postmarketos-qcom-sdm845` + reflash/reboot to apply the kernel
cmdline. See `aports/hifi-player/hifi-player.post-install` for the full checklist.

> **USB OTG is officially "Broken" on beryllium.** The whole Mojo-2-over-USB path
> depends on the host-mode workaround in the init script succeeding — verify
> `lsusb | grep -i chord` enumerates the DAC before anything else.

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

- **kernel** — `linux-postmarketos-qcom-sdm845` is an *upstream* package we override, so it
  can't use `custom-*`. Its canonical artifacts live in `../kernel/` (the `0002`/`0003`
  patches + `audiophile.kconfig`); the working edits sit directly in the pmaports kernel dir
  and **do** dirty the tree. To update pmaports:

  ```sh
  cd ~/.local/var/pmbootstrap/cache_git/pmaports
  git checkout -- device/community/linux-postmarketos-qcom-sdm845/        # drop our edits
  rm -f device/community/linux-postmarketos-qcom-sdm845/000[23]-*.patch   # drop staged patches
  pmbootstrap pull                                                        # clean now → updates
  ```
  then re-apply from `../kernel/` per its README (copy the patches, add them to `source=`,
  bump `pkgrel`, re-apply the `audiophile.kconfig` deltas, `pmbootstrap checksum`).
