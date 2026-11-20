//! Live-feed latency benchmark.
//!
//! `S` redundant streams (default 2 and 4, one pinned thread each) deliver the same
//! sequence of message ids, in order, at a fixed average rate per stream. The
//! timing models live market or telemetry events:
//!
//! * **Synchronized clusters.** Ids are grouped into bursts (geometric size,
//!   mean `--burst-mean`). Burst starts form a Poisson process whose rate
//!   gives the requested messages per second. Every stream sees the same
//!   bursts at the same ids.
//! * **Per-stream randomness.** Each stream reaches a burst after its own
//!   exponential delay (mean `--jitter-ns`, a feed-path difference), and
//!   spaces the burst's messages by its own exponential gaps (mean
//!   `--gap-ns`). So the streams agree on *which* ids arrive together but
//!   not on exactly *when*.
//! * **Loss.** Each stream independently drops a message with probability
//!   `--loss`; another stream fills it in.
//!
//! Each thread spins until a message's scheduled time, then times the
//! `register` call alone with `RDTSC` (timer overhead subtracted) into an
//! HDR histogram. The first `--warmup-ms` of the schedule are not recorded.
//!
//! cargo bench --bench latency -- [--rates 10,50,200] [--streams 2,4] [--secs 5]
//!     [--burst-mean 8] [--gap-ns 1000] [--jitter-ns 2000] [--loss 0.01]
//!     [--targets Bits512,naive-scc/512] [--out bench-results/latency-HOST]

mod common;

use common::*;
use common::naive;
use seqdedupe::*;
use std::io::Write as _;
use std::num::NonZeroUsize;
use std::sync::atomic::{AtomicU64, Ordering};

const FIRST: u64 = 1_000_000;

#[derive(Clone, Copy)]
enum Target {
    D(Window),
    Naive(u64),
}

impl Target {
    fn label(&self) -> String {
        match self {
            Target::D(w) => format!("{w:?}"),
            Target::Naive(n) => format!("naive-scc/{n}"),
        }
    }
}

struct Params {
    rate_kps: f64,
    streams: usize,
    secs: f64,
    warmup_ms: f64,
    burst_mean: f64,
    gap_ns: f64,
    jitter_ns: f64,
    loss: f64,
    seed: u64,
}

/// One stream's schedule: (time in ns from start, id), in id order.
type Schedule = Vec<(u64, u64)>;

fn exp(rng: &mut Rng, mean: f64) -> f64 {
    // Inverse CDF; 53 random bits, never exactly 0.
    let u = ((rng.next() >> 11) as f64 + 0.5) / (1u64 << 53) as f64;
    -mean * u.ln()
}

fn schedules(p: &Params) -> Vec<Schedule> {
    let total = ((p.secs + p.warmup_ms / 1e3) * p.rate_kps * 1e3) as u64;
    let mut shared = Rng(p.seed);
    // Bursts: (start ns, first id, size). Shared by all streams.
    let mut bursts = vec![];
    let mut t = 0.0;
    let mut id = FIRST + 1;
    let burst_interval = p.burst_mean / (p.rate_kps * 1e3) * 1e9;
    while id < FIRST + 1 + total {
        // Geometric size >= 1 with the requested mean.
        let q = 1.0 / p.burst_mean;
        let u = ((shared.next() >> 11) as f64 + 0.5) / (1u64 << 53) as f64;
        let size = 1 + (u.ln() / (1.0 - q).ln()).floor() as u64;
        bursts.push((t, id, size));
        id += size;
        t += exp(&mut shared, burst_interval);
    }
    (0..p.streams)
        .map(|s| {
            let mut rng = Rng(p.seed ^ (0x5EED_0000 + s as u64 * 7919));
            let mut out = Vec::with_capacity(total as usize);
            let mut last = 0.0f64;
            for &(start, first, size) in &bursts {
                let mut at = start + exp(&mut rng, p.jitter_ns);
                for k in 0..size {
                    if k > 0 {
                        at += exp(&mut rng, p.gap_ns);
                    }
                    // A feed delivers in order: never before its previous
                    // message.
                    last = last.max(at);
                    let lost = (rng.next() >> 11) as f64 / (1u64 << 53) as f64 <= p.loss;
                    if !lost {
                        out.push((last as u64, first + k));
                    }
                }
            }
            out
        })
        .collect()
}

