//! End-to-end registration benchmark.
//!
//! cargo bench --bench register -- [--quick] [--only main,shift,delta1,load,preprotect,protection,recheck,best,batch]
//!                                 [--threads 1,2,4,8] [--ops N] [--csv PATH]
//!                                 [--targets Bits512,naive-scc/512] [--loads Seq,Dups]
//!                                 [--reps N]   (extra untimed passes, for perf record)
//!
//! Each configuration runs twice on a fresh deduper: a throughput pass (no
//! timing inside the loop) and a latency pass (RDTSC around every call, the
//! measured timer overhead is subtracted). Threads are pinned to distinct
//! physical cores (first SMT sibling of each core).

mod common;

use common::*;
use seqdedupe::naive;
use seqdedupe::*;
use std::hint::black_box;
use std::io::Write as _;
use std::num::NonZeroUsize;

use std::time::{Duration, Instant};

// ---------------------------------------------------------------------------
// Targets
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
enum Target {
    D(Window, Tuning),
    Naive(u64),
}

impl Target {
    fn nbits(&self) -> u64 {
        match self {
            Target::D(w, _) => w.bits() as u64,
            Target::Naive(n) => *n,
        }
    }
    fn label(&self) -> String {
        match self {
            Target::D(w, _) => format!("{w:?}"),
            Target::Naive(n) => format!("naive-scc/{n}"),
        }
    }
}

// ---------------------------------------------------------------------------
// Workloads
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Load {
    /// One sequential stream, dealt round-robin to the threads.
    Seq,
    /// Stream with gaps: 80% +1, 15% +1..N/4, 5% +N..2N (history reset).
    Gaps,
    /// Sequential stream shuffled within blocks of 16: many late arrivals.
    Late,
    /// Every thread sees the full stream (A/B feed arbitration). With one
    /// thread every sequence is delivered twice.
    Dups,
}

const FIRST: u64 = 1_000_000;

fn workload(load: Load, threads: usize, ops: usize, nbits: u64) -> Vec<Vec<u64>> {
    let n = nbits.max(64);
    let total = threads * ops;
    let mut rng = Rng(42);
    let global: Vec<u64> = match load {
        Load::Seq => (0..total as u64).map(|k| FIRST + 1 + k).collect(),
        Load::Gaps => {
            let mut s = FIRST;
            (0..total)
                .map(|_| {
                    let r = rng.below(100);
                    s += 1 + match r {
                        0..80 => 0,
                        80..95 => 1 + rng.below(n / 4),
                        _ => n + rng.below(n),
                    };
                    s
                })
                .collect()
        }
        Load::Late => {
            let mut v: Vec<u64> = (0..total as u64).map(|k| FIRST + 1 + k).collect();
            for block in v.chunks_mut(16) {
                for i in (1..block.len()).rev() {
                    block.swap(i, rng.below(i as u64 + 1) as usize);
                }
            }
            v
        }
        Load::Dups => {
            return if threads == 1 {
                vec![(0..ops as u64).map(|k| FIRST + 1 + k / 2).collect()]
            } else {
                let v: Vec<u64> = (0..ops as u64).map(|k| FIRST + 1 + k).collect();
                vec![v; threads]
            };
        }
    };
    (0..threads)
        .map(|t| global.iter().skip(t).step_by(threads).copied().collect())
        .collect()
}

// ---------------------------------------------------------------------------
// Timing / placement
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Runner
// ---------------------------------------------------------------------------

struct Pass {
    wall: Duration,
    counts: [u64; 3],
    retries: u64,
    lats: Vec<u32>,
}

