# seqdedupe

Lock-free deduplication of `u64` sequence numbers for a fixed set of threads
(x86-64, `CX16`). Every thread holds a `Worker` and calls `register(seq)`. The
answer is `New` exactly once per sequence number, then `Duplicate`. Anything
older than the retained window is `TooOld`.

```rust
use seqdedupe::{new_workers, Registration, Window};
use std::num::NonZeroUsize;

let mut workers = new_workers(Window::Bits256, NonZeroUsize::new(2).unwrap(), 0, 0)?;
let mut b = workers.pop().unwrap();
let mut a = workers.pop().unwrap();
std::thread::scope(|s| {
    s.spawn(move || assert_eq!(a.register(5), Registration::New));
    s.spawn(move || { b.register(3); });
});
```

| Mode | History kept below the watermark |
|---|---|
| `WatermarkOnly` | none (`AtomicU64`). Anything at or below the watermark is rejected. |
| `Bits64` | 64 predecessors, inline in a 128-bit header |
| `Bits128/256/512` | N predecessors in immutable preallocated slots, validated optimistically (or hazard-protected) |

`Worker::snapshot()` returns the watermark and the missing sequence numbers in
the window, both from one logical instant.

## Use cases

* **A/B feed arbitration.** Exchanges publish market data on redundant
  multicast feeds. Each feed handler thread registers every packet's
  sequence number; the first copy (`New`) is processed and the other
  (`Duplicate`) is dropped. Whichever feed is faster for each packet wins,
  with no lock and no single arbiter thread.
* **Multi-path or replicated ingest.** Telemetry, IoT or trading events
  replicated over several links, regions or UDP paths, where every receiver
  thread sees most messages more than once.
* **At-least-once consumers.** Redelivery after a timeout or failover (a
  Kafka or NATS consumer, a retrying RPC client) with a monotonic per-producer
  sequence. Exactly-once processing needs bounded memory and no per-message
  allocation.
* **Gap detection and recovery.** `snapshot().missing()` lists what no feed
  has delivered yet, which is the input for a retransmission request or a
  snapshot recovery.
* **Fan-in of replicated logs.** Apply each log index exactly once when
  several replicas push the same entries.
* **`WatermarkOnly` for latest-value-wins streams.** Quotes or state updates
  where anything older than the newest message is stale anyway. It is the
  cheapest mode, but it cannot tell a duplicate from a late fill-in: both are
  `TooOld`.

**Choosing a window.** N must cover the worst reordering between feeds,
measured in messages: the maximum feed skew × the message rate. For example,
feeds up to 2 ms apart at 200k msg/s are 400 messages apart, so use
`Bits512`. A sequence that arrives more than N behind the watermark is
reported `TooOld`, even if it was never seen.

## Latency under live-feed traffic

`benches/latency.rs` replays a simulated live feed through every mode and
through an `scc::HashSet` baseline (`seqdedupe::naive`, same answers, window
512).

* **Streams.** 4 redundant streams, each a pinned thread on its own physical
  core. Each delivers the same ids in order at 10k, 50k and 200k msg/s per
  stream.
* **Synchronized bursts.** Messages come in bursts (geometric size, mean 8).
  Burst starts form a Poisson process. All streams see the same bursts at
  the same message ids.
* **Per-stream randomness.** Each stream reaches a burst after its own
  exponential delay (mean 2 µs) and spaces the burst's messages by its own
  exponential gaps (mean 1 µs). The streams agree on *which* ids arrive
  together, but not on exactly *when*.
* **Loss.** 1% per-stream message loss, filled in by the other streams.
* **Measurement.** Each thread spins until a message is due, then times the
  `register()` call alone with `RDTSC`. Results are from a 5 s run on an
  EPYC 9845 (Zen 5c), with up to 4 M samples per configuration and default
  tuning.

![register() latency percentiles per mode, one panel per rate](docs/latency-tails.svg)

![p99 latency per mode and rate](docs/latency-p99.svg)

