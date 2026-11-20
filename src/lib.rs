//! Lock-free sequence deduper for a fixed set of threads.
//!
//! See `DESIGN.md` for the slot/reclamation protocol, the pool capacity bound
//! and the progress argument.
//!
//! **Build requirement.** `Window::Last64` and wider use a 128-bit
//! `LOCK CMPXCHG16B` that is compiled only when the target enables
//! `cmpxchg16b`. Applications must enable it in their own build, e.g.
//! `rustflags = ["-C", "target-feature=+cmpxchg16b"]` in `.cargo/config.toml`
//! (or `-C target-cpu=x86-64-v2` / `native`); otherwise `new_workers`
//! returns [`InitError::MissingTargetFeature`] for those modes.

mod atomic128;
// The scc baseline lives with the benchmarks (scc is a dev-dependency); the
// unit tests compare against it too.
#[cfg(test)]
extern crate self as seqdedupe;
#[cfg(test)]
#[path = "../benches/common/hashset.rs"]
mod hashset;
mod shift;
#[cfg(test)]
mod tests;

use atomic128::{Header, Pair};
use core::cell::UnsafeCell;
use shift::Shifter;
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Window {
    Latest,
    Last64,
    Last128,
    Last256,
    Last512,
}

impl Window {
    /// Number of retained predecessors.
    pub const fn bits(self) -> u32 {
        match self {
            Window::Latest => 0,
            Window::Last64 => 64,
            Window::Last128 => 128,
            Window::Last256 => 256,
            Window::Last512 => 512,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Registration {
    New,
    Duplicate,
    TooOld,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InitError {
    /// `expected_start > first_seen`.
    InvalidBounds {
        expected_start: u64,
        first_seen: u64,
    },
    /// The selected mode or tuning needs a CPU feature this machine lacks.
    MissingCpuFeature(&'static str),
    /// The selected mode needs a target feature the crate was not compiled
    /// with. The 128-bit modes need `cmpxchg16b`: build with
    /// `-C target-feature=+cmpxchg16b` (or `-C target-cpu=x86-64-v2`/`native`).
    MissingTargetFeature(&'static str),
    /// The slot pool (`1 + P * (P - 1 + reclaim_batch)` slots) would not fit
    /// in 32-bit slot ids / a sane amount of memory.
    TooManyParticipants { requested: usize, max: usize },
}

impl std::fmt::Display for InitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            InitError::InvalidBounds {
                expected_start,
                first_seen,
            } => write!(
                f,
                "expected_start ({expected_start}) must be <= first_seen ({first_seen})"
            ),
            InitError::MissingCpuFeature(feat) => write!(f, "CPU feature `{feat}` is required"),
            InitError::MissingTargetFeature(feat) => write!(
                f,
                "compiled without target feature `{feat}`; rebuild with `-C target-feature=+{feat}`"
            ),
            InitError::TooManyParticipants { requested, max } => {
                write!(
                    f,
                    "{requested} participants requested, at most {max} supported"
                )
            }
        }
    }
}

impl std::error::Error for InitError {}

/// How 128-bit header loads are performed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HeaderLoad {
    /// `LockCmpxchg16b` for `Last64`. For the slot modes, `Avx` when the CPU
    /// has AVX, otherwise `LockCmpxchg16b`.
    ///
    /// In `Last64` almost every load is followed by a CAS on the same line.
    /// A locked load takes the line exclusive up front, which lowers the
    /// multi-thread p99 (BENCHMARKS.md, "Header load"). The slot modes also
    /// reload the header to validate a `Duplicate`, and there a write is
    /// pure cost.
    Auto,
    /// `LOCK CMPXCHG16B` with expected == desired == 0.
    LockCmpxchg16b,
    /// Aligned `VMOVDQA` (atomic on AVX-capable Intel and AMD CPUs).
    Avx,
    /// `PREFETCHW` then `VMOVDQA`: request the line in exclusive state up
    /// front, so a following CAS does not need a second coherence round trip.
    PrefetchwAvx,
}

/// Implementation of the multi-word shift used by `Last256` / `Last512`.
/// `Last128` always uses scalar code.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShiftImpl {
    Auto,
    Scalar,
    /// AVX2 for `Last256` and `Last512`.
    Avx2,
    /// AVX-512F for `Last512`; `Last256` falls back to AVX2.
    Avx512,
    /// AVX-512 VBMI2 (+VL): the history is shifted in one zmm/ymm register
    /// with `VPERMQ` + `VPSHLDVQ`. `Last256` and `Last512`.
    Avx512Vbmi2,
}

/// How a participant makes sure the slot it reads is not recycled under it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SlotProtection {
    /// Publish a hazard entry, then revalidate the header (DESIGN.md,
    /// "Hazards"). Readers never see a slot being rewritten.
    Hazard,
    /// No hazards. Read the slot, then check the header did not change: by
    /// the reload for a `Duplicate` answer or a snapshot, by the CAS itself
    /// for an update. Sound because the header is ABA-free: an unchanged
    /// header proves the slot stayed current, and current slots are
    /// immutable. Displaced slots are recycled without any scan.
    Optimistic,
}

