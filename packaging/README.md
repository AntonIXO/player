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

# 1. stage the package into the pmaports checkout and build it
cp -r packaging/aports/hifi-player \
      ~/.local/var/pmbootstrap/cache_git/pmaports/temp/
pmbootstrap checksum hifi-player
pmbootstrap build --arch aarch64 --src="$PWD" hifi-player   # first build is slow (qemu)

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
