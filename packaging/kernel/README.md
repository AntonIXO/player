# Custom kernel: permanent USB host mode + self-powered VBUS + audiophile tuning (beryllium)


Reference/authoring artifacts for the beryllium audio kernel: the per-patch rationale and
the annotated config deltas (`audiophile.kconfig`). The **buildable** kernel is the
self-contained package next door at
[`../linux-postmarketos-qcom-sdm845-audio/`](../linux-postmarketos-qcom-sdm845-audio/) —
a renamed copy of the upstream `linux-postmarketos-qcom-sdm845` aport with these patches
in `source=` and the config delta already applied. Build from there; this dir just
documents *what* changed and *why*.

> **Why a renamed package.** pmbootstrap errors on two aports sharing a `pkgname`, so we
> can't drop a same-named override into pmaports' `custom-*/`. The package is renamed to
> `linux-postmarketos-qcom-sdm845-audio` and `provides`/`replaces` the upstream one; it
> keeps the same `_flavor`, so it builds identical boot/dtb/module artifacts. It is linked
> into pmaports' git-ignored `custom-kernel/` by [`../link-kernel-into-pmaports.sh`](../link-kernel-into-pmaports.sh),
> so it builds straight from this repo and survives `pmbootstrap pull`.

| File | Role |
|---|---|
| `0002-…-force-usb-host-mode.patch` | DTS: `&usb_1_dwc3` `dr_mode = "peripheral"` → `"host"` in the shared `sdm845-xiaomi-beryllium-common.dtsi` (covers both EBBG + Tianma panels). |
| `0003-…-disable-internal-audio.patch` | DTS: `status="disabled"` on `&sound`/`&wcd9340`/`&slim`/`&slimbam` — frees SLIMbus + the internal codec (audio is USB-only). Reversible. |
| `0004-…-regulator-qcom_usb_vbus-…-add-pmi8998-support.patch` | Driver: teaches `drivers/regulator/qcom_usb_vbus-regulator.c` the `qcom,pmi8998-vbus-reg` compatible (same OTG register layout as pm8150b, different current table). Backport of the upstream linux-arm-msm series; drop once it lands. |
| `0005-…-self-power-usb-host-vbus.patch` | DTS: adds the PMI8998 OTG-boost regulator (`usb-vbus-regulator@1100`, `regulator-always-on`) so the phone sources its own 5 V VBUS in host mode — the DAC enumerates over a **plain** OTG cable, no powered Y-cable / Mojo power mode. |
| `0006-sound-usb-tune-usb-audio-compiler-flags.patch` | Build: adds `ccflags-y += -falign-functions=32 -fno-math-errno -fno-trapping-math` to `sound/usb/Makefile` (icache-aligned hot callbacks + relaxed FP on the snd-usb-audio objects). Safe subset of the audio-tuning guide §5.3, adapted to mainline + plain clang — see the note below. Marginal; the real low-latency wins are runtime (`hifi-player`). |
| `audiophile.kconfig` | Kernel-config deltas: `NO_HZ_FULL`+`RCU_NOCB`, `LTO_CLANG_THIN`, `SECURITY_YAMA` (build-gate fix), `REGULATOR_QCOM_USB_VBUS=y` (for patches 0004/0005), and the ALSA core + `SND_USB_AUDIO` built-in `=y` (needs `SOUND`/`SND`/`SND_PCM`/… `=y`). Most audiophile options ship enabled already. |