/// Knobs for the benchmark experiments. `Tuning::default()` is what
/// `new_workers` uses.
#[derive(Clone, Copy, Debug)]
pub struct Tuning {
    pub header_load: HeaderLoad,
    pub shift: ShiftImpl,
    /// Special-case `seq == high + 1` in the slot modes (`Last64` is
    /// branch-free and ignores it).
    pub delta1_fast_path: bool,
    /// Slots each participant reclaims per hazard scan, at least. Each
    /// participant owns `participants - 1 + reclaim_batch` slots.
    pub reclaim_batch: NonZeroUsize,
    /// Slot modes: publish the hazard for the new slot (plain store) before
    /// the CAS that installs it, so the next operation by the same writer can
    /// skip protect-and-revalidate when nobody else wrote in between.
    /// (`SlotProtection::Hazard` only.)
    pub pre_protect: bool,
    pub protection: SlotProtection,
    /// Re-read the header right before the CAS and skip the CAS if it
    /// changed (test-and-test-and-set). Under contention the losers then
    /// re-decide from a plain load instead of a failed locked CAS.
    pub recheck_before_cas: bool,
}

impl Default for Tuning {
    fn default() -> Self {
        Tuning {
            header_load: HeaderLoad::Auto,
            shift: ShiftImpl::Auto,
            delta1_fast_path: true,
            reclaim_batch: NonZeroUsize::new(8).unwrap(),
            pre_protect: true,
            protection: SlotProtection::Optimistic,
            recheck_before_cas: false,
        }
    }
}

/// Per-worker counters (writer-local, never shared).
#[derive(Clone, Copy, Debug, Default)]
pub struct WorkerStats {
    /// Header CAS attempts that failed because another participant won.
    pub cas_retries: u64,
    /// Hazard publications whose revalidation saw a changed header.
    pub revalidation_retries: u64,
    /// Hazard scans performed to refill the free list.
    pub reclaim_scans: u64,
    /// CAS attempts skipped because the pre-CAS recheck saw a new header.
    pub cas_skipped: u64,
}

/// Watermark and history from one logical instant. Fully owned.
#[derive(Clone, Debug)]
pub struct Snapshot {
    high: u64,
    expected_start: u64,
    /// 0 when history is unavailable (`Latest`).
    nbits: u32,
    bits: [u64; 8],
}

impl Snapshot {
    pub fn high_watermark(&self) -> u64 {
        self.high
    }

    /// Unseen predecessors at or above `expected_start` inside the retained
    /// window, in ascending order. `None` means history is unavailable.
    pub fn missing(&self) -> Option<impl Iterator<Item = u64> + '_> {
        if self.nbits == 0 {
            return None;
        }
        let mut words = [0u64; 8];
        // Valid bit indices are 0..=imax where high - 1 - imax >= expected_start.
        if self.high > self.expected_start {
            let span = self.high - self.expected_start; // >= 1
            let nvalid = span.min(self.nbits as u64) as usize; // bits 0..nvalid
            for (w, out) in words.iter_mut().enumerate().take(self.nbits as usize / 64) {
                let lo = w * 64;
                let mask = if nvalid >= lo + 64 {
                    !0
                } else if nvalid > lo {
                    (1u64 << (nvalid - lo)) - 1
                } else {
                    0
                };
                *out = !self.bits[w] & mask;
            }
        }
        Some(Missing {
            high: self.high,
            words,
            w: self.nbits as usize / 64,
        })
    }
}

struct Missing {
    high: u64,
    words: [u64; 8],
    w: usize,
}

impl Iterator for Missing {
    type Item = u64;
    fn next(&mut self) -> Option<u64> {
        while self.w > 0 {
            let x = self.words[self.w - 1];
            if x != 0 {
                let b = 63 - x.leading_zeros() as usize;
                self.words[self.w - 1] &= !(1u64 << b);
                let i = (self.w - 1) * 64 + b;
                return Some(self.high - 1 - i as u64);
            }
            self.w -= 1;
        }
        None
    }
}

