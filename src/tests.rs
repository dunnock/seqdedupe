use crate::*;
use std::collections::BTreeSet;
use std::sync::Barrier;
use std::thread;

const ALL: [Window; 5] = [
    Window::WatermarkOnly,
    Window::Bits64,
    Window::Bits128,
    Window::Bits256,
    Window::Bits512,
];

fn nz(n: usize) -> NonZeroUsize {
    NonZeroUsize::new(n).unwrap()
}

/// Reference model: the set of every sequence ever registered as New.
struct Model {
    high: u64,
    n: u64,
    es: u64,
    seen: BTreeSet<u64>,
}

impl Model {
    fn new(w: Window, es: u64, first: u64) -> Model {
        Model {
            high: first,
            n: w.bits() as u64,
            es,
            seen: BTreeSet::from([first]),
        }
    }
    fn register(&mut self, seq: u64) -> Registration {
        use Registration::*;
        if seq == self.high {
            Duplicate
        } else if seq > self.high {
            self.high = seq;
            self.seen.insert(seq);
            New
        } else if seq < self.es || self.high - seq > self.n {
            TooOld
        } else if !self.seen.insert(seq) {
            Duplicate
        } else {
            New
        }
    }
    fn missing(&self) -> Option<Vec<u64>> {
        if self.n == 0 {
            return None;
        }
        let lo = self.high.saturating_sub(self.n).max(self.es);
        Some((lo..self.high).filter(|s| !self.seen.contains(s)).collect())
    }
}

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        // splitmix64
        self.0 = self.0.wrapping_add(0x9E3779B97F4A7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
        z ^ (z >> 31)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

/// Every tuning combination the current CPU supports.
fn tunings() -> Vec<Tuning> {
    let f = Features::detect();
    let mut shifts = vec![ShiftImpl::Scalar];
    if f.avx2 {
        shifts.push(ShiftImpl::Avx2);
    }
    if f.avx512f {
        shifts.push(ShiftImpl::Avx512);
    }
    if f.avx512vbmi2 {
        shifts.push(ShiftImpl::Avx512Vbmi2);
    }
    let mut loads = vec![HeaderLoad::LockCmpxchg16b];
    if f.avx {
        loads.push(HeaderLoad::Avx);
    }
    if f.avx && f.prfchw {
        loads.push(HeaderLoad::PrefetchwAvx);
    }
    let mut out = vec![];
    for &shift in &shifts {
        for &header_load in &loads {
            for delta1_fast_path in [false, true] {
                for batch in [1, 3] {
                    for (protection, pre_protect) in [
                        (SlotProtection::Hazard, false),
                        (SlotProtection::Hazard, true),
                        (SlotProtection::Optimistic, false),
                    ] {
                        out.push(Tuning {
                            header_load,
                            shift,
                            delta1_fast_path,
                            reclaim_batch: nz(batch),
                            pre_protect,
                            protection,
                            recheck_before_cas: batch == 3,
                        });
                    }
                }
            }
        }
    }
    out
}

/// Tunings for the concurrent tests: both protection schemes, smallest
/// pools, and the widest SIMD shift the CPU has.
fn concurrent_tunings() -> Vec<Tuning> {
    let f = Features::detect();
    let wide = if f.avx512vbmi2 {
        ShiftImpl::Avx512Vbmi2
    } else if f.avx2 {
        ShiftImpl::Avx2
    } else {
        ShiftImpl::Scalar
    };
    let mut out = vec![];
    for protection in [SlotProtection::Hazard, SlotProtection::Optimistic] {
        for (batch, shift, recheck_before_cas) in [
            (8, ShiftImpl::Scalar, false),
            (1, ShiftImpl::Scalar, false),
            (1, wide, true),
        ] {
            out.push(Tuning {
                reclaim_batch: nz(batch),
                shift,
                protection,
                recheck_before_cas,
                ..Tuning::default()
            });
        }
    }
    out
}

/// Pick the next sequence relative to the model's watermark, biased towards
/// the interesting cases.
fn pick(rng: &mut Rng, m: &Model, n: u64) -> u64 {
    let h = m.high;
    let n = n.max(1);
    let r = rng.below(100);
    let jump = |d: u64| h.saturating_add(d);
    let back = |d: u64| h.saturating_sub(d);
    match r {
        0..=24 => jump(1),
        25..=34 => jump(1 + rng.below(n)),
        35..=37 => jump(n - 1),
        38..=40 => jump(n),
        41..=43 => jump(n + 1),
        44..=45 => jump(n + 1 + rng.below(4 * n)),
        46..=55 => h,                      // watermark duplicate
        56..=80 => back(1 + rng.below(n)), // late arrival / dup in window
        81..=85 => back(n),                // oldest retained
        86..=89 => back(n + 1),            // just outside
        90..=94 => back(rng.below(3 * n)),
        _ => m.es.saturating_sub(rng.below(3)), // around expected_start
    }
}

fn check_snapshot(s: &Snapshot, m: &Model, ctx: &str) {
    assert_eq!(s.high_watermark(), m.high, "{ctx}: high");
    let got = s.missing().map(|it| it.collect::<Vec<_>>());
    assert_eq!(got, m.missing(), "{ctx}: missing");
}

/// Drive `p` workers single-threaded (round-robin at random) against the
/// model. Exercises cross-worker slot transfer, sticky hazards of idle
/// workers, and ring reuse.
fn model_run(w: Window, t: Tuning, p: usize, es: u64, first: u64, ops: usize, seed: u64) {
    let mut ws = new_workers_with(w, nz(p), es, first, t).unwrap();
    let mut m = Model::new(w, es, first);
    let mut rng = Rng(seed);
    let pool0: Vec<usize> = ws.iter().map(|w| w.pool_size()).collect();
    for i in 0..ops {
        let k = rng.below(p as u64) as usize;
        let seq = pick(&mut rng, &m, w.bits() as u64);
        let want = m.register(seq);
        let got = ws[k].register(seq);
        assert_eq!(got, want, "{w:?} {t:?} op {i} seq {seq} (high {})", m.high);
        if rng.below(16) == 0 {
            let k = rng.below(p as u64) as usize;
            check_snapshot(&ws[k].snapshot(), &m, &format!("{w:?} {t:?} op {i}"));
        }
    }
    check_snapshot(&ws[0].snapshot(), &m, "final");
    for (w, &p0) in ws.iter().zip(&pool0) {
        assert_eq!(w.pool_size(), p0, "pool size must be conserved");
    }
}

#[test]
fn model_all_modes_all_tunings() {
    for w in ALL {
        for (ti, t) in tunings().into_iter().enumerate() {
            for p in [1, 3] {
                model_run(w, t, p, 0, 0, 4000, 1000 + ti as u64 * 7 + p as u64);
                model_run(w, t, p, 1000, 1500, 4000, 2000 + ti as u64 * 7 + p as u64);
            }
        }
    }
}

#[test]
fn model_long_runs_default_tuning() {
    for w in ALL {
        for p in [1, 2, 5] {
            model_run(w, Tuning::default(), p, 0, 10, 50_000, 77 + p as u64);
        }
    }
}

#[test]
fn model_near_u64_max() {
    for w in ALL {
        for t in tunings() {
            let first = u64::MAX - 3 * 512;
            model_run(w, t, 2, first - 100, first, 3000, 5);
        }
    }
}

#[test]
fn zero_and_max_explicit() {
    use Registration::*;
    for w in ALL {
        let n = w.bits() as u64;
        let mut ws = new_workers(w, nz(1), 0, 0).unwrap();
        let wk = &mut ws[0];
        assert_eq!(wk.register(0), Duplicate, "{w:?}: first_seen is registered");
        assert_eq!(wk.register(2), New);
        assert_eq!(wk.register(1), if n == 0 { TooOld } else { New });
        assert_eq!(wk.register(1), if n == 0 { TooOld } else { Duplicate });
        assert_eq!(wk.register(0), if n == 0 { TooOld } else { Duplicate });
        let s = wk.snapshot();
        assert_eq!(s.high_watermark(), 2);
        if n > 0 {
            // Bits for sequences below zero don't exist and are not reported.
            assert_eq!(s.missing().unwrap().count(), 0);
        } else {
            assert!(s.missing().is_none());
        }
        assert_eq!(wk.register(u64::MAX), New);
        assert_eq!(wk.register(u64::MAX), Duplicate);
        assert_eq!(wk.register(u64::MAX - 1), if n == 0 { TooOld } else { New });
        let s = wk.snapshot();
        assert_eq!(s.high_watermark(), u64::MAX);
        if n > 0 {
            let miss: Vec<u64> = s.missing().unwrap().collect();
            assert_eq!(miss.len() as u64, n - 1);
            assert_eq!(miss.first(), Some(&(u64::MAX - n)));
            assert_eq!(miss.last(), Some(&(u64::MAX - 2)));
            assert_eq!(wk.register(u64::MAX - n), New);
            assert_eq!(wk.register(u64::MAX - n - 1), TooOld);
        }
    }
}

#[test]
fn jumps_n_minus_1_n_n_plus_1() {
    use Registration::*;
    for w in ALL.into_iter().skip(1) {
        let n = w.bits() as u64;
        for t in tunings() {
            for d in [n - 1, n, n + 1] {
                let base = 1000;
                let mut ws = new_workers_with(w, nz(1), 0, base, t).unwrap();
                let wk = &mut ws[0];
                // Fill the window below base completely.
                for s in base - n..base {
                    assert_eq!(wk.register(s), New);
                }
                assert_eq!(wk.register(base + d), New);
                // Old watermark sits at bit d - 1: seen iff d <= N.
                let exp = if d <= n { Duplicate } else { TooOld };
                assert_eq!(wk.register(base), exp, "{w:?} d={d} {t:?}");
                // Sequence just above the old watermark: unseen, in window.
                assert_eq!(wk.register(base + 1), if d >= 2 { New } else { Duplicate });
                // Old window: the oldest retained is base+d-n.
                let oldest = base + d - n;
                assert_eq!(oldest == base, d == n);
                assert_eq!(wk.register(oldest - 1), TooOld, "{w:?} d={d}");
                let s = wk.snapshot();
                let miss: Vec<u64> = s.missing().unwrap().collect();
                let model: Vec<u64> = (oldest..base + d)
                    .filter(|&x| !(x < base || x == base && d <= n || x == base + 1))
                    .collect();
                assert_eq!(miss, model, "{w:?} d={d}");
            }
        }
    }
}

#[test]
fn init_errors() {
    assert!(matches!(
        new_workers(Window::Bits64, nz(1), 5, 4),
        Err(InitError::InvalidBounds {
            expected_start: 5,
            first_seen: 4
        })
    ));
    let hazard = Tuning {
        protection: SlotProtection::Hazard,
        ..Tuning::default()
    };
    // Hazard pools grow with P (P - 1 + R each); optimistic pools are R.
    assert!(matches!(
        new_workers_with(Window::Bits512, nz(100_000), 0, 0, hazard),
        Err(InitError::TooManyParticipants { max: 2044, .. })
    ));
    assert!(new_workers(Window::Bits512, nz(100_000), 0, 0).is_ok());
    assert!(matches!(
        new_workers(Window::Bits512, nz(1_000_000), 0, 0),
        Err(InitError::TooManyParticipants { max: 524_287, .. })
    ));
    assert!(new_workers(Window::WatermarkOnly, nz(100_000), 0, 0).is_ok());
    if !Features::detect().avx512f {
        let t = Tuning {
            shift: ShiftImpl::Avx512,
            ..Tuning::default()
        };
        assert!(matches!(
            new_workers_with(Window::Bits512, nz(1), 0, 0, t),
            Err(InitError::MissingCpuFeature("avx512f"))
        ));
    }
}

fn assert_send<T: Send>() {}
#[test]
fn worker_is_send() {
    assert_send::<Worker>();
    assert_send::<Snapshot>();
}

// ---------------------------------------------------------------------------
// Concurrency
// ---------------------------------------------------------------------------

fn run_threads<R: Send>(ws: Vec<Worker>, f: impl Fn(usize, &mut Worker) -> R + Sync) -> Vec<R> {
    let bar = Barrier::new(ws.len());
    thread::scope(|s| {
        let hs: Vec<_> = ws
            .into_iter()
            .enumerate()
            .map(|(i, mut w)| {
                let (f, bar) = (&f, &bar);
                s.spawn(move || {
                    bar.wait();
                    f(i, &mut w)
                })
            })
            .collect();
        hs.into_iter().map(|h| h.join().unwrap()).collect()
    })
}

const STRESS: u64 = if cfg!(debug_assertions) {
    20_000
} else {
    400_000
};

/// Every thread registers the same stream: each sequence is New exactly once.
#[test]
fn duplicate_race_exactly_one_new() {
    let threads = 4;
    for w in ALL {
        for t in concurrent_tunings() {
            let ws = new_workers_with(w, nz(threads), 0, 0, t).unwrap();
            let res = run_threads(ws, |_, wk| {
                (1..STRESS)
                    .map(|s| wk.register(s) == Registration::New)
                    .collect::<Vec<_>>()
            });
            for s in 0..(STRESS - 1) as usize {
                let news = res.iter().filter(|r| r[s]).count();
                assert_eq!(news, 1, "{w:?} {t:?} seq {}", s + 1);
            }
        }
    }
}

/// Thread k registers sequences k, k+T, k+2T...: constant late arrivals.
/// No duplicates; TooOld only below the window; final window has no gaps.
#[test]
fn interleaved_late_arrivals() {
    let threads = 4u64;
    for (w, t) in ALL
        .into_iter()
        .skip(1)
        .flat_map(|w| concurrent_tunings().into_iter().map(move |t| (w, t)))
    {
        let ws = new_workers_with(w, nz(threads as usize), 0, 0, t).unwrap();
        let res = run_threads(ws, |i, wk| {
            let mut too_old = vec![];
            let mut s = i as u64 + 1;
            while s < STRESS {
                match wk.register(s) {
                    Registration::New => {}
                    Registration::Duplicate => panic!("{w:?} {t:?}: {s} duplicate"),
                    Registration::TooOld => too_old.push(s),
                }
                s += threads;
            }
            (too_old, wk.snapshot())
        });
        let fin = res.iter().map(|(_, s)| s.high_watermark()).max().unwrap();
        // Last registrations happen after all to-old ones, so check against
        // the max high seen by any thread's final snapshot.
        let n = w.bits() as u64;
        for (old, _) in &res {
            for &s in old {
                assert!(s + n < fin, "{w:?}: {s} TooOld but inside final window");
            }
        }
    }
}

/// Snapshot readers race against writers registering every sequence in
/// order. Any torn or stale-slot read shows up as a non-empty gap list or a
/// watermark going backwards.
#[test]
fn snapshots_are_consistent_under_writes() {
    for w in ALL.into_iter().skip(1) {
        for t in concurrent_tunings() {
            let ws = new_workers_with(w, nz(4), 0, 0, t).unwrap();
            run_threads(ws, |i, wk| {
                if i < 2 {
                    for s in 1..STRESS {
                        wk.register(s);
                    }
                } else {
                    let mut last = 0;
                    for _ in 0..STRESS / 4 {
                        let s = wk.snapshot();
                        assert!(s.high_watermark() >= last);
                        last = s.high_watermark();
                        let miss: Vec<u64> = s.missing().unwrap().collect();
                        assert!(miss.is_empty(), "{w:?} {t:?} high {last}: missing {miss:?}");
                    }
                }
            });
        }
    }
}

/// A participant stalls while holding a hazard on a slot. Writers keep
/// cycling their rings many times over; the pinned slot must stay intact and
/// the writers must never block.
#[test]
fn stalled_reader_blocks_only_one_slot() {
    for w in [Window::Bits128, Window::Bits256, Window::Bits512] {
        for batch in [1, 4] {
            let t = Tuning {
                reclaim_batch: nz(batch),
                protection: SlotProtection::Hazard,
                ..Tuning::default()
            };
            let mut ws = new_workers_with(w, nz(4), 0, 0, t).unwrap();
            for s in 1..=37 {
                ws[0].register(s * 3);
            }
            let mut stalled = ws.pop().unwrap();
            let (high, sid, contents) = stalled.pin_current();
            assert_eq!(high, 111);
            let pools: Vec<usize> = ws.iter().map(|w| w.pool_size()).collect();
            let res = run_threads(ws, |i, wk| {
                let mut news = 0;
                for s in 0..STRESS {
                    news += (wk.register(200 + s * 3 + i as u64) == Registration::New) as usize;
                }
                (wk.pool_size(), wk.stats(), news)
            });
            for ((pool, st, news), p0) in res.iter().zip(pools) {
                assert_eq!(*pool, p0);
                // Every New consumed a slot; a scan refills at most p0 of
                // them, so the pool cycled news / p0 times despite the stall.
                assert!(*news > 1000);
                assert!(st.reclaim_scans as usize >= news / p0 - 1, "{st:?} {news}");
            }
            assert_eq!(
                stalled.read_slot(sid),
                contents,
                "{w:?}: pinned slot was recycled"
            );
            // The stalled reader resumes and sees the latest state.
            let s = stalled.snapshot();
            assert_eq!(s.high_watermark(), 200 + (STRESS - 1) * 3 + 2);
        }
    }
}

/// Mixed random traffic with all threads; only checks invariants that hold
/// for any interleaving (at most one New per sequence).
#[test]
fn random_traffic_at_most_one_new() {
    let threads = 4;
    for (w, t) in ALL
        .into_iter()
        .flat_map(|w| concurrent_tunings().into_iter().map(move |t| (w, t)))
    {
        let n = w.bits().max(1) as u64;
        let ws = new_workers_with(w, nz(threads), 0, 0, t).unwrap();
        let res = run_threads(ws, |i, wk| {
            let mut rng = Rng(i as u64 + 99);
            let mut base = 0u64;
            let mut news = vec![];
            for _ in 0..STRESS {
                base += rng.below(3);
                let s = base.saturating_sub(rng.below(n + 4));
                if wk.register(s) == Registration::New {
                    news.push(s);
                }
            }
            news
        });
        let mut all: Vec<u64> = res.into_iter().flatten().collect();
        let len = all.len();
        all.sort_unstable();
        all.dedup();
        assert_eq!(all.len(), len, "{w:?} {t:?}: a sequence was New twice");
    }
}

#[test]
fn naive_matches_model_single_threaded() {
    for w in ALL.into_iter().skip(1) {
        let mut nv = naive::new_workers(w.bits() as u64, 1, 100, 150);
        let mut m = Model::new(w, 100, 150);
        let mut rng = Rng(3);
        for _ in 0..20_000 {
            let seq = pick(&mut rng, &m, w.bits() as u64);
            assert_eq!(nv[0].register(seq), m.register(seq), "{w:?} seq {seq}");
        }
        let s = nv[0].snapshot();
        assert_eq!(s.high, m.high);
        assert_eq!(Some(s.missing), m.missing());
    }
}

/// Exercise the raw CAS/load in many inlined contexts. Regression test for
/// two register-allocation bugs in the CMPXCHG16B asm (an operand landing in
/// RBX, which the asm swaps out).
#[test]
fn cas_register_allocation() {
    use crate::atomic128::{Header, Pair};
    let avx = Features::detect().avx;
    let h = Header::new(Pair { lo: 1, hi: 2 });
    let mut rng = Rng(9);
    let mut cur = Pair { lo: 1, hi: 2 };
    for _ in 0..100_000 {
        let new = Pair {
            lo: rng.next(),
            hi: rng.next(),
        };
        let wrong = Pair {
            lo: cur.lo ^ (1 << rng.below(64)),
            hi: cur.hi,
        };
        assert_eq!(h.cas(wrong, new), Err(cur));
        let wrong = Pair {
            lo: cur.lo,
            hi: cur.hi ^ (1 << rng.below(64)),
        };
        assert_eq!(h.cas(wrong, new), Err(cur));
        assert_eq!(h.load_locked(), cur);
        if avx {
            assert_eq!(unsafe { h.load_avx() }, cur);
            assert_eq!(unsafe { h.load_prefetchw_avx() }, cur);
        }
        assert_eq!(h.cas(cur, new), Ok(()));
        cur = new;
    }
    let z = Pair { lo: 0, hi: 0 };
    let h = Header::new(z);
    assert_eq!(h.load_locked(), z);
}

/// Deterministic version of the publish-and-revalidate race: a participant
/// reads the header, stalls *before* publishing its hazard, and in the
/// meantime the slot it saw is displaced, recycled and rewritten. Protecting
/// the stale header must notice and return the fresh one.
#[test]
fn protect_revalidates_stale_header() {
    for w in [Window::Bits128, Window::Bits256, Window::Bits512] {
        let t = Tuning {
            reclaim_batch: nz(1),
            protection: SlotProtection::Hazard,
            ..Tuning::default()
        };
        let mut ws = new_workers_with(w, nz(2), 0, 0, t).unwrap();
        let (a, b) = ws.split_at_mut(1);
        let (a, b) = (&mut a[0], &mut b[0]);
        let stale = a.load();
        for s in 1..50 {
            b.register(s * 2);
        }
        let fresh = a.load();
        assert_ne!(stale, fresh);
        let got = a.protect_current(stale);
        assert_eq!(
            got, fresh,
            "{w:?}: stale header accepted after hazard publish"
        );
        // And the slot now protected holds what a snapshot reports.
        let mut bits = a.read_slot(slot_of(got));
        bits.resize(8, 0);
        assert_eq!(bits[..], a.snapshot().bits[..]);
    }
}

/// Every shift implementation the CPU supports agrees with the scalar one
/// for every advance `1 <= d < N`, on random histories.
#[test]
fn shift_impls_agree() {
    use crate::shift::{self, Shifter};
    fn check<const W: usize, S: Shifter>(name: &str) {
        let mut rng = Rng(5);
        for _ in 0..50 {
            // W guard words, then W data words, 64-byte aligned like the arena.
            #[repr(C, align(64))]
            struct Buf([u64; 16]);
            let mut src = Buf([0; 16]);
            for k in 0..W {
                src.0[W + k] = rng.next();
            }
            for d in 1..64 * W {
                let (mut a, mut b) = (Buf([0; 16]), Buf([0; 16]));
                unsafe {
                    let s = src.0.as_ptr().add(W);
                    shift::Scalar::advance::<W>(s, a.0.as_mut_ptr(), d);
                    S::advance::<W>(s, b.0.as_mut_ptr(), d);
                }
                assert_eq!(a.0[..W], b.0[..W], "{name} W={W} d={d}");
            }
        }
    }
    let f = Features::detect();
    #[cfg(target_arch = "x86_64")]
    {
        #[target_feature(enable = "avx2")]
        unsafe fn avx2() {
            check::<4, shift::Avx2>("avx2");
            check::<8, shift::Avx2>("avx2");
        }
        #[target_feature(enable = "avx512f")]
        unsafe fn avx512() {
            check::<8, shift::Avx512>("avx512");
        }
        #[target_feature(enable = "avx2,avx512f,avx512vl,avx512vbmi2")]
        unsafe fn vbmi2() {
            check::<4, shift::Vbmi2>("vbmi2");
            check::<8, shift::Vbmi2>("vbmi2");
        }
        if f.avx2 {
            unsafe { avx2() };
        }
        if f.avx512f {
            unsafe { avx512() };
        }
        if f.avx512vbmi2 {
            unsafe { vbmi2() };
        }
    }
    let _ = f;
}