> **Permanent host-only USB — read before you plug anything in.** The USB-C port is host
> mode every boot *and now actively sources 5 V* (always-on OTG boost). While booted:
> **no charging, no MTP/adb, no usb-networking.** Do **not** plug a charger or the Mojo's
> own power-output mode into the port while booted — two 5 V sources must not fight.
> - **Charge:** power the phone **off**, then plug a charger — the host-forcing only
>   applies while this kernel runs, so the PMIC charges normally (fastboot/bootloader is a
>   fallback). `fastboot` from the bootloader is unaffected.
> - **File transfer:** over the network (SSH/SFTP/`rsync` over Wi-Fi) or the microSD card.
>   MTP needs the port in *peripheral* mode, which this kernel disables.
> - Dev-iterate over SSH/Wi-Fi (`pmbootstrap sideload` over USB-net won't work).

## Audio-tuning guide (§5.3 / §6.1) — adopted vs. rejected

`0006` is the only kernel patch taken from *Kernel-Level Audio Tuning … Beryllium + Mojo 2*;
that guide targets the **downstream Android 4.19** kernel, so each item was re-checked
against this **mainline 7.1_rc1** build first:

- **§5.3 compiler flags — adopted as a safe subset** (`0006`). The guide's literal
  `CFLAGS_snd-usb-audio += …` is not valid kbuild (no such per-module target; mainline also
  has **no `urb.c`** — URB handling moved into `endpoint.c`), so we use `ccflags-y`. We keep
  `-falign-functions=32 -fno-math-errno -fno-trapping-math` and drop: `-O2` (already the
  kernel default via `CONFIG_CC_OPTIMIZE_FOR_PERFORMANCE`; we never use `-O3`),
  `-mllvm -polly=…` (Polly is a Qualcomm-CPULLVM feature, **absent** from Alpine's clang —
  passing it can fail the build), and `-mllvm -enable-misched=true` (already default on
  AArch64). These flags are compile-time, so they won't show up in `/proc/config.gz`.
- **§6.1 DMA coherency — config already done, `dma-coherent` deliberately NOT added.**
  `CONFIG_IOMMU_DMA=y` (and `DMA_CMA=y`) are already in the config. The guide's suggestion
  to mark the USB node `dma-coherent` is **unsafe on SDM845**: mainline `sdm845.dtsi`
  declares **zero** `dma-coherent` nodes (I/O-coherent SoCs like sm8250/sc7180 do), because
  SDM845-era peripheral DMA is **non-coherent** — Qualcomm peripheral I/O-coherency arrived
  on sm8150/sm8250+. Forcing `dma-coherent` makes the kernel skip cache maintenance →
  **DMA corruption** (cf. the sc7280-PCIe rule that the property must match hardware). The
  Mojo 2 is async isochronous anyway, so transport timing is already decoupled → ~zero
  upside. Left out on purpose; do not re-add.

## "Beyond ArchQ" guide — adopted vs. rejected

The second guide (*Beyond ArchQ … Beryllium + Chord Mojo 2*) is a 12-layer set modelled on
ArchQ (x86_64 / Linux-6.18 **EVL** dual-kernel) and written for downstream Android 4.19.
Audited against this **mainline 7.1** build, almost all of it was **already implemented** in
earlier phases; only one config knob was added (`VIRT_CPU_ACCOUNTING_GEN`, hygiene — see
`audiophile.kconfig`). No new source patches.

- **Already in place:** `HZ_1000` (arm64 max), full `PREEMPT`, `HIGH_RES_TIMERS`,
  `NO_HZ_FULL`+`RCU_NOCB_CPU`, `IRQ_FORCED_THREADING`, `SND_USB_AUDIO=y`+ALSA core `=y`,
  `USB_DWC3`/`USB_DWC3_QCOM=y`, ThinLTO (DWC3 **and** snd-usb-audio built-in → cross-inline),
  `nrpacks=1`, `lowlatency=1`, the cmdline isolation (`nohz_full=7 rcu_nocbs=7 isolcpus=7
  irqaffinity=0-6 threadirqs usbcore.autosuspend=-1`), the runtime IRQ→CPU7 / FIFO-90 /
  ksoftirqd / perf-governor setup, THP=`madvise`, and C-states bounded during playback by
  the player's PM-QoS `CpuLatencyGuard`.
- **Rejected (record the *why*):**
  - **EVL/Dovetail + 352.8 kHz tick** — x86/EVL only; arm64 caps at `HZ_1000`.
  - **`USB_DWC3_MSM` / `dwc3-msm.c` patch / `no_suspend_resume`** — downstream-only; mainline
    is `dwc3-qcom` (there is no `dwc3-msm.c`).
  - **`COMPACTION=n` / `MIGRATION=n`** — `DMA_CMA=y` (256 MB) depends on `MIGRATION`.
  - **`SWAP=n`** — kills the `zram` swap pmOS relies on → OOM risk under Phosh.
  - **`IPV6`/`NETFILTER`/`BPF_SYSCALL=n`** — break systemd / Phosh / **Wi-Fi** (the only
    file-transfer path, since USB is host-only).
  - **`implicit_fb=1`** — Mojo 2 is **async** (explicit feedback); wrong sync mode for it.
  - **`use_vmalloc=0`** — on non-coherent SDM845 this gives an *uncached* PCM buffer → slower
    CPU copies; mainline's vmalloc default is better (same lesson as `dma-coherent`).
  - **`idle=poll`** — never sleeps → battery/heat; PM-QoS already bounds wakeup latency.
    (`processor.max_cstate` is x86-inert on PSCI arm64.)
  - **Polly excludes / debug strips / ALSA 1.1.9** — Polly absent from our clang; debug
    prompts need `EXPERT` and are nop-patched anyway; ALSA lib is userspace (`hw:` direct).
  - **`PREEMPT_RT`** — *available* on mainline (`ARCH_SUPPORTS_RT=y`) and the closest in-tree
    analog to ArchQ's EVL, but needs `CONFIG_EXPERT` + on-device boot/stability validation;
    deliberately not enabled (stays on `PREEMPT` full).

## Prerequisites

- Local pmaports checkout at `~/.local/var/pmbootstrap/cache_git/pmaports/` (`pmbootstrap init`).
- Run `pmbootstrap pull` so the upstream kernel the `-audio` package forks matches the tag
  it pins (currently **7.1_rc1**, `_tag = sdm845-7.1-rc1-r0`). Built with `LLVM=1`.
- Panel variant chosen at `pmbootstrap init` (tianma vs ebbg).
- If you previously spliced patches into the in-tree `device/community/linux-postmarketos-qcom-sdm845/`,
  revert it so the two kernels don't compete and `pmbootstrap pull` stays clean:
  `git -C ~/.local/var/pmbootstrap/cache_git/pmaports checkout -- device/community/linux-postmarketos-qcom-sdm845/`

## De-risk first (optional, ~2 min)

Prove host mode + VBUS before the ~30-min build — on the running phone force the role at
runtime (`echo host > /sys/.../a600000.dwc3*/mode`, as the hifi-player init script does).
Note: this only validates *host mode*; the self-powered VBUS (patches 0004/0005) needs the
driver, so it can only be proven by building the kernel.

## Build & flash

```sh
# 1. Link the -audio kernel package into pmaports (idempotent; survives `pmbootstrap pull`).
sh packaging/link-kernel-into-pmaports.sh

# 2. Build the kernel (LLVM; tens of minutes).
pmbootstrap build --arch aarch64 linux-postmarketos-qcom-sdm845-audio

# 3. Bake into the image. `--add` forces our kernel in over the stock one; provides/replaces
#    then handle the swap + file ownership.
pmbootstrap build --src="$PWD" hifi-player          # picks up the cmdline/initd
pmbootstrap install --add linux-postmarketos-qcom-sdm845-audio --add hifi-player --filesystem f2fs

# 4. Flash (device in fastboot)
pmbootstrap flasher flash_kernel                    # carries the new dtb (deviceinfo_append_dtb)
pmbootstrap flasher flash_rootfs
fastboot reboot                                     # NOT the power button
```

To change a patch or the config: edit the live copy in
`../linux-postmarketos-qcom-sdm845-audio/` (that's what builds), mirror it here for the
record, then `pmbootstrap checksum linux-postmarketos-qcom-sdm845-audio` and rebuild.

## Verify on-device

```sh
# Plug the DAC with a PLAIN passive OTG cable, Mojo on its own battery, Mojo
# power-output/charging mode OFF — it should enumerate with NO external power:
lsusb | grep -i chord                               # DAC enumerates with NO manual echo host
grep -i vbus /sys/kernel/debug/regulator/regulator_summary   # pmi8998_vbus present & enabled
uname -r                                            # …-postmarketos-qcom-sdm845 (the -audio build)
cat /proc/cmdline                                   # nohz_full=7 isolcpus=7 snd_usb_audio.nrpacks=1 …
zcat /proc/config.gz | grep -E 'NO_HZ_FULL|RCU_NOCB|SND_USB_AUDIO|REGULATOR_QCOM_USB_VBUS'   # all =y
```

## Recovery

If the new kernel won't boot, re-flash the previous boot image from fastboot
(`fastboot flash boot <old>.img`), or revert to the stock kernel
(`pmbootstrap install` without `--add linux-postmarketos-qcom-sdm845-audio`) and reflash.
