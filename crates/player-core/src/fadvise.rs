//! `posix_fadvise(POSIX_FADV_DONTNEED)` wrapper for the decode read path.
//!
//! ## What this is (and isn't)
//!
//! As a track is decoded it is read **once, front to back**. Every block read
//! populates the kernel page cache, and on a small-RAM phone that cache grows
//! for data we will never touch again, pushing the kernel toward reclaim/
//! writeback churn. This wrapper tells the kernel "I'm done with the bytes I've
//! already read" via `posix_fadvise(POSIX_FADV_DONTNEED)`, so those pages are
//! dropped promptly instead of accumulating.
//!
//! This is a **memory-hygiene / reliability** tweak, scoped to the one place it
//! has a defensible mechanism — it is the in-process, sample-safe equivalent of
//! ArchQ's `pagecache-management.so -r` (ignore-reads) LD_PRELOAD shim. It is
//! **not** a "sound quality" change: the Mojo 2 is an asynchronous USB DAC that
//! reclocks from its own crystal, so host memory behaviour is never audible
//! jitter (see `CLAUDE.md` → "Does a realtime kernel help?").
//!
//! ## Why it cannot break bit-perfect
//!
//! `POSIX_FADV_DONTNEED` is a pure **cache hint**. It evicts clean pages from
//! the page cache; the next read of any offset re-faults the exact same bytes
//! from the file. It never alters file contents and never changes what `read()`
//! returns. The loopback harness (`player-cli loopback-verify`) decodes through
//! this path and proves byte-identical output.
//!
//! ## Conservative by construction
//!
//! - Only **whole pages strictly below the current read cursor** are advised,
//!   and only once the un-advised prefix crosses [`ADVISE_THRESHOLD`]. The page
//!   containing the cursor is never touched, so an in-flight read is never
//!   disturbed.
//! - On a **backward seek** (rewind for gapless/`.cue`/DSD), the advised
//!   watermark resets so we never advise a region we're about to re-read.
//! - Every `fadvise` failure is ignored — it is advisory. On non-Linux or any
//!   error the wrapper degrades to a plain pass-through reader.

use std::io::{Read, Seek, SeekFrom};
use std::os::unix::io::AsRawFd;

use symphonia::core::io::MediaSource;

/// Advise `DONTNEED` once at least this many un-advised bytes sit behind the
/// read cursor. 4 MiB keeps the syscall rate negligible (a handful per track)
/// while bounding resident cache for the already-consumed prefix.
const ADVISE_THRESHOLD: u64 = 4 * 1024 * 1024;

/// Page-align *down*. `posix_fadvise(DONTNEED)` only drops whole pages, so we
/// must never hand it a range whose tail page still holds bytes at/after the
/// cursor. 4096 is the SDM845 (and x86_64) page size; if the real page size
/// were larger this remains correct (we simply advise a little less).
const PAGE: u64 = 4096;

/// Wraps a seekable file reader and drops already-consumed page-cache pages.
///
/// Generic over `R` so tests can exercise the offset/threshold logic against an
/// in-memory cursor; in production `R = std::fs::File`.
pub struct FadviseReader<R> {
    inner: R,
    /// Absolute byte offset of the read cursor (mirrors the file position).
    pos: u64,
    /// Highest offset already advised away; pages below this are released.
    advised: u64,
}

impl<R: AsRawFd> FadviseReader<R> {
    /// Wrap `inner`. The cursor is assumed to start at offset 0 (symphonia opens
    /// at the start); a `Seek` before the first read corrects it regardless.
    pub fn new(inner: R) -> Self {
        Self { inner, pos: 0, advised: 0 }
    }

    /// Drop clean pages in `[advised, page_floor(pos))` if the un-advised prefix
    /// has grown past the threshold. Page-floored so the cursor's page is kept.
    fn maybe_advise(&mut self) {
        let floor = self.pos & !(PAGE - 1);
        if floor <= self.advised || floor - self.advised < ADVISE_THRESHOLD {
            return;
        }
        let off = self.advised as libc::off_t;
        let len = (floor - self.advised) as libc::off_t;
        // Advisory only: ignore the return value. POSIX_FADV_DONTNEED on a clean
        // range simply drops those cache pages; a later read re-faults them.
        unsafe {
            libc::posix_fadvise(self.inner.as_raw_fd(), off, len, libc::POSIX_FADV_DONTNEED);
        }
        self.advised = floor;
    }
}