/// One participant's handle. `Send`, not `Clone`.
pub struct Worker {
    shared: Arc<Shared>,
    id: usize,
    kind: Kind,
    load: LoadKind,
    delta1: bool,
    pre_protect: bool,
    optimistic: bool,
    recheck: bool,
    expected_start: u64,
    /// Value most recently published in `hazards[id]` (`NO_HAZARD` if none).
    hazard_val: u64,
    // Slot pool. Invariant: free.len() + retired.len() + in_hand.is_some()
    // == pool capacity, for the whole life of the worker.
    free: Vec<u32>,
    retired: Vec<u32>,
    in_hand: Option<u32>,
    scratch: Vec<u64>,
    stats: WorkerStats,
}

pub fn new_workers(
    window: Window,
    participants: NonZeroUsize,
    expected_start: u64,
    first_seen: u64,
) -> Result<Vec<Worker>, InitError> {
    new_workers_with(
        window,
        participants,
        expected_start,
        first_seen,
        Tuning::default(),
    )
}

pub fn new_workers_with(
    window: Window,
    participants: NonZeroUsize,
    expected_start: u64,
    first_seen: u64,
    tuning: Tuning,
) -> Result<Vec<Worker>, InitError> {
    if expected_start > first_seen {
        return Err(InitError::InvalidBounds {
            expected_start,
            first_seen,
        });
    }
    let p = participants.get();
    let feats = Features::detect();

    if window != Window::Latest {
        // The 128-bit CAS is only compiled in when the target guarantees
        // CMPXCHG16B (see atomic128.rs).
        if !cfg!(all(target_arch = "x86_64", target_feature = "cmpxchg16b")) {
            return Err(InitError::MissingTargetFeature("cmpxchg16b"));
        }
        if !feats.cx16 {
            return Err(InitError::MissingCpuFeature("cmpxchg16b"));
        }
    }
    let load = match tuning.header_load {
        HeaderLoad::Auto if window == Window::Last64 => LoadKind::Locked,
        HeaderLoad::Auto if feats.avx => LoadKind::Avx,
        HeaderLoad::Auto | HeaderLoad::LockCmpxchg16b => LoadKind::Locked,
        HeaderLoad::Avx if feats.avx => LoadKind::Avx,
        HeaderLoad::PrefetchwAvx if feats.avx && feats.prfchw => LoadKind::PrefetchwAvx,
        HeaderLoad::Avx => return Err(InitError::MissingCpuFeature("avx")),
        HeaderLoad::PrefetchwAvx => return Err(InitError::MissingCpuFeature("avx+prfchw")),
    };
    let shift = match tuning.shift {
        // Benchmarks (BENCHMARKS.md): only Last512 gains from SIMD (+8% on
        // Zen 5c, up to +30% on Zen 2). VBMI2 and AVX2 are within noise of
        // each other; VBMI2 is preferred as it reads no guard words.
        ShiftImpl::Auto => match window {
            Window::Last512 if feats.avx512vbmi2 => ShiftKind::Vbmi2,
            Window::Last512 if feats.avx2 => ShiftKind::Avx2,
            _ => ShiftKind::Scalar,
        },
        ShiftImpl::Scalar => ShiftKind::Scalar,
        ShiftImpl::Avx2 if feats.avx2 => ShiftKind::Avx2,
        ShiftImpl::Avx2 => return Err(InitError::MissingCpuFeature("avx2")),
        ShiftImpl::Avx512 => match window {
            Window::Last512 if feats.avx512f => ShiftKind::Avx512,
            Window::Last512 => return Err(InitError::MissingCpuFeature("avx512f")),
            _ if feats.avx2 => ShiftKind::Avx2,
            _ => return Err(InitError::MissingCpuFeature("avx2")),
        },
        ShiftImpl::Avx512Vbmi2 => match window {
            Window::Last256 | Window::Last512 if feats.avx512vbmi2 => ShiftKind::Vbmi2,
            Window::Last256 | Window::Last512 => {
                return Err(InitError::MissingCpuFeature("avx512f+avx512vl+avx512vbmi2"));
            }
            _ => ShiftKind::Scalar,
        },
    };
    let optimistic = tuning.protection == SlotProtection::Optimistic;
    let kind = match window {
        Window::Latest => Kind::Watermark,
        Window::Last64 => Kind::Inline64,
        Window::Last128 => Kind::S2,
        Window::Last256 => Kind::S4(shift),
        Window::Last512 => Kind::S8(shift),
    };

    let words = window.bits() as usize / 64;
    let slotted = words >= 2;
    let batch = tuning.reclaim_batch.get();
    // Pool per participant: P - 1 others can each pin one slot, plus `batch`.
    // Without hazards nothing is ever pinned: `batch` slots, recycled in turn.
    let pool = match (slotted, optimistic) {
        (false, _) => 0,
        (true, false) => p - 1 + batch,
        (true, true) => batch,
    };
    let nslots = if slotted {
        1 + p.saturating_mul(pool)
    } else {
        0
    };
    const MAX_SLOTS: usize = 1 << 22;
    if nslots > MAX_SLOTS {
        let pool_for = |q: usize| if optimistic { batch } else { q - 1 + batch };
        let max = (1..)
            .take_while(|&q: &usize| q * pool_for(q) < MAX_SLOTS)
            .last();
        return Err(InitError::TooManyParticipants {
            requested: p,
            max: max.unwrap_or(0),
        });
    }

    let arena = Arena::new(words, nslots);
    let (hdr, wm) = match window {
        Window::Latest => (
            Pair {
                lo: first_seen,
                hi: 0,
            },
            first_seen,
        ),
        // Slot 0 is the initial all-zero bitmap; its owner is "the structure".
        _ => (
            Pair {
                lo: first_seen,
                hi: 0,
            },
            0,
        ),
    };
    let shared = Arc::new(Shared {
        header: Header::new(hdr),
        wm: CachePadded(AtomicU64::new(wm)),
        hazards: (0..p)
            .map(|_| CachePadded(AtomicU64::new(NO_HAZARD)))
            .collect(),
        arena,
    });

    Ok((0..p)
        .map(|id| {
            let first = 1 + id * pool;
            Worker {
                shared: shared.clone(),
                id,
                kind,
                load,
                delta1: tuning.delta1_fast_path,
                pre_protect: tuning.pre_protect && !optimistic,
                optimistic,
                recheck: tuning.recheck_before_cas,
                expected_start,
                hazard_val: NO_HAZARD,
                free: (first..first + pool).rev().map(|s| s as u32).collect(),
                retired: Vec::with_capacity(pool),
                in_hand: None,
                scratch: Vec::with_capacity(p),
                stats: WorkerStats::default(),
            }
        })
        .collect())
}

