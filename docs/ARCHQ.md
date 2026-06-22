# ArchQ — what it is, and what (if anything) it teaches a bit-perfect ARM DAP

A rigorous, non-marketing comparison between **ArchQ** (`sam0402/ArchQ`) and this
player, written after reading both codebases in full. The goal: separate the
**measurable engineering wins** from the **placebo-on-an-async-USB-path** folklore,
and record honestly which ideas were already implemented here, which one gap we
closed, and which we deliberately rejected.

> TL;DR — Almost everything in ArchQ that has a defensible mechanism was **already
> implemented in this repo, and more rigorously** (see `packaging/aports/hifi-player/`
> and `CLAUDE.md`). We closed the one real gap (volatile dirs on tmpfs) and added an
> in-process page-cache drop (`fadvise`). We rejected the audiophile kernel-tick and
> "alsa sound-signature" ideas because they cannot affect an asynchronous USB DAC's
> analog output, which is the entire premise of this player.

## 1. What ArchQ actually is

ArchQ is a **headless x86_64 Arch Linux distribution** for audiophile music *servers/
players*. Every kernel package it ships is `x86_64` (Intel/AMD). It is **not** an ARM
or phone project, and it has **no decode engine of its own**: it is a
**system-tuning + service-orchestration** distro that wires together stock players —
MPD (with CD playback), LMS/Squeezelite, shairport-sync (AirPlay), Roon Bridge,
HQPlayer NAA, OwnTone. Its "secret sauce" is entirely in the OS layer:

- a patched **EVL/Dovetail real-time kernel** built at exotic tick rates named by
  frequency (`Q44`/`Q220`/`Q352`/`Q396`/`Q441` ≈ 44.1/352.8/396.9/441 kHz tick);
- aggressive **kernel cmdline** (`isolcpus`, `rcu_nocbs`, `idle=poll`, `nohz=off`,
  `clocksource=tsc`, `hpet=disable`, `nosmt`, …);
- **core isolation** + per-service core pinning written into the GRUB cmdline;
- CPU **frequency locking** (`cpupower -f`, SpeedStep off);
- an LD_PRELOAD **page-cache limiter** (`pagecache-management.so`, with an
  `ignore-reads` mode) wrapped around every player's `ExecStart=`;
- **ramroot** (whole OS in RAM), **F2FS**, `/var/log`+`/tmp` on **tmpfs**, `noatime`;
- multiple prebuilt **alsa-lib** binaries (Halo/Soft/Analytical/Dynamic — same source,
  different GCC flags) sold as selectable "sound signatures";
- service "modes" = (set of active services + a chosen kernel) you switch between.

This player shares none of ArchQ's code and **none of its scope** (we are one bit-
perfect app for one async USB DAC, not a multi-protocol server distro). What's worth
mining is a handful of *system-tuning ideas* — judged below against our actual target:
**Poco F1 / SDM845 / postmarketOS feeding a Chord Mojo 2 over async USB.**

## 2. The decisive physical fact: the Mojo 2 is an asynchronous USB DAC

The Mojo 2 is **clock master**. It reclocks playback from its own crystal; the host
adapts to the DAC via the USB feedback endpoint. Therefore **host-side timing jitter
does not reach the DAC's analog output** — it is absorbed by the DAC's buffer. Host
scheduling only changes *how often we underrun* (xrun), i.e. **reliability**, never
"digital sound quality."

This is not our opinion alone — it is already the repo's documented position
(`CLAUDE.md` → *"Does a realtime kernel help?"*): a PREEMPT_RT kernel "cannot touch the
burst hazard or the DAC's clock… host scheduling only affects USB packet delivery
timing, i.e. how often we underrun." A web+scholar deep-research pass (Linux/JACK/
PREEMPT_RT literature, async UAC2 behaviour) reached the same conclusion independently.

Consequence: **every ArchQ tweak whose stated mechanism is "lower digital jitter /
phase noise to the DAC" is placebo on this path.** The only tweaks that matter are the
ones that reduce **xruns** (each xrun is a guarded-but-real PCM re-init — and re-inits
are the full-scale-burst hazard, see `CLAUDE.md` "Output safety") and **scheduling
non-determinism**.

## 3. ArchQ tweak → mechanism → verdict → status here

