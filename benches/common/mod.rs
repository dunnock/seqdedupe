//! Shared benchmark harness: TSC timing, core placement, RNG, the `Reg`
//! abstraction over the bitmap deduper and the scc baseline.
#![allow(dead_code)]

pub mod hashset;

use hashset::HashSetWorker;
use seqdedupe::*;
use std::collections::BTreeSet;
use std::hint::black_box;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

pub struct SpinBarrier {
    n: usize,
    arrived: AtomicUsize,
}
impl SpinBarrier {
    pub fn new(n: usize) -> Self {
        SpinBarrier {
            n,
            arrived: AtomicUsize::new(0),
        }
    }
    pub fn wait(&self) {
        self.arrived.fetch_add(1, Ordering::AcqRel);
        while self.arrived.load(Ordering::Acquire) < self.n {
            std::hint::spin_loop();
        }
    }
}

pub trait Reg: Send {
    fn reg(&mut self, s: u64) -> Registration;
    fn retries(&self) -> u64;
}

impl Reg for Worker {
    #[inline(always)]
    fn reg(&mut self, s: u64) -> Registration {
        self.register(s)
    }
    fn retries(&self) -> u64 {
        // Failed CAS plus CAS skipped by the pre-CAS recheck: both are lost
        // races that cost a re-decision.
        self.stats().cas_retries + self.stats().cas_skipped
    }
}

impl Reg for HashSetWorker {
    #[inline(always)]
    fn reg(&mut self, s: u64) -> Registration {
        self.register(s)
    }
    fn retries(&self) -> u64 {
        0
    }
}

pub struct Rng(pub u64);
impl Rng {
    pub fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E3779B97F4A7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
        z ^ (z >> 31)
    }
    pub fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }
}

#[inline(always)]
pub fn tsc() -> u64 {
    unsafe {
        use std::arch::x86_64::*;
        _mm_lfence();
        let t = _rdtsc();
        _mm_lfence();
        t
    }
}

pub fn tsc_per_ns() -> f64 {
    let t0 = Instant::now();
    let c0 = tsc();
    while t0.elapsed() < Duration::from_millis(200) {}
    let c1 = tsc();
    (c1 - c0) as f64 / t0.elapsed().as_nanos() as f64
}

pub fn timer_overhead() -> u64 {
    let mut v: Vec<u64> = (0..100_000)
        .map(|_| {
            let a = tsc();
            black_box(());
            tsc() - a
        })
        .collect();
    v.sort_unstable();
    v[v.len() / 2]
}

/// First logical CPU of each physical core, in core order.
pub fn physical_cores() -> Vec<usize> {
    let mut seen = BTreeSet::new();
    let mut out = vec![];
    let mut cpus: Vec<usize> = std::fs::read_dir("/sys/devices/system/cpu")
        .unwrap()
        .filter_map(|e| {
            e.ok()?
                .file_name()
                .to_str()?
                .strip_prefix("cpu")?
                .parse()
                .ok()
        })
        .collect();
    cpus.sort_unstable();
    for c in cpus {
        let rd = |f: &str| {
            std::fs::read_to_string(format!("/sys/devices/system/cpu/cpu{c}/topology/{f}"))
                .map(|s| s.trim().to_string())
                .unwrap_or_default()
        };
        if seen.insert((rd("physical_package_id"), rd("core_id"))) {
            out.push(c);
        }
    }
    out
}

pub fn pin(cpu: usize) {
    assert!(
        core_affinity::set_for_current(core_affinity::CoreId { id: cpu }),
        "cannot pin to cpu {cpu}"
    );
}

/// Latency histogram in TSC cycles, preallocated so recording never
/// allocates (3 significant digits, up to 2^32 cycles).
pub type Hist = hdrhistogram::Histogram<u64>;

pub fn hist() -> Hist {
    Hist::new_with_bounds(1, u32::MAX as u64, 3).unwrap()
}

/// Cycle-to-ns conversion with the timer overhead subtracted.
#[derive(Clone, Copy)]
pub struct Clock {
    pub tpn: f64,
    pub overhead: u64,
}

impl Clock {
    pub fn measure() -> Self {
        Clock {
            tpn: tsc_per_ns(),
            overhead: timer_overhead(),
        }
    }
    pub fn ns(&self, cycles: u64) -> f64 {
        cycles.saturating_sub(self.overhead) as f64 / self.tpn
    }
    /// Quantile `q` of `h`, in ns.
    pub fn q(&self, h: &Hist, q: f64) -> f64 {
        self.ns(h.value_at_quantile(q))
    }
    /// Mean of `h`, in ns.
    pub fn mean(&self, h: &Hist) -> f64 {
        (h.mean() - self.overhead as f64).max(0.0) / self.tpn
    }
}