impl Worker {
    #[inline]
    pub fn register(&mut self, seq: u64) -> Registration {
        match self.kind {
            Kind::Watermark => self.reg_watermark(seq),
            Kind::Inline64 => self.reg_inline64(seq),
            Kind::S2 => self.reg_slot::<2, shift::Scalar>(seq),
            Kind::S4(ShiftKind::Scalar) => self.reg_slot::<4, shift::Scalar>(seq),
            Kind::S8(ShiftKind::Scalar) => self.reg_slot::<8, shift::Scalar>(seq),
            #[cfg(target_arch = "x86_64")]
            Kind::S4(ShiftKind::Vbmi2) => unsafe { self.reg_slot_vbmi2::<4>(seq) },
            #[cfg(target_arch = "x86_64")]
            Kind::S8(ShiftKind::Vbmi2) => unsafe { self.reg_slot_vbmi2::<8>(seq) },
            #[cfg(target_arch = "x86_64")]
            Kind::S4(_) => unsafe { self.reg_slot_avx2::<4>(seq) },
            #[cfg(target_arch = "x86_64")]
            Kind::S8(ShiftKind::Avx2) => unsafe { self.reg_slot_avx2::<8>(seq) },
            #[cfg(target_arch = "x86_64")]
            Kind::S8(ShiftKind::Avx512) => unsafe { self.reg_slot_avx512::<8>(seq) },
            #[cfg(not(target_arch = "x86_64"))]
            _ => unreachable!(),
        }
    }