| ArchQ tweak | Real mechanism | Verdict on async-USB bit-perfect ARM | Status in this repo |
|---|---|---|---|
| Core isolation `isolcpus` + `nohz_full` + `rcu_nocbs` on the audio core | Stops the scheduler/RCU callbacks from preempting the RT audio thread | **Measurable win** (fewer xruns / lower jitter) | **Already done** — `packaging/aports/hifi-player/90-audio-cmdline.conf`: `isolcpus=7 nohz_full=7 rcu_nocbs=7 irqaffinity=0-6` |
| IRQ affinity off the audio core + `threadirqs` + RT-boost the USB (DWC3/xHCI) IRQ threads | Confines interrupts to housekeeping cores; makes USB IRQ delivery deterministic | **Measurable win** (the single most useful one for USB audio) | **Already done** — `hifi-player-audio-setup` (labelled "ArchQ pattern"): moves IRQs to CPU 0-6, RT-prio the xHCI/DWC3 threads + ksoftirqd boost |
| CPU frequency / governor lock, disable DVFS | Removes DVFS transition latency from the RT path | **Niche win**, but on a phone DVFS-off raises heat→throttle→*more* jitter; use bounded, not fixed | **Already done, correctly bounded** — `performance` governor on CPUs 4-7 only, not a hard fixed freq |
| Disable deep CPU idle C-states | Cuts wake-from-idle latency for USB IRQ servicing | **Measurable win** | **Already done** — `processor.max_cstate=1` (cmdline) + a `/dev/cpu_dma_latency` PM-QoS guard held by the audio group (`99-cpu-dma-latency.rules`) |
| RT scheduling + `mlockall` + rtprio/memlock limits | SCHED_FIFO audio thread, no paging of audio memory | **Measurable win** | **Already done** — `rt.rs` (SCHED_FIFO, affinity, rlimit) + `99-audio.conf` (rtprio 95, memlock unlimited) |
| VM/writeback sysctls (swappiness, dirty ratios, compaction off, RT runtime, timer migration, nmi_watchdog off) | Reduces background kernel work that can stall the audio path | **Measurable win** | **Already done** — `99-audio-sysctl.conf` (`vm.compaction_proactiveness=0`, `kernel.sched_rt_runtime_us=-1`, `kernel.timer_migration=0`, `kernel.nmi_watchdog=0`, dirty/ swappiness tuned) |
| USB power management off (`power_save=0`, `autosuspend=-1`, `lowlatency=1`) | Stops the DAC's USB link from autosuspending mid-stream | **Measurable win** (also a burst-hazard backstop) | **Already done** — cmdline + `CLAUDE.md` "System backstop" |
| `CONFIG_HZ=1000`, THP=madvise, ZRAM | Higher timer resolution; controlled memory pressure | **Minor win** | **Already done** — kernel config + cmdline |
| LD_PRELOAD page-cache limiter, `ignore-reads` (drop read pages) | Stop the played file's pages accumulating in cache → less reclaim/writeback pressure | **Niche win** (memory hygiene, small-RAM phone) — *not* a jitter cure | **Added in this branch** as the in-process, sample-safe equivalent: `crates/player-core/src/fadvise.rs` calls `posix_fadvise(DONTNEED)` on the already-decoded prefix. No LD_PRELOAD needed. |
| `/var/log`, `/tmp`, `/var/tmp` on **tmpfs**, `noatime` | Routine log/temp writeback never hits flash → removes a real (if small) source of block-layer stalls that can delay a USB URB | **Niche win** (the one genuine gap here) | **Added in this branch** — `var-log.mount` (+ docs note for `/tmp` & `noatime`); wired into the APKBUILD |
| **EVL/Dovetail kernel at "audio-aligned" 352.8/396.9/441 kHz tick** to reduce "phase noise" | Claims aligning scheduler ticks to audio rates lowers jitter audible at the DAC | **Placebo on async USB** — the DAC reclocks from its own crystal; host tick cadence never reaches the analog output. Also: a months-long Dovetail port to SDM845 for an unmeasurable effect. | **Rejected** (documented, not implemented) |
| Multiple **alsa-lib "sound signature"** builds (Halo/Soft/Analytical/Dynamic = same source, different `-O`/inline flags) | Claims binary layout changes the "sound" | **Placebo** — on a bit-perfect path the bytes to the DAC are identical regardless of compiler flags; any "difference" is unmeasured/expectation bias. Directly contradicts our non-negotiable invariant. | **Rejected** (documented, not implemented) |
| x86-only cmdline knobs: `clocksource=tsc`, `tsc=*`, `hpet=disable`, `nosmt`, `idle=poll` | TSC/HPET/SMT are x86 concepts; `idle=poll` busy-waits the CPU | **Meaningless or harmful on ARM** — SDM845 has no TSC/HPET/SMT (uses `arch_timer`); `idle=poll` would wreck battery/thermals on a phone (and thermal throttling *adds* jitter) | **Rejected** (documented, not implemented) |

## 4. What we actually changed in this branch (`archq-system-tuning`)

Two conservative, sample-safe additions — everything else above was already present:

1. **`crates/player-core/src/fadvise.rs`** — a transparent `FadviseReader<File>` wrapped
   around the decode input. As the file is streamed once front-to-back, it drops the
   already-read page-cache prefix via `posix_fadvise(POSIX_FADV_DONTNEED)` (threshold
   4 MiB, page-aligned, watermark resets on backward seek). This is **memory hygiene**,
   the in-process equivalent of ArchQ's `pagecache-management.so -r`. It is **provably
   bit-transparent**: `POSIX_FADV_DONTNEED` is a pure cache hint that never alters what
   `read()` returns; the existing `convert`/`dop`/`dsd`/`decode` tests and the
   `verify-bitperfect` decode gate pass unchanged, plus three new `fadvise` unit tests
   prove byte-identical reads and correct watermark behaviour.

2. **tmpfs for volatile dirs** — `var-log.mount` (size-capped, `nodev,nosuid,noexec`,
   degrades safely to "logs on flash" if the mount ever fails so it can never block
   boot). `/tmp` is already tmpfs under systemd; `noatime` on the rootfs is recommended
   in the post-install checklist rather than rewriting a generated fstab.

**No bit-perfect / sample-touching code was altered.** The decode→convert→pack→sink
pipeline is byte-for-byte unchanged; `fadvise` only manages cache residency of the
input file.

## 5. Honest bottom line

ArchQ is a serious, lovingly over-engineered audiophile *server distro*, but most of
its mystique (audio-aligned kernel ticks, compiler "sound signatures", `idle=poll`)
has **no mechanism that can reach an asynchronous USB DAC's analog output**. The parts
that *do* matter — core isolation, IRQ affinity, RT scheduling, bounded C-states, USB
power-management off, VM/writeback sysctls — are real engineering, and this player
**already implemented them before this analysis**, with the discipline that they reduce
**xruns** (hence re-init/burst risk), not "digital sound." This branch adds only the
two genuinely-missing, low-risk, reliability-flavoured pieces and writes the rest down
as deliberately rejected so nobody re-litigates the folklore later.