fn run_pass<R: Reg>(workers: Vec<R>, streams: &[Vec<u64>], cores: &[usize], timed: bool) -> Pass {
    let bar = SpinBarrier::new(workers.len());
    let results: Vec<(Duration, [u64; 3], u64, Vec<u32>)> = std::thread::scope(|sc| {
        let hs: Vec<_> = workers
            .into_iter()
            .enumerate()
            .map(|(i, mut w)| {
                let (bar, stream, cpu) = (&bar, &streams[i], cores[i]);
                sc.spawn(move || {
                    pin(cpu);
                    let mut counts = [0u64; 3];
                    let mut lats = Vec::with_capacity(if timed { stream.len() } else { 0 });
                    // Spin so the core is at full clock (schedutil ramps over
                    // tens of ms), then a spinning barrier so nobody sleeps.
                    let t = Instant::now();
                    while t.elapsed() < Duration::from_millis(100) {
                        std::hint::spin_loop();
                    }
                    bar.wait();
                    let t0 = Instant::now();
                    if timed {
                        for &s in stream {
                            let a = tsc();
                            let r = w.reg(s);
                            let b = tsc();
                            lats.push((b - a).min(u32::MAX as u64) as u32);
                            counts[r as usize] += 1;
                        }
                    } else {
                        for &s in stream {
                            counts[w.reg(black_box(s)) as usize] += 1;
                        }
                    }
                    (t0.elapsed(), counts, w.retries(), lats)
                })
            })
            .collect();
        hs.into_iter().map(|h| h.join().unwrap()).collect()
    });
    let mut p = Pass {
        wall: Duration::ZERO,
        counts: [0; 3],
        retries: 0,
        lats: vec![],
    };
    for (d, c, r, l) in results {
        p.wall = p.wall.max(d);
        for k in 0..3 {
            p.counts[k] += c[k];
        }
        p.retries += r;
        p.lats.extend(l);
    }
    p
}

fn build(
    t: Target,
    threads: usize,
) -> Result<Box<dyn FnOnce(&[Vec<u64>], &[usize], bool) -> Pass>, String> {
    match t {
        Target::D(w, tuning) => {
            let ws = new_workers_with(w, NonZeroUsize::new(threads).unwrap(), 0, FIRST, tuning)
                .map_err(|e| e.to_string())?;
            Ok(Box::new(move |s, c, timed| run_pass(ws, s, c, timed)))
        }
        Target::Naive(n) => {
            let ws = naive::new_workers(n, threads, 0, FIRST);
            Ok(Box::new(move |s, c, timed| run_pass(ws, s, c, timed)))
        }
    }
}

struct Ctx {
    cores: Vec<usize>,
    /// `--targets`/`--loads` filters (substring of the label / exact name).
    targets: Option<Vec<String>>,
    loads: Option<Vec<String>>,
    /// Extra throughput-only passes (for profiling: `--reps N`).
    reps: usize,
    ops: usize,
    tpn: f64,
    overhead: u64,
    csv: std::fs::File,
}

