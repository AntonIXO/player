//! Real-time scheduling helper. Tries `SCHED_FIFO`; on a desktop without
//! rtprio limits it falls back gracefully instead of failing — bit-perfect does
//! not depend on RT, only on exact rate/format. RT lowers xrun risk on the
//! target device once a dedicated audio thread is split out (Phase 2).

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Sched {
    Fifo(i32),
    #[default]
    Fallback,
}

/// Attempt to put the current thread on `SCHED_FIFO` at `priority`.
/// Never panics; returns what was actually applied.
pub fn try_set_realtime_fifo(priority: i32) -> Sched {
    // `sched_param` carries extra SCHED_SPORADIC fields on musl (Alpine) that
    // don't exist on glibc, so a struct literal won't compile across both. Zero-
    // init and set only the field we use — portable and forward-compatible.
    let mut param: libc::sched_param = unsafe { std::mem::zeroed() };
    param.sched_priority = priority;
    // Thread 0 == the calling thread.
    let ret = unsafe { libc::sched_setscheduler(0, libc::SCHED_FIFO, &raw const param) };
    if ret == 0 {
        Sched::Fifo(priority)
    } else {
        Sched::Fallback
    }
}

/// Best-effort pin of the current thread to a single CPU (`cpu`). On the Poco
/// F1's big.LITTLE SoC, pinning the audio thread to a big core reduces wake-up
/// jitter (and DVFS migration) under `SCHED_FIFO`. Never panics; returns whether
/// the affinity was applied. No-op-safe on any core count.
pub fn pin_to_cpu(cpu: usize) -> bool {
    unsafe {
        let mut set: libc::cpu_set_t = std::mem::zeroed();
        libc::CPU_ZERO(&mut set);
        libc::CPU_SET(cpu, &mut set);
        libc::sched_setaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &raw const set) == 0
    }
}

/// Holds `/dev/cpu_dma_latency` open to bound CPU wake-up latency for the
/// duration of the guard. Writing a PM-QoS target (microseconds) and keeping the
/// fd open prevents the CPU from entering deep C-states whose exit latency
/// exceeds it — those transitions are a common source of USB-audio xruns on the
/// SDM845 (see the Poco F1 audio guide). `0` forbids any C-state transition;
/// `100` (the value the audio thread uses) wakes within 100 µs, gentler on
/// battery. Dropping the guard closes the fd and restores normal power
/// management.
///
/// Best-effort like the rest of this module: construction returns `None` when
/// the file isn't writable (e.g. a desktop dev box without the udev rule that
/// grants the `audio` group access), so callers never have to special-case it
/// and bit-perfect output is unaffected either way.
pub struct CpuLatencyGuard {
    _fd: std::fs::File,
}

impl CpuLatencyGuard {
    /// Acquire the guard at `latency_us`. Returns `None` if
    /// `/dev/cpu_dma_latency` can't be opened for writing or the write fails.
    pub fn new(latency_us: u32) -> Option<Self> {
        use std::io::Write;
        let mut fd = std::fs::OpenOptions::new()
            .write(true)
            .open("/dev/cpu_dma_latency")
            .ok()?;
        // The interface reads a single little-endian s32; the kernel keeps the
        // request alive only while this fd stays open.
        fd.write_all(&latency_us.to_le_bytes()).ok()?;
        Some(Self { _fd: fd })
    }
}

/// Best-effort RT priority ceiling from `RLIMIT_RTPRIO` (for diagnostics).
pub fn rtprio_limit() -> Option<u64> {
    let mut lim = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    let ret = unsafe { libc::getrlimit(libc::RLIMIT_RTPRIO, &raw mut lim) };
    if ret == 0 {
        Some(lim.rlim_cur)
    } else {
        None
    }
}
