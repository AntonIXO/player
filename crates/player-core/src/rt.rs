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
    let param = libc::sched_param {
        sched_priority: priority,
    };
    // Thread 0 == the calling thread.
    let ret = unsafe { libc::sched_setscheduler(0, libc::SCHED_FIFO, &param) };
    if ret == 0 {
        Sched::Fifo(priority)
    } else {
        Sched::Fallback
    }
}

/// Best-effort RT priority ceiling from `RLIMIT_RTPRIO` (for diagnostics).
pub fn rtprio_limit() -> Option<u64> {
    let mut lim = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    let ret = unsafe { libc::getrlimit(libc::RLIMIT_RTPRIO, &mut lim) };
    if ret == 0 {
        Some(lim.rlim_cur)
    } else {
        None
    }
}
