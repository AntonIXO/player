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

Conservative, sample-safe additions — everything in §3 was already present:

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

3. **An opt-in, OFF-by-default experimental cmdline knob** (`skew_tick=1 rcu.blimit=64`),
   shipped *inert* under `/usr/share/hifi-player/cmdline-experimental.conf` (not in
   `/etc/kernel-cmdline.d/`, so it has zero effect until the user copies it there and
   rebuilds the boot image). It is an **A/B knob to measure, not an always-on tweak** —
   see §9 for the rationale and the measure-it protocol.

4. **A user-tunable output buffer** in GTK Settings (`Player::spawn_with` +
   `ui/settings.rs`): an `Output buffer` ComboRow whose presets set the ALSA buffer
   *depth* (`DEFAULT_PERIOD × periods`), persisted as the `audio_periods` meta key. The
   presets bias **upward** (robustness) from the default 8192 frames; the one smaller
   option is flagged; `periods` is clamped (`MIN_PERIODS..=MAX_PERIODS`) so it can never
   open a dropout-prone tiny buffer. See §8.

**No bit-perfect / sample-touching code was altered.** The decode→convert→pack→sink
pipeline is byte-for-byte unchanged; `fadvise` only manages cache residency of the input
file, and the buffer control changes only ALSA framing/latency (the bytes written are
identical — `loopback-verify` stays MATCH at any depth).

## 5. Honest bottom line

ArchQ is a serious, lovingly over-engineered audiophile *server distro*, but most of
its mystique (audio-aligned kernel ticks, compiler "sound signatures", `idle=poll`)
has **no mechanism that can reach an asynchronous USB DAC's analog output**. The parts
that *do* matter — core isolation, IRQ affinity, RT scheduling, bounded C-states, USB
power-management off, VM/writeback sysctls — are real engineering, and this player
**already implemented them before this analysis**, with the discipline that they reduce
**xruns** (hence re-init/burst risk), not "digital sound." This branch adds only the
two genuinely-missing, low-risk, reliability-flavoured pieces (§4.1–4.2), plus one
*opt-in, unproven* A/B knob (§4.3) and a user-facing buffer control (§4.4) — and writes
the rest down as deliberately rejected so nobody re-litigates the folklore later.

After a full token-by-token pass over ArchQ's real default cmdline (§6) and its other
config layers (§7), the conclusion is firm: **no further always-on system tweak is worth
adopting.** Every remaining ArchQ cmdline delta is either an x86-only no-op on SDM845, a
placebo on an async-USB path, or battery-harmful on a phone. The receipts follow.

## 6. ArchQ's real default cmdline — token-by-token verdict

ArchQ writes this exact line into GRUB at install time (`inst-archq`). Verdicts are
against our target (SDM845 + async-USB Mojo 2 + battery), per the §2 physics and the
xrun/burst safety model:

```
loglevel=0 nohz=off idle=poll rcu_nocb_poll rcu.blimit=0 relax_domain_level=0
skew_tick=0 nosmt noirqdebug no_timer_check clocksource=tsc tsc=reliable
tsc=noirqtime tsc=nowatchdog hpet=disable iomem=relaxed ipv6.disable=1 vsyscall=none
```