impl<R: Read + AsRawFd> Read for FadviseReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.pos += n as u64;
        self.maybe_advise();
        Ok(n)
    }
}

impl<R: Seek + AsRawFd> Seek for FadviseReader<R> {
    fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
        let new = self.inner.seek(pos)?;
        self.pos = new;
        // Rewound behind the watermark: future reads will re-touch this region,
        // so reset so we don't advise pages we're about to read again.
        if new < self.advised {
            self.advised = new & !(PAGE - 1);
        }
        Ok(new)
    }
}

// Symphonia's `MediaSource` requires `Read + Seek + Send + Sync` plus liveness
// and byte-length. We forward both to the inner file so probing/format logic is
// unchanged. `File` is always seekable and has a known length.
impl<R: Read + Seek + AsRawFd + Send + Sync> MediaSource for FadviseReader<R> {
    fn is_seekable(&self) -> bool {
        true
    }
    fn byte_len(&self) -> Option<u64> {
        // Query size via fstat on the raw fd; None if it fails (treated as a
        // non-seekable-length stream, which only disables a length optimisation).
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        let rc = unsafe { libc::fstat(self.inner.as_raw_fd(), &mut st) };
        if rc == 0 && st.st_size >= 0 {
            Some(st.st_size as u64)
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// A file-backed reader so we exercise the *real* fd path (fadvise + fstat),
    /// not a mock — proving reads stay byte-identical through the wrapper.
    fn temp_file_with(bytes: &[u8]) -> std::fs::File {
        let dir = std::env::temp_dir().join(format!("pc-fadv-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("f-{:p}", bytes.as_ptr()));
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(bytes).unwrap();
        f.sync_all().unwrap();
        // Reopen read-only at offset 0.
        std::fs::File::open(&path).unwrap()
    }

    #[test]
    fn reads_are_byte_identical_through_wrapper() {
        // Larger than the threshold so advise actually fires mid-stream.
        let data: Vec<u8> = (0..(ADVISE_THRESHOLD as usize * 2 + 1234))
            .map(|i| (i % 251) as u8)
            .collect();
        let mut r = FadviseReader::new(temp_file_with(&data));
        let mut got = Vec::new();
        let mut buf = [0u8; 7919]; // odd, prime-ish chunk to cross page/threshold edges
        loop {
            let n = r.read(&mut buf).unwrap();
            if n == 0 {
                break;
            }
            got.extend_from_slice(&buf[..n]);
        }
        assert_eq!(got, data, "wrapper must not alter a single byte");
        // It must have advised at least once on a 2×threshold stream.
        assert!(r.advised >= ADVISE_THRESHOLD, "expected mid-stream advise");
        // Watermark never runs ahead of the cursor, and is page-aligned.
        assert!(r.advised <= r.pos);
        assert_eq!(r.advised % PAGE, 0);
    }

    #[test]
    fn byte_len_reports_true_size() {
        let data = vec![0u8; 10_000];
        let r = FadviseReader::new(temp_file_with(&data));
        assert_eq!(r.byte_len(), Some(10_000));
    }

    #[test]
    fn backward_seek_resets_watermark() {
        let data = vec![1u8; ADVISE_THRESHOLD as usize * 2];
        let mut r = FadviseReader::new(temp_file_with(&data));
        let mut buf = vec![0u8; ADVISE_THRESHOLD as usize + PAGE as usize];
        r.read(&mut buf).unwrap();
        let advanced = r.advised;
        assert!(advanced > 0, "advised after a large read");
        // Rewind to the very start: the watermark must drop so we don't advise
        // pages we're about to re-read.
        r.seek(SeekFrom::Start(0)).unwrap();
        assert_eq!(r.advised, 0, "backward seek resets the advise watermark");
        // Forward seek past the watermark must NOT reset it.
        r.read(&mut buf).unwrap();
        let a2 = r.advised;
        r.seek(SeekFrom::Start(a2 + PAGE)).unwrap();
        assert_eq!(r.advised, a2, "forward seek keeps the watermark");
    }
}