    pub fn snapshot(&mut self) -> Snapshot {
        let mut snap = Snapshot {
            high: 0,
            expected_start: self.expected_start,
            nbits: 0,
            bits: [0; 8],
        };
        match self.kind {
            Kind::Watermark => snap.high = self.shared.wm.0.load(Ordering::Acquire),
            Kind::Inline64 => {
                let cur = self.load();
                snap.high = cur.lo;
                snap.bits[0] = cur.hi;
                snap.nbits = 64;
            }
            Kind::S2 | Kind::S4(_) | Kind::S8(_) if self.optimistic => {
                let words = self.shared.arena.words;
                let mut cur = self.load();
                loop {
                    let src = self.shared.arena.data(slot_of(cur));
                    for k in 0..words {
                        snap.bits[k] = read_word(src, k);
                    }
                    let again = self.load();
                    if again == cur {
                        break;
                    }
                    cur = again;
                    self.stats.revalidation_retries += 1;
                }
                snap.high = cur.lo;
                snap.nbits = (words * 64) as u32;
            }
            Kind::S2 | Kind::S4(_) | Kind::S8(_) => {
                let words = self.shared.arena.words;
                let cur = self.protect_current(self.load());
                // SAFETY: protect_current validated the slot under our hazard.
                unsafe {
                    let src = self.shared.arena.data(slot_of(cur));
                    core::ptr::copy_nonoverlapping(src, snap.bits.as_mut_ptr(), words);
                }
                snap.high = cur.lo;
                snap.nbits = (words * 64) as u32;
                // Unpin: a snapshot leaves no slot protected.
                self.shared.hazards[self.id]
                    .0
                    .store(NO_HAZARD, Ordering::Release);
                self.hazard_val = NO_HAZARD;
            }
        }
        snap
    }

    pub fn stats(&self) -> WorkerStats {
        self.stats
    }

    pub fn participant_id(&self) -> usize {
        self.id
    }

    // -- header access -----------------------------------------------------

    #[inline(always)]
    fn load(&self) -> Pair {
        // SAFETY: the AVX variants are only selected when AVX was detected.
        match self.load {
            LoadKind::Avx => unsafe { self.shared.header.load_avx() },
            LoadKind::PrefetchwAvx => unsafe { self.shared.header.load_prefetchw_avx() },
            LoadKind::Locked => self.shared.header.load_locked(),
        }
    }

    // -- Latest -----------------------------------------------------

    #[inline(always)]
    fn reg_watermark(&mut self, seq: u64) -> Registration {
        let wm = &self.shared.wm.0;
        let mut cur = wm.load(Ordering::Acquire);
        loop {
            if seq < cur {
                return Registration::TooOld;
            }
            if seq == cur {
                return Registration::Duplicate;
            }
            match wm.compare_exchange_weak(cur, seq, Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => return Registration::New,
                Err(v) => {
                    cur = v;
                    self.stats.cas_retries += 1;
                }
            }
        }
    }

    // -- Last64 (inline mask) ---------------------------------------------

    #[inline(always)]
    fn reg_inline64(&mut self, seq: u64) -> Registration {
        let mut cur = self.load();
        loop {
            let Pair { lo: h, hi: m } = cur;
            let new = if seq > h {
                // Branch-free: `unbounded_shl` gives 0 for shifts >= 64, so
                // d == 64 leaves only the old watermark at bit 63 and d > 64
                // clears the history. Clamp first: shift amounts are u32.
                let d = (seq - h).min(65) as u32;
                let m2 = m.unbounded_shl(d) | 1u64.unbounded_shl(d - 1);
                Pair { lo: seq, hi: m2 }
            } else if seq == h {
                return Registration::Duplicate;
            } else {
                if seq < self.expected_start {
                    return Registration::TooOld;
                }
                let i = h - 1 - seq;
                if i >= 64 {
                    return Registration::TooOld;
                }
                let bit = 1u64 << i;
                if m & bit != 0 {
                    return Registration::Duplicate;
                }
                Pair { lo: h, hi: m | bit }
            };
            if self.recheck {
                let again = self.load();
                if again != cur {
                    cur = again;
                    self.stats.cas_skipped += 1;
                    continue;
                }
            }
            match self.shared.header.cas(cur, new) {
                Ok(()) => return Registration::New,
                Err(v) => {
                    cur = v;
                    self.stats.cas_retries += 1;
                }
            }
        }
    }

    // -- Last128 / 256 / 512 (slots) ----------------------------------------

    #[cfg(target_arch = "x86_64")]
    #[target_feature(enable = "avx2")]
    unsafe fn reg_slot_avx2<const W: usize>(&mut self, seq: u64) -> Registration {
        self.reg_slot::<W, shift::Avx2>(seq)
    }

    #[cfg(target_arch = "x86_64")]
    #[target_feature(enable = "avx512f")]
    unsafe fn reg_slot_avx512<const W: usize>(&mut self, seq: u64) -> Registration {
        self.reg_slot::<W, shift::Avx512>(seq)
    }