| Token | What it does | On SDM845 / our path | Verdict |
|---|---|---|---|
| `idle=poll` | CPUs busy-spin in idle (never enter C-states) | Murders phone battery + heat → thermal throttle → *more* jitter/xruns. We bound C-state **exit latency** instead, on demand, via `processor.max_cstate=1` + the `/dev/cpu_dma_latency` PM-QoS guard held during playback. | **REJECT** (battery-harmful; covered, saner) |
| `nohz=off` | Disable the tickless idle/full system-wide | We use targeted **`nohz_full=7`** — the *isolated* audio core is tickless under steady playback while the rest keep dyntick. Strictly better than a global tick-on. | **ALREADY-COVERED (better)** |
| `rcu_nocb_poll` | A kthread *polls* for offloaded RCU callbacks instead of being woken | We already offload via `rcu_nocbs=7`; the poll variant adds periodic wakeups/power for ~nil gain on an isolated core. | **REJECT** (battery; nil benefit) |
| `rcu.blimit=0` | RCU callback batch limit | Arch-agnostic and harmless, but on a `nocb` core callbacks are serviced off-core anyway, so the effect on the audio core is ~nil. Offered only as an unproven A/B knob — see §9. | **EXPERIMENTAL** |
| `skew_tick=0` | Per-CPU periodic ticks fire *synchronously* | Moot on a tickless isolated core. Note the RT-audio convention is the **opposite** (`skew_tick=1`, de-correlate ticks to avoid lock-contention bursts); harmless to try. See §9. | **EXPERIMENTAL** (we ship `=1`) |
| `relax_domain_level=0` | Limit scheduler load-balancing domains | No demonstrated xrun mechanism here; our audio thread is pinned + isolated already. | **REJECT** (no mechanism) |
| `loglevel=0` | Silence kernel console messages | No jitter mechanism; actively *hides* the diagnostics we tune by (xrun/USB errors). | **REJECT** (hurts observability) |
| `noirqdebug` | Disable the spurious-IRQ detector | Negligible; also a debug-aid we'd rather keep. | **REJECT** (no benefit) |
| `no_timer_check` | Skip the x86 timer-IRQ routing sanity check | **x86 boot concept**; nothing to check on `arch_timer`. | **x86-NO-OP** |
| `clocksource=tsc`, `tsc=reliable`, `tsc=noirqtime`, `tsc=nowatchdog` | Select/trust the x86 TSC | SDM845 has **no TSC** — it uses the ARM `arch_timer`. | **x86-NO-OP** |
| `hpet=disable` | Disable the x86 HPET | **No HPET** on SDM845. | **x86-NO-OP** |
| `nosmt` | Disable SMT/Hyper-Threading | SDM845 has **no SMT** (8 physical Kryo cores). | **x86-NO-OP** |
| `vsyscall=none` | Disable the legacy x86 vsyscall page | **x86-only** ABI; absent on ARM64. | **x86-NO-OP** |
| `iomem=relaxed` | Loosen `/dev/mem` access checks | No audio mechanism; a small hardening regression. | **REJECT** |
| `ipv6.disable=1` | Disable the IPv6 stack | Negligible xrun effect, and it would break our mDNS/SFTP "Music sync". | **REJECT** (breaks features) |

Net: of 18 tokens, **5 are pure x86 no-ops**, **8 are reject** (battery/observability/
hardening/features/no-mechanism), **2 are unproven experiments** (shipped opt-in, §9), and
**1 (`nohz=off`) we already do better**. Nothing here is a missing always-on win.

## 7. ArchQ's other config layers — audited

- **F2FS mount options** (`partimnt-cfg.sh`: `background_gc`, `discard`, `inline_*`,
  `fsync_mode=posix`, …) + `noatime` everywhere. The flash-friendly-FS + `noatime` *idea*
  is sound and we already recommend `noatime` (post-install 3b); the rootfs/FS choice is
  postmarketOS image territory, out of this app's scope. **Idea acknowledged, not ours to
  set.**
- **`pagecache-management.so`** — an LD_PRELOAD that caps page cache (`PAGECACHE_MAX_BYTES`)
  and, with `-r` (ignore-reads), drops read pages so writeback/reclaim doesn't churn during
  playback. **ALREADY-COVERED** by our in-process, sample-safe `fadvise.rs` (§4.1) — same
  effect, no preload shim.
- **Network stack** — `ipv6.disable=1` (§6), bundled Tailscale, a custom Realtek `r8127`
  driver, and shairport-sync's tiny `period_size=78 / buffer_size=468`. All **server-distro
  scope**; the tiny ALSA numbers are AirPlay-sync latency, not a robustness target for us —
  see §8.