fn measure(ctx: &mut Ctx, exp: &str, variant: &str, t: Target, load: Load, threads: usize) {
    if threads > ctx.cores.len() {
        return;
    }
    if let Some(ts) = &ctx.targets
        && !ts.iter().any(|x| t.label() == *x)
    {
        return;
    }
    if let Some(ls) = &ctx.loads
        && !ls.iter().any(|x| format!("{load:?}") == *x)
    {
        return;
    }
    for _ in 0..ctx.reps {
        let streams = workload(load, threads, ctx.ops, t.nbits());
        if let Ok(f) = build(t, threads) {
            let p = f(&streams, &ctx.cores[..threads], false);
            let total: u64 = p.counts.iter().sum();
            eprintln!(
                "  rep {:.1} Mops/s",
                total as f64 / p.wall.as_secs_f64() / 1e6
            );
        }
    }
    let streams = workload(load, threads, ctx.ops, t.nbits());
    let cores = &ctx.cores[..threads];
    let thr = match build(t, threads) {
        Ok(f) => f(&streams, cores, false),
        Err(e) => {
            println!(
                "| {exp} | {} | {variant} | {load:?} | {threads} | skipped: {e} |",
                t.label()
            );
            return;
        }
    };
    let mut lat = build(t, threads).unwrap()(&streams, cores, true);
    lat.lats.sort_unstable();
    let pct = |q: f64| {
        let c = lat.lats[((lat.lats.len() - 1) as f64 * q) as usize] as u64;
        c.saturating_sub(ctx.overhead) as f64 / ctx.tpn
    };
    let total: u64 = thr.counts.iter().sum();
    let mops = total as f64 / thr.wall.as_secs_f64() / 1e6;
    let ns_per_op = thr.wall.as_nanos() as f64 / (total as f64 / threads as f64);
    let (p50, p99, p999) = (pct(0.50), pct(0.99), pct(0.999));
    let rpo = thr.retries as f64 / total as f64;
    let mix = thr.counts.map(|c| 100.0 * c as f64 / total as f64);
    println!(
        "| {exp} | {} | {variant} | {load:?} | {threads} | {mops:.1} | {ns_per_op:.1} | {p50:.0} | {p99:.0} | {p999:.0} | {rpo:.3} | {:.0}/{:.0}/{:.0} |",
        t.label(),
        mix[0],
        mix[1],
        mix[2]
    );
    writeln!(
        ctx.csv,
        "{exp},{},{variant},{load:?},{threads},{mops:.3},{ns_per_op:.2},{p50:.1},{p99:.1},{p999:.1},{rpo:.4},{:.2},{:.2},{:.2}",
        t.label(),
        mix[0],
        mix[1],
        mix[2]
    )
    .unwrap();
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let arg = |name: &str| {
        args.iter()
            .position(|a| a == name)
            .and_then(|i| args.get(i + 1).cloned())
    };
    let quick = args.iter().any(|a| a == "--quick");
    let only: Option<Vec<String>> = arg("--only").map(|s| s.split(',').map(String::from).collect());
    let want = |e: &str| only.as_ref().is_none_or(|o| o.iter().any(|x| x == e));
    let cores = physical_cores();
    let threads: Vec<usize> = arg("--threads")
        .map(|s| s.split(',').map(|x| x.parse().unwrap()).collect())
        .unwrap_or_else(|| vec![1, 2, 4, 8]);
    let ops = arg("--ops")
        .map(|s| s.parse().unwrap())
        .unwrap_or(if quick { 200_000 } else { 2_000_000 });
    let host = std::fs::read_to_string("/proc/sys/kernel/hostname").unwrap_or_default();
    let csv_path = arg("--csv").unwrap_or_else(|| format!("target/bench-{}.csv", host.trim()));
    let mut csv = std::fs::File::create(&csv_path).unwrap();
    writeln!(csv, "experiment,target,variant,workload,threads,mops,ns_per_op,p50_ns,p99_ns,p999_ns,retries_per_op,new_pct,dup_pct,old_pct").unwrap();

    let cpu = std::fs::read_to_string("/proc/cpuinfo")
        .unwrap_or_default()
        .lines()
        .find(|l| l.starts_with("model name"))
        .map(|l| l.split(':').nth(1).unwrap().trim().to_string())
        .unwrap_or_default();
    let tpn = tsc_per_ns();
    let overhead = timer_overhead();
    println!(
        "# {cpu} — {} physical cores, TSC {tpn:.3} GHz, timer overhead {overhead} cycles (subtracted)",
        cores.len()
    );
    println!("# {ops} ops per thread; results also in {csv_path}\n");
    println!(
        "| exp | target | variant | workload | thr | Mops/s | ns/op/thr | p50 ns | p99 ns | p99.9 ns | retries/op | New/Dup/Old % |"
    );
    println!("|---|---|---|---|---|---|---|---|---|---|---|---|");

    let split = |s: String| s.split(',').map(String::from).collect::<Vec<_>>();
    let mut ctx = Ctx {
        cores,
        targets: arg("--targets").map(split),
        loads: arg("--loads").map(split),
        reps: arg("--reps").map(|s| s.parse().unwrap()).unwrap_or(0),
        ops,
        tpn,
        overhead,
        csv,
    };
    let def = Tuning::default();
    let loads = [Load::Seq, Load::Gaps, Load::Late, Load::Dups];

    if want("main") {
        let mut targets: Vec<Target> = [
            Window::WatermarkOnly,
            Window::Bits64,
            Window::Bits128,
            Window::Bits256,
            Window::Bits512,
        ]
        .map(|w| Target::D(w, def))
        .to_vec();
        targets.push(Target::Naive(64));
        targets.push(Target::Naive(512));
        for &load in &loads {
            for &t in &targets {
                for &th in &threads {
                    measure(&mut ctx, "main", "default", t, load, th);
                }
            }
        }
    }
    let th2: Vec<usize> = threads
        .iter()
        .copied()
        .filter(|&t| t == 1 || t == 4)
        .collect();
    if want("shift") {
        for w in [Window::Bits256, Window::Bits512] {
            for (name, shift) in [
                ("scalar", ShiftImpl::Scalar),
                ("avx2", ShiftImpl::Avx2),
                ("avx512", ShiftImpl::Avx512),
                ("avx512vbmi2", ShiftImpl::Avx512Vbmi2),
            ] {
                for load in [Load::Seq, Load::Gaps, Load::Late] {
                    for &th in &th2 {
                        measure(
                            &mut ctx,
                            "shift",
                            name,
                            Target::D(w, Tuning { shift, ..def }),
                            load,
                            th,
                        );
                    }
                }
            }
        }
    }
    if want("delta1") {
        for w in [Window::Bits64, Window::Bits128, Window::Bits512] {
            for (name, on) in [("delta1-off", false), ("delta1-on", true)] {
                for &th in &th2 {
                    let t = Target::D(
                        w,
                        Tuning {
                            delta1_fast_path: on,
                            ..def
                        },
                    );
                    measure(&mut ctx, "delta1", name, t, Load::Seq, th);
                }
            }
        }
    }
    if want("load") {
        for w in [Window::Bits64, Window::Bits512] {
            for (name, hl) in [
                ("lock-cmpxchg16b", HeaderLoad::LockCmpxchg16b),
                ("vmovdqa", HeaderLoad::Avx),
                ("prefetchw+vmovdqa", HeaderLoad::PrefetchwAvx),
            ] {
                for load in [Load::Seq, Load::Late, Load::Dups] {
                    for &th in &th2 {
                        let t = Target::D(
                            w,
                            Tuning {
                                header_load: hl,
                                ..def
                            },
                        );
                        measure(&mut ctx, "load", name, t, load, th);
                    }
                }
            }
        }
    }
    if want("preprotect") {
        for w in [Window::Bits128, Window::Bits512] {
            for (name, on) in [("pre_protect-off", false), ("pre_protect-on", true)] {
                for load in [Load::Seq, Load::Dups] {
                    for &th in &th2 {
                        let t = Target::D(
                            w,
                            Tuning {
                                pre_protect: on,
                                ..def
                            },
                        );
                        measure(&mut ctx, "preprotect", name, t, load, th);
                    }
                }
            }
        }
    }
    if want("protection") {
        let vbmi2 = Tuning {
            shift: ShiftImpl::Avx512Vbmi2,
            ..def
        };
        for w in [Window::Bits128, Window::Bits256, Window::Bits512] {
            for (name, t) in [
                ("hazard", def),
                (
                    "optimistic",
                    Tuning {
                        protection: SlotProtection::Optimistic,
                        ..def
                    },
                ),
                ("hazard+vbmi2", vbmi2),
                (
                    "optimistic+vbmi2",
                    Tuning {
                        protection: SlotProtection::Optimistic,
                        ..vbmi2
                    },
                ),
            ] {
                for &load in &loads {
                    for &th in &threads {
                        measure(&mut ctx, "protection", name, Target::D(w, t), load, th);
                    }
                }
            }
        }
    }
    if want("recheck") {
        let best = Tuning {
            protection: SlotProtection::Optimistic,
            shift: ShiftImpl::Avx512Vbmi2,
            ..def
        };
        for w in [Window::Bits64, Window::Bits512] {
            for (name, on) in [("recheck-off", false), ("recheck-on", true)] {
                for load in [Load::Seq, Load::Late, Load::Dups] {
                    for &th in &threads {
                        let t = Tuning {
                            recheck_before_cas: on,
                            ..best
                        };
                        measure(&mut ctx, "recheck", name, Target::D(w, t), load, th);
                    }
                }
            }
        }
    }
    if want("best") {
        // Candidate defaults: optimistic protection + VBMI2 shift, with each
        // header load.
        for w in [Window::Bits128, Window::Bits256, Window::Bits512] {
            for (name, hl) in [
                ("opt+vbmi2+vmovdqa", HeaderLoad::Avx),
                ("opt+vbmi2+prefetchw", HeaderLoad::PrefetchwAvx),
            ] {
                let t = Tuning {
                    protection: SlotProtection::Optimistic,
                    shift: ShiftImpl::Avx512Vbmi2,
                    header_load: hl,
                    ..def
                };
                for &load in &loads {
                    for &th in &threads {
                        measure(&mut ctx, "best", name, Target::D(w, t), load, th);
                    }
                }
            }
        }
    }
    if want("batch") {
        for b in [1usize, 4, 16, 64] {
            for &th in &threads {
                let t = Target::D(
                    Window::Bits512,
                    Tuning {
                        reclaim_batch: NonZeroUsize::new(b).unwrap(),
                        ..def
                    },
                );
                measure(&mut ctx, "batch", &format!("batch={b}"), t, Load::Seq, th);
            }
        }
    }
}