| Mode | 10k: p50 / p99 / p99.9 ns | 50k: p50 / p99 / p99.9 ns | 200k: p50 / p99 / p99.9 ns |
|---|---|---|---|
| WatermarkOnly | 20 / 50 / 70 | 20 / 70 / 90 | 20 / 60 / 70 |
| Bits64 | 20 / 60 / 110 | 20 / 70 / 90 | 20 / 60 / 80 |
| Bits128 | 30 / 110 / 160 | 30 / 110 / 160 | 30 / 110 / 150 |
| Bits256 | 40 / 120 / 170 | 40 / 120 / 170 | 30 / 110 / 140 |
| Bits512 | 50 / 130 / 180 | 40 / 120 / 170 | 40 / 120 / 150 |
| naive scc (N=512) | 90 / 200 / 310 | 100 / 210 / 320 | 90 / 210 / 370 |

Each mode gives 25% `New` and 75% `Duplicate`, except `WatermarkOnly`, which
reports 56–58% `TooOld`.

**Comparisons:**

* **Bitmap modes vs the scc baseline.**
  * Every bitmap mode beats the hash set at every percentile and every rate.
  * Bits512, which keeps the same 512-message window, is about 2× faster at
    p50 (40–50 vs 90–100 ns) and at p99 (120–130 vs 200–210 ns).
  * At p99.99 it is 2–4× faster (0.4–0.8 µs vs 1.4–2.1 µs).
  * The baseline hashes, takes a bucket lock and touches a bucket line that
    another stream just wrote. The bitmap answers from one header load plus
    at most one slot word.
* **`Bits64` vs `Bits128`–`Bits512`.**
  * Slots cost about 50 ns at p99 (60–70 vs 110–130 ns): a duplicate that
    arrives behind the watermark must read a slot line that the winning
    stream just wrote on another core.
  * `Bits64` keeps the history in the header line itself.
  * Use `Bits64` when 64 messages of reordering is enough.
* **`Bits128` vs `Bits256` vs `Bits512`.** Within 10–20 ns of each other.
  A bigger window is nearly free: the cost is the extra cache line, not the
  shift.
* **`WatermarkOnly` vs `Bits64`.** The same latency, but `WatermarkOnly`
  answers `TooOld` for 56–58% of deliveries. It cannot tell "already seen"
  from "late, never seen", so a late fill-in for a message lost on the
  fastest stream is rejected as stale. In this run that discarded about 0.6%
  of distinct messages (`New` 25.10% vs 25.25% of deliveries).
* **Message rate.** Within about 10 ns from 10k to 200k msg/s. At these
  rates the streams rarely collide on the same header CAS, so the latency is
  mostly cache-line transfers. A thread that sleeps between messages
  instead of spinning would add cold-cache misses on top.
* **Reproducibility.** A second run with another seed matched within about
  10 ns (the TSC resolution here) up to p99.9. p99.99 and max vary up to 2×
  between runs (interrupts, SMIs), so read those as an order of magnitude.

```sh
cargo bench --bench latency                       # writes target/latency-HOST{,-curves}.csv
cargo bench --bench latency -- --rates 10,50,200 --streams 4 --secs 5 \
    --burst-mean 8 --gap-ns 1000 --jitter-ns 2000 --loss 0.01 --out bench-results/latency-HOST
python3 scripts/latency_charts.py bench-results/latency-HOST docs   # SVG charts
```

Raw data: `bench-results/latency-epyc-9845.csv` (summary) and
`bench-results/latency-epyc-9845-curves.csv` (percentile curves).

## More

* `DESIGN.md`: slot ownership, the ABA-free header, optimistic and hazard
  protection, the pool capacity bound, the progress argument and the test
  strategy.
* `BENCHMARKS.md`: throughput under saturation, the contention analysis
  (including why the scc baseline wins 16-thread lockstep duplicates on
  throughput), and the tuning experiments.
* `new_workers_with(.., Tuning)` exposes the tuning knobs:
  * header load instruction;
  * scalar or SIMD shift (including AVX-512 VBMI2);
  * the `delta1` fast path;
  * slot protection (`Optimistic`, the default, or the hazard protocol);
  * reclaim batch size, `pre_protect` and `recheck_before_cas`.
* `seqdedupe::naive`: the `scc::HashSet` baseline.

```sh
cargo test --release
scripts/check_lockfree.sh                       # verify inline lock-free atomics
cargo bench --bench register                    # saturation throughput matrix (~10 min)
cargo bench --bench register -- --quick --only main --threads 1,4
cargo bench --bench register -- --only main --threads 16 --targets Bits512 --loads Dups --reps 3   # for perf record
```