    #[cfg(target_arch = "x86_64")]
    #[target_feature(enable = "avx2,avx512f,avx512vl,avx512vbmi2")]
    unsafe fn reg_slot_vbmi2<const W: usize>(&mut self, seq: u64) -> Registration {
        self.reg_slot::<W, shift::Vbmi2>(seq)
    }

    /// Make sure our hazard covers the slot in the header and the header was
    /// re-read after the hazard became visible. Returns a header value whose
    /// slot is safe to read until our hazard changes.
    #[inline(always)]
    fn protect_current(&mut self, mut cur: Pair) -> Pair {
        // If our hazard already holds this slot id, it was published before
        // `cur` was loaded, so the slot cannot have been recycled since it
        // became current: no need to publish and revalidate again.
        while self.hazard_val != slot_of(cur) as u64 {
            // XCHG: a full barrier, so the hazard is globally visible before
            // the header is re-read (store->load ordering).
            self.shared.hazards[self.id]
                .0
                .swap(slot_of(cur) as u64, Ordering::SeqCst);
            self.hazard_val = slot_of(cur) as u64;
            let again = self.load();
            if again != cur {
                self.stats.revalidation_retries += 1;
                cur = again;
            }
        }
        cur
    }

    #[inline(always)]
    fn reg_slot<const W: usize, S: Shifter>(&mut self, seq: u64) -> Registration {
        let nbits = (W * 64) as u64;
        let mut cur = self.load();
        loop {
            let h = cur.lo;
            // Decide what the update needs before touching the slot.
            let (new_high, late_bit, d) = if seq > h {
                (seq, None, seq - h)
            } else if seq == h {
                return Registration::Duplicate;
            } else {
                if seq < self.expected_start {
                    return Registration::TooOld;
                }
                let i = h - 1 - seq;
                if i >= nbits {
                    return Registration::TooOld;
                }
                (h, Some(i as usize), 0)
            };

            // A jump of >= N discards the whole history: no need to read it.
            let src = if self.optimistic {
                // No hazard: whatever we read is only trusted after the
                // header is seen unchanged (ABA-free, so the slot stayed
                // current, hence immutable, the whole time).
                let src = self.shared.arena.data(slot_of(cur));
                if let Some(i) = late_bit {
                    let w = read_word(src, i / 64);
                    if w & (1u64 << (i % 64)) != 0 {
                        // The answer is final only if the slot was current
                        // while we read it. Loads are not reordered with
                        // older loads (x86-TSO), and the asm load is a
                        // compiler barrier.
                        let again = self.load();
                        if again == cur {
                            return Registration::Duplicate;
                        }
                        cur = again;
                        self.stats.revalidation_retries += 1;
                        continue;
                    }
                    // Not seen: the CAS below validates the copy.
                }
                src
            } else if late_bit.is_some() || d < nbits {
                cur = self.protect_current(cur);
                if cur.lo != h {
                    continue; // header moved while protecting: re-decide
                }
                let src = self.shared.arena.data(slot_of(cur));
                if let Some(i) = late_bit {
                    // SAFETY: protected and validated.
                    let w = unsafe { *src.add(i / 64) };
                    if w & (1u64 << (i % 64)) != 0 {
                        return Registration::Duplicate;
                    }
                }
                src
            } else {
                core::ptr::null()
            };

            let dst_id = self.take_slot();
            let dst = self.shared.arena.data(dst_id);
            // SAFETY: dst is ours alone (not current, not reachable by any
            // reader that could pass validation); src is protected.
            unsafe {
                if let Some(i) = late_bit {
                    core::ptr::copy_nonoverlapping(src, dst, W);
                    *dst.add(i / 64) |= 1u64 << (i % 64);
                } else if d < nbits {
                    let d = d as usize;
                    if self.delta1 && d == 1 {
                        S::advance::<W>(src, dst, 1);
                    } else {
                        S::advance::<W>(src, dst, d);
                    }
                } else {
                    core::ptr::write_bytes(dst, 0, W);
                    if d == nbits {
                        *dst.add(W - 1) = 1u64 << 63;
                    }
                }
            }

            // Late updates since the last advance: makes every header value
            // unique (see AUX_LATE_SHIFT), so the CAS below is ABA-free.
            let late = if late_bit.is_some() {
                (cur.hi >> AUX_LATE_SHIFT) + 1
            } else {
                0
            };
            let new = Pair {
                lo: new_high,
                hi: dst_id as u64 | late << AUX_LATE_SHIFT,
            };
            if self.recheck {
                let again = self.load();
                if again != cur {
                    cur = again;
                    self.stats.cas_skipped += 1;
                    continue;
                }
            }
            if self.pre_protect {
                // Protect our own slot *before* publishing it. A plain store
                // is enough: the locked CAS below drains the store buffer, so
                // the hazard is visible before any header value naming dst.
                // If we are still the last writer next time, protect_current
                // finds hazard_val == slot and skips the XCHG + revalidation.
                // We are done reading src, so dropping its hazard is fine.
                self.shared.hazards[self.id]
                    .0
                    .store(dst_id as u64, Ordering::Release);
                self.hazard_val = dst_id as u64;
            }
            match self.shared.header.cas(cur, new) {
                Ok(()) => {
                    self.in_hand = None;
                    // We displaced cur.hi; it is now ours to recycle once no
                    // other participant's hazard names it (immediately, in
                    // optimistic mode, after the rest of the pool).
                    self.retired.push(slot_of(cur));
                    return Registration::New;
                }
                Err(v) => {
                    // Keep dst in hand for the next attempt.
                    cur = v;
                    self.stats.cas_retries += 1;
                }
            }
        }
    }