struct Result {
    /// Service latency of each recorded `register` call, in cycles.
    lats: Hist,
    /// Completion minus scheduled time (includes any backlog), in cycles.
    lateness: Hist,
    counts: [u64; 3],
}

fn run<R: Reg>(
    workers: Vec<R>,
    scheds: &[Schedule],
    cores: &[usize],
    tpn: f64,
    warmup_ns: u64,
) -> Vec<Result> {
    let bar = SpinBarrier::new(workers.len());
    let t0 = AtomicU64::new(0);
    std::thread::scope(|sc| {
        let hs: Vec<_> = workers
            .into_iter()
            .enumerate()
            .map(|(i, mut w)| {
                let (bar, t0, sched, cpu) = (&bar, &t0, &scheds[i], cores[i]);
                sc.spawn(move || {
                    pin(cpu);
                    let t = std::time::Instant::now();
                    while t.elapsed() < std::time::Duration::from_millis(100) {
                        std::hint::spin_loop();
                    }
                    // Thread 0 fixes the common start a little in the future.
                    if i == 0 {
                        t0.store(tsc() + (1e6 * tpn) as u64, Ordering::Release);
                    }
                    bar.wait();
                    let start = t0.load(Ordering::Acquire);
                    let mut r = Result {
                        lats: hist(),
                        lateness: hist(),
                        counts: [0; 3],
                    };
                    for &(at, id) in sched {
                        let due = start + (at as f64 * tpn) as u64;
                        while tsc() < due {
                            std::hint::spin_loop();
                        }
                        let a = tsc();
                        let reg = w.reg(id);
                        let b = tsc();
                        if at >= warmup_ns {
                            r.lats.saturating_record(b - a);
                            r.lateness.saturating_record(b - due);
                            r.counts[reg as usize] += 1;
                        }
                    }
                    r
                })
            })
            .collect();
        hs.into_iter().map(|h| h.join().unwrap()).collect()
    })
}

