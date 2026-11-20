//! Hash-set baseline for benchmarks and tests (not part of the library): an `scc::HashSet<u64>` of seen sequence
//! numbers plus an `AtomicU64` watermark. Entries that fall `N` below the
//! watermark are rotated out by whichever thread advanced it.
//!
//! Same answers as the bitmap deduper in the absence of races. Under races it
//! is weaker: a late arrival can be reported `New` after it has fallen out of
//! the window (its insert raced with the rotation), and `snapshot()` is not a
//! single logical instant (it probes the set after reading the watermark).

use seqdedupe::Registration;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

struct Shared {
    set: scc::HashSet<u64>,
    high: AtomicU64,
    n: u64,
    expected_start: u64,
    /// Watermark at the last full sweep (removes entries leaked by the race
    /// described above).
    last_sweep: AtomicU64,
}

pub struct HashSetWorker {
    shared: Arc<Shared>,
}

pub struct HashSetSnapshot {
    pub high: u64,
    pub missing: Vec<u64>,
}

pub fn new_workers(
    window_bits: u64,
    participants: usize,
    expected_start: u64,
    first_seen: u64,
) -> Vec<HashSetWorker> {
    assert!(window_bits > 0 && expected_start <= first_seen);
    let set = scc::HashSet::with_capacity(2 * window_bits as usize);
    let _ = set.insert_sync(first_seen);
    let shared = Arc::new(Shared {
        set,
        high: AtomicU64::new(first_seen),
        n: window_bits,
        expected_start,
        last_sweep: AtomicU64::new(first_seen),
    });
    (0..participants)
        .map(|_| HashSetWorker {
            shared: shared.clone(),
        })
        .collect()
}

impl HashSetWorker {
    pub fn register(&mut self, seq: u64) -> Registration {
        let s = &*self.shared;
        if seq < s.expected_start {
            return Registration::TooOld;
        }
        let h = s.high.load(Ordering::Acquire);
        if seq < h.saturating_sub(s.n) {
            return Registration::TooOld;
        }
        if s.set.insert_sync(seq).is_err() {
            return Registration::Duplicate;
        }
        let prev = s.high.fetch_max(seq, Ordering::AcqRel);
        if seq > prev {
            self.rotate(prev, seq);
        }
        Registration::New
    }

    /// Remove everything in `[old - N, new - N)`.
    fn rotate(&self, old: u64, new: u64) {
        let s = &*self.shared;
        let lo = old.saturating_sub(s.n);
        let hi = new.saturating_sub(s.n);
        if hi - lo <= 4 * s.n {
            for k in lo..hi {
                s.set.remove_sync(&k);
            }
        }
        let last = s.last_sweep.load(Ordering::Relaxed);
        if (hi - lo > 4 * s.n || new.saturating_sub(last) > 1 << 16)
            && s.last_sweep
                .compare_exchange(last, new, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
        {
            s.set.retain_sync(|&k| k >= hi);
        }
    }

    pub fn snapshot(&mut self) -> HashSetSnapshot {
        let s = &*self.shared;
        let high = s.high.load(Ordering::Acquire);
        let lo = high.saturating_sub(s.n).max(s.expected_start);
        let missing = (lo..high).filter(|k| !s.set.contains_sync(k)).collect();
        HashSetSnapshot { high, missing }
    }
}
