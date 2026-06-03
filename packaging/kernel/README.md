# Custom kernel: permanent USB host mode + audiophile tuning (beryllium)

Canonical, version-controlled artifacts for rebuilding the postmarketOS beryllium
kernel (`linux-postmarketos-qcom-sdm845`) so the Chord Mojo 2 USB DAC enumerates on
every boot, plus the remaining audiophile config deltas. These are **spliced into the
pmaports kernel package at build time** (the same pattern as `../aports/hifi-player/`),
because the in-tree pmaports copy gets wiped by `pmbootstrap pull`.

| File | Role |
|---|---|
| `0002-…-force-usb-host-mode.patch` | DTS patch: `&usb_1_dwc3` `dr_mode = "peripheral"` → `"host"` in the shared `sdm845-xiaomi-beryllium-common.dtsi` (covers both EBBG + Tianma panels). |
| `audiophile.kconfig` | Reference list of the kernel-config deltas to apply (`NO_HZ_FULL`, `SND_USB_AUDIO=y`). Most audiophile options ship enabled already. |

> **Permanent host-only USB.** After this, the USB-C port is host-only every boot: no
> charging, no adb/usb-networking while booted. `fastboot` from the bootloader still
> works. Dev-iterate over SSH/Wi-Fi (`pmbootstrap sideload` over USB-net won't work).

## Prerequisites

- Local pmaports checkout at `~/.local/var/pmbootstrap/cache_git/pmaports/`.
- Kernel package at `device/community/linux-postmarketos-qcom-sdm845/`, built with `LLVM=1`.
  Run `pmbootstrap pull` first so it matches the kernel the phone runs (currently **7.1_rc1**,
  tag `sdm845-7.1-rc1-r0`). The `gitlab.com/postmarketOS` mirror lags — trust the canonical
  `gitlab.postmarketos.org` checkout that `pmbootstrap pull` syncs.
- Panel variant already chosen at `pmbootstrap init` (tianma vs ebbg).

## De-risk first (optional, ~2 min)

Prove host mode works before the ~30-min build — on the running phone, either force it
at runtime (`echo host > /sys/.../a600000.dwc3*/mode`, as the hifi-player init script
does) or repack the DTB by hand (`dtc` decompile → `sed peripheral→host` → `dtc` →
`abootimg`). If `lsusb | grep -i chord` shows the Mojo 2, the patch below makes it permanent.

## Build & flash

```sh
KDIR=~/.local/var/pmbootstrap/cache_git/pmaports/device/community/linux-postmarketos-qcom-sdm845

# 1. Splice the DTS patch into the kernel package
cp packaging/kernel/0002-arm64-dts-qcom-sdm845-xiaomi-beryllium-force-usb-host-mode.patch "$KDIR"/
#    then, in $KDIR/APKBUILD:
#      - add the patch filename to source= (it's the only patch; 0001 was upstreamed in 7.x)
#      - bump pkgrel (e.g. 1 -> 2)
#      - update sha512sums for the patch AND the edited config (sha512sum each, or run:)
pmbootstrap checksum linux-postmarketos-qcom-sdm845   # note: downloads the kernel tarball

# 2. Apply the config deltas (see audiophile.kconfig), then validate
pmbootstrap kconfig edit linux-postmarketos-qcom-sdm845   # set NO_HZ_FULL=y, SND_USB_AUDIO=y
pmbootstrap kconfig check linux-postmarketos-qcom-sdm845

# 3. Build the kernel (LLVM; tens of minutes), then bake into the image
pmbootstrap build --arch aarch64 linux-postmarketos-qcom-sdm845
pmbootstrap build --src="$PWD" hifi-player          # picks up the new cmdline/initd
pmbootstrap install --add hifi-player --filesystem f2fs

# 4. Flash (device in fastboot)
pmbootstrap flasher flash_kernel                    # carries the new dtb (deviceinfo_append_dtb)
pmbootstrap flasher flash_rootfs
fastboot reboot                                     # NOT the power button
```

## Verify on-device

```sh
lsusb | grep -i chord                               # DAC enumerates with NO manual echo host
cat /proc/cmdline                                   # nohz_full=7 isolcpus=7 snd_usb_audio.nrpacks=1 …
zcat /proc/config.gz | grep -E 'NO_HZ_FULL|RCU_NOCB|SND_USB_AUDIO'   # all =y
```

## Recovery

If the new kernel won't boot, re-flash the previous boot image from fastboot
(`fastboot flash boot <old>.img`) or revert the `pkgrel`/config and rebuild.