const QS: [f64; 12] = [
    0.5, 0.75, 0.9, 0.95, 0.99, 0.995, 0.999, 0.9995, 0.9999, 0.99995, 0.99999, 1.0,
];

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let arg = |name: &str| {
        args.iter()
            .position(|a| a == name)
            .and_then(|i| args.get(i + 1).cloned())
    };
    let num = |name: &str, d: f64| arg(name).map(|s| s.parse().unwrap()).unwrap_or(d);
    let list = |name: &str, d: &str| -> Vec<f64> {
        arg(name)
            .unwrap_or_else(|| d.to_string())
            .split(',')
            .map(|x| x.parse().unwrap())
            .collect()
    };
    let rates = list("--rates", "10,50,200");
    let stream_counts: Vec<usize> = list("--streams", "2,4").iter().map(|&s| s as usize).collect();
    let targets_filter: Option<Vec<String>> =
        arg("--targets").map(|s| s.split(',').map(String::from).collect());
    let host = std::fs::read_to_string("/proc/sys/kernel/hostname").unwrap_or_default();
    let out = arg("--out").unwrap_or_else(|| format!("target/latency-{}", host.trim()));

    let all_cores = physical_cores();
    let max_streams = *stream_counts.iter().max().unwrap();
    assert!(all_cores.len() >= max_streams, "need {max_streams} physical cores");
    let clock = Clock::measure();
    let cpu = std::fs::read_to_string("/proc/cpuinfo")
        .unwrap_or_default()
        .lines()
        .find(|l| l.starts_with("model name"))
        .map(|l| l.split(':').nth(1).unwrap().trim().to_string())
        .unwrap_or_default();

    let base = Params {
        rate_kps: 0.0,
        streams: 0,
        secs: num("--secs", 5.0),
        warmup_ms: num("--warmup-ms", 200.0),
        burst_mean: num("--burst-mean", 8.0),
        gap_ns: num("--gap-ns", 1000.0),
        jitter_ns: num("--jitter-ns", 2000.0),
        loss: num("--loss", 0.01),
        seed: num("--seed", 7.0) as u64,
    };
    println!(
        "# {cpu}; cores {:?}; TSC {:.3} GHz; timer overhead {} cycles (subtracted)",
        &all_cores[..max_streams],
        clock.tpn,
        clock.overhead
    );
    println!(
        "# {} s per run after {} ms warm-up; bursts: mean {} msgs, gap {} ns, per-stream jitter {} ns; loss {}",
        base.secs, base.warmup_ms, base.burst_mean, base.gap_ns, base.jitter_ns, base.loss
    );
    println!("# latency = one register() call, ns\n");
    println!(
        "| target | streams | kps/stream | samples | p50 | p90 | p99 | p99.9 | p99.99 | max | lateness p99 | New/Dup/Old % |"
    );
    println!("|---|---|---|---|---|---|---|---|---|---|---|---|");

    let mut summary = std::fs::File::create(format!("{out}.csv")).unwrap();
    writeln!(
        summary,
        "target,rate_kps,streams,samples,mean_ns,p50_ns,p90_ns,p99_ns,p999_ns,p9999_ns,max_ns,lateness_p50_ns,lateness_p99_ns,lateness_p999_ns,new_pct,dup_pct,old_pct"
    )
    .unwrap();
    let mut curves = std::fs::File::create(format!("{out}-curves.csv")).unwrap();
    writeln!(curves, "target,streams,rate_kps,quantile,ns").unwrap();

    let targets = [
        Target::D(Window::WatermarkOnly),
        Target::D(Window::Bits64),
        Target::D(Window::Bits128),
        Target::D(Window::Bits256),
        Target::D(Window::Bits512),
        Target::Naive(512),
    ];
    for &streams in &stream_counts {
        let cores = &all_cores[..streams];
        for &rate in &rates {
            let p = Params {
                rate_kps: rate,
                streams,
                ..base
            };
            let scheds = schedules(&p);
            let warmup_ns = (p.warmup_ms * 1e6) as u64;
            for t in targets {
                if let Some(f) = &targets_filter
                    && !f.iter().any(|x| *x == t.label())
                {
                    continue;
                }
                let res = match t {
                    Target::D(w) => {
                        let ws = new_workers(w, NonZeroUsize::new(streams).unwrap(), 0, FIRST)
                            .unwrap();
                        run(ws, &scheds, cores, clock.tpn, warmup_ns)
                    }
                    Target::Naive(n) => {
                        let ws = naive::new_workers(n, streams, 0, FIRST);
                        run(ws, &scheds, cores, clock.tpn, warmup_ns)
                    }
                };
                let (mut lats, mut late, mut counts) = (hist(), hist(), [0u64; 3]);
                for r in &res {
                    lats.add(&r.lats).unwrap();
                    late.add(&r.lateness).unwrap();
                    for k in 0..3 {
                        counts[k] += r.counts[k];
                    }
                }
                let q = |h: &Hist, x: f64| clock.q(h, x);
                let total: u64 = counts.iter().sum();
                let mix = counts.map(|c| 100.0 * c as f64 / total as f64);
                println!(
                    "| {} | {streams} | {rate} | {} | {:.0} | {:.0} | {:.0} | {:.0} | {:.0} | {:.0} | {:.0} | {:.0}/{:.0}/{:.0} |",
                    t.label(),
                    lats.len(),
                    q(&lats, 0.5),
                    q(&lats, 0.9),
                    q(&lats, 0.99),
                    q(&lats, 0.999),
                    q(&lats, 0.9999),
                    q(&lats, 1.0),
                    q(&late, 0.99),
                    mix[0],
                    mix[1],
                    mix[2],
                );
                writeln!(
                    summary,
                    "{},{rate},{streams},{},{:.1},{:.1},{:.1},{:.1},{:.1},{:.1},{:.1},{:.1},{:.1},{:.1},{:.2},{:.2},{:.2}",
                    t.label(),
                    lats.len(),
                    clock.mean(&lats),
                    q(&lats, 0.5),
                    q(&lats, 0.9),
                    q(&lats, 0.99),
                    q(&lats, 0.999),
                    q(&lats, 0.9999),
                    q(&lats, 1.0),
                    q(&late, 0.5),
                    q(&late, 0.99),
                    q(&late, 0.999),
                    mix[0],
                    mix[1],
                    mix[2],
                )
                .unwrap();
                for x in QS {
                    writeln!(curves, "{},{streams},{rate},{x},{:.1}", t.label(), q(&lats, x)).unwrap();
                }
            }
        }
    }
    eprintln!("results: {out}.csv, {out}-curves.csv");
}
