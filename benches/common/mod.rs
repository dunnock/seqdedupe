//! Shared benchmark harness: TSC timing, core placement, RNG, the `Reg`
//! abstraction over the bitmap deduper and the scc baseline.
#![allow(dead_code)]

use seqdedupe::naive::NaiveWorker;
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

impl Reg for NaiveWorker {
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
    unsafe {
        let mut set: libc::cpu_set_t = std::mem::zeroed();
        libc::CPU_SET(cpu, &mut set);
        libc::sched_setaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &set);
    }
}