    #[inline(always)]
    fn take_slot(&mut self) -> u32 {
        if let Some(s) = self.in_hand {
            return s;
        }
        let s = match self.free.pop() {
            Some(s) => s,
            None => self.reclaim(),
        };
        self.in_hand = Some(s);
        s
    }

    #[cold]
    #[inline(never)]
    fn reclaim(&mut self) -> u32 {
        self.stats.reclaim_scans += 1;
        if self.optimistic {
            // Nothing is ever pinned. Reuse the oldest-displaced slot first,
            // so a slot other cores may still be reading is not rewritten
            // straight away.
            self.free.extend(self.retired.drain(..).rev());
            return self.free.pop().expect("slot pool empty");
        }
        self.scratch.clear();
        for (k, hz) in self.shared.hazards.iter().enumerate() {
            // Our own hazard is skipped: when allocating we never read a slot
            // that is in our own pool (the slot we are copying from is
            // current or was current, and only enters our pool when our CAS
            // displaces it).
            if k != self.id {
                let v = hz.0.load(Ordering::SeqCst);
                if v != NO_HAZARD {
                    self.scratch.push(v);
                }
            }
        }
        let scratch = &self.scratch;
        let free = &mut self.free;
        self.retired.retain(|&s| {
            if scratch.contains(&(s as u64)) {
                true
            } else {
                free.push(s);
                false
            }
        });
        // Pool bound: retired holds the whole pool (P - 1 + batch slots) and
        // at most P - 1 are pinned by other participants.
        self.free.pop().expect("slot pool bound violated")
    }
}

#[cfg(test)]
impl Worker {
    pub(crate) fn pool_size(&self) -> usize {
        self.free.len() + self.retired.len() + self.in_hand.is_some() as usize
    }

    /// Protect the current slot and keep it pinned (simulates a reader that
    /// stalls between validation and copy). Returns (header, contents).
    pub(crate) fn pin_current(&mut self) -> (u64, u32, Vec<u64>) {
        let cur = self.protect_current(self.load());
        let words = self.shared.arena.words;
        let src = self.shared.arena.data(slot_of(cur));
        let v = (0..words).map(|j| unsafe { *src.add(j) }).collect();
        (cur.lo, slot_of(cur), v)
    }

    pub(crate) fn read_slot(&self, sid: u32) -> Vec<u64> {
        let src = self.shared.arena.data(sid);
        (0..self.shared.arena.words)
            .map(|j| unsafe { src.add(j).read_volatile() })
            .collect()
    }
}

// ---------------------------------------------------------------------------
// Internals
// ---------------------------------------------------------------------------

const NO_HAZARD: u64 = u64::MAX;

/// Slot modes: header `aux` = `slot_id | late_updates << 32`, where
/// `late_updates` counts late-bit insertions since the last advance (at most
/// N). Every successful CAS either raises `high` or, at equal `high`, raises
/// `late_updates` by one, so `(high, late_updates)` is strictly increasing
/// and no header value can ever reappear. The header CAS is therefore
/// ABA-free by itself; hazards are needed only to keep slot *contents* stable
/// while they are read.
const AUX_LATE_SHIFT: u32 = 32;

#[inline(always)]
fn slot_of(p: Pair) -> u32 {
    p.hi as u32
}