- **Service "modes"** (`srvmode-cfg.sh` + `mboot`: a saved set of services + a chosen
  kernel you switch between) — **N/A** for a single-app player. The looser idea ("a preset
  bundles engine config + system knobs") maps onto our Settings only as a possible future
  nicety, not now.
- **alsa-lib "sound signatures"** (Halo/Soft/Analytical/Dynamic = versions 11/15/21/25, all
  alsa-lib 1.1.9, same source built with different GCC flags) and the installer's **`F`
  "Frequency"** menu (which times the scheduler tick via `/proc/interrupts | grep tick` over
  10 s). The signatures are **placebo** on a bit-perfect path — identical bytes reach the DAC
  regardless of compiler flags. But the *verification reflex* of the Frequency menu is worth
  borrowing: §9 reuses the same `/proc/interrupts` method to confirm an experimental knob
  actually changed the tick before trusting any "improvement."

## 8. ALSA period/buffer sizing — why smaller is wrong for us

The engine defaults to `DEFAULT_PERIOD = 1024` frames × `DEFAULT_PERIODS = 8`
(`engine/mod.rs`) — an 8192-frame buffer, ≈186 ms @44.1k / ≈85 ms @96k. That depth is a
**deliberate robustness choice**, and it is the *right* direction for us:

- We have **no low-latency requirement** — this is local file playback, not live monitoring
  or AirPlay sync. Latency is free to spend.
- Every **xrun is a guarded-but-real PCM re-init** = the full-scale-burst hazard (§ "Output
  safety" in `CLAUDE.md`). A deeper buffer absorbs more scheduling/USB jitter before it
  underruns, so **deeper = fewer re-inits = safer.**

ArchQ's `78/468` (~10 ms) shairport buffer is therefore **contraindicated** here — it exists
to minimise AirPlay round-trip latency, the opposite of our goal. The only sane direction to
*ever* test is **larger** (e.g. 16384 frames), and only if real xruns are observed.

This is now exposed as a user control (§4.4): an `Output buffer` ComboRow with presets
*Low latency* (4096, flagged "more dropout risk") · *Default* (8192) · *Large* (16384) ·
*Largest* (32768) — i.e. it biases **up**, never toward the dangerous tiny end. The depth is
clamped in `Player::spawn_with` (`MIN_PERIODS..=MAX_PERIODS`) so neither the UI nor a tampered
`audio_periods` meta value can violate the "never go tiny" rule. Changing it respawns the
engine (a guarded reopen, silence-led like a device change), so it momentarily stops playback;
it never alters the bytes written.

## 9. Experimental, opt-in, UNPROVEN knobs

Two ARM-valid, harmless-but-likely-nil cmdline tokens are shipped **OFF by default** for the
curious to A/B on real hardware — `skew_tick=1 rcu.blimit=64`
(`/usr/share/hifi-player/cmdline-experimental.conf`, *not* under `/etc/kernel-cmdline.d/`):

- **`skew_tick=1`** — de-correlate per-CPU tick timers (the kernel-rt/audio convention;
  ArchQ ships the opposite `=0`). On our tickless isolated core it should be moot, but it is
  the conventionally-recommended value and cannot hurt.
- **`rcu.blimit=64`** — let RCU drain more callbacks per batch. ~nil on a `nocb` core, but
  cheap to try.

Neither can affect the Mojo 2's analog output (§2: async DAC reclocks from its own crystal),
so the only thing to look for is a **measurable xrun reduction**. Enable:

```sh
sudo cp /usr/share/hifi-player/cmdline-experimental.conf \
        /etc/kernel-cmdline.d/91-audio-experimental.conf
sudo apk fix linux-postmarketos-qcom-sdm845      # rebuild the boot image
sudo reboot
```

**Measure before keeping it** (the whole point — borrowed from ArchQ's `F` menu reflex):

- Count re-inits: the player logs `[alsa] xrun #N recovered (EPIPE)` to stderr, and the
  engine tracks `Stats.xruns`. Play the same material with vs without the knob.
- Confirm the tick actually changed: `cat /proc/interrupts | grep -iE 'arch_timer|tick'`,
  diffed over ~10 s, before vs after.

If the xrun count doesn't measurably drop, **revert** (`rm
/etc/kernel-cmdline.d/91-audio-experimental.conf` + `apk fix …` + reboot). This is folklore
until a measurement on *your* hardware says otherwise.