/// Optimistic read of one slot word, which may race with its owner
/// rewriting it; the caller only trusts the value after validating the
/// header. A relaxed atomic load is a plain `mov` and keeps the race
/// defined on the reader's side.
#[inline(always)]
fn read_word(src: *const u64, k: usize) -> u64 {
    // SAFETY: in bounds of a live arena slot; 8-byte aligned.
    unsafe { AtomicU64::from_ptr(src.add(k) as *mut u64).load(Ordering::Relaxed) }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LoadKind {
    Locked,
    Avx,
    PrefetchwAvx,
}

#[derive(Clone, Copy, Debug)]
enum ShiftKind {
    Scalar,
    #[cfg_attr(not(target_arch = "x86_64"), allow(dead_code))]
    Avx2,
    #[cfg_attr(not(target_arch = "x86_64"), allow(dead_code))]
    Avx512,
    #[cfg_attr(not(target_arch = "x86_64"), allow(dead_code))]
    Vbmi2,
}

#[derive(Clone, Copy, Debug)]
enum Kind {
    Watermark,
    Inline64,
    S2,
    S4(ShiftKind),
    S8(ShiftKind),
}

#[repr(align(128))]
struct CachePadded<T>(T);

struct Shared {
    /// 128-bit header: `{high, mask}` or `{high, slot_id}`.
    header: Header,
    /// `Latest` state.
    wm: CachePadded<AtomicU64>,
    /// One hazard entry per participant.
    hazards: Box<[CachePadded<AtomicU64>]>,
    arena: Arena,
}

/// Preallocated bitmap slots. Slot `k` occupies `stride` words starting at
/// `k * stride`: `words` guard words (always zero) followed by `words` data
/// words. Strides are multiples of 64 bytes, so a slot never shares a cache
/// line with another slot's data.
struct Arena {
    mem: Box<[Line]>,
    words: usize,
    stride: usize,
}

#[repr(C, align(64))]
struct Line(UnsafeCell<[u64; 8]>);

// SAFETY: slot contents are only written by the participant that holds the
// slot in its private pool. With `SlotProtection::Hazard` they are only read
// under a validated hazard, so reads never overlap writes. With
// `SlotProtection::Optimistic` a stale reader may overlap its owner's write
// (seqlock-style); the value is discarded unless the header proves the slot
// stayed current, i.e. unwritten, throughout (see DESIGN.md).
unsafe impl Sync for Arena {}

impl Arena {
    fn new(words: usize, nslots: usize) -> Arena {
        let stride = (2 * words).div_ceil(8) * 8;
        let lines = nslots * stride / 8;
        Arena {
            mem: (0..lines).map(|_| Line(UnsafeCell::new([0; 8]))).collect(),
            words,
            stride,
        }
    }

    #[inline(always)]
    fn data(&self, sid: u32) -> *mut u64 {
        debug_assert!((sid as usize + 1) * self.stride <= self.mem.len() * 8);
        let base =
            UnsafeCell::raw_get(self.mem.as_ptr() as *const UnsafeCell<[u64; 8]>) as *mut u64;
        // SAFETY: in bounds by construction.
        unsafe { base.add(sid as usize * self.stride + self.words) }
    }
}

struct Features {
    cx16: bool,
    prfchw: bool,
    avx: bool,
    avx2: bool,
    avx512f: bool,
    /// avx512f + avx512vl + avx512vbmi2.
    avx512vbmi2: bool,
}

impl Features {
    #[cfg(target_arch = "x86_64")]
    fn detect() -> Features {
        Features {
            cx16: std::arch::is_x86_feature_detected!("cmpxchg16b"),
            // PREFETCHW: CPUID 0x8000_0001 ECX bit 8 (Intel and AMD).
            prfchw: {
                use std::arch::x86_64::__cpuid;
                __cpuid(0x8000_0000).eax >= 0x8000_0001 && __cpuid(0x8000_0001).ecx & (1 << 8) != 0
            },
            avx: std::arch::is_x86_feature_detected!("avx"),
            avx2: std::arch::is_x86_feature_detected!("avx2"),
            avx512f: std::arch::is_x86_feature_detected!("avx512f"),
            avx512vbmi2: std::arch::is_x86_feature_detected!("avx512f")
                && std::arch::is_x86_feature_detected!("avx512vl")
                && std::arch::is_x86_feature_detected!("avx512vbmi2"),
        }
    }
    #[cfg(not(target_arch = "x86_64"))]
    fn detect() -> Features {
        Features {
            cx16: false,
            prfchw: false,
            avx: false,
            avx2: false,
            avx512f: false,
            avx512vbmi2: false,
        }
    }
}
