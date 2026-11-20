# Benchmarks

All numbers are from an **AMD EPYC 9845** (Zen 5c, 160 cores, AVX-512, 16
cores per L3) with default tuning unless noted. The Intel 11th-gen target has
not been measured; rerun there before relying on absolute numbers:
`cargo bench --bench register`.

Raw data:

* `bench-results/epyc-9845.{csv,md}`: main matrix, 1–16 threads.
* `bench-results/epyc-9845-experiments.{csv,md}`: tuning experiments, 1–4
  threads.
* `bench-results/latency-epyc-9845*.csv`: live-feed latency (see README).

## Method

The harness is `benches/register.rs`.

* **Placement and warm-up.** Each thread is pinned to its own physical core
  and spins for 100 ms before a spinning start barrier, so it measures a core
  at full clock rather than the frequency ramp.
* **Two passes per configuration**, each on a fresh deduper with 2 M ops per
  thread:
  * a throughput pass with no timing inside the loop;
  * a latency pass with `lfence; rdtsc` around every call (timer overhead
    subtracted), recorded into per-thread HDR histograms.
* **Resolution** is about 10 ns, so p50 values of 0–10 ns mean "below timer
  resolution".
* **CAS retries** are counted per worker (`Worker::stats()`).
* **Which pass each column comes from.** Throughput, ns/op, CAS retries
  and the New/Dup/Old mix come from the untimed pass. Mean, p50, p99 and
  p99.9 come from the timed pass's histogram. The timed pass puts about
  20 ns of timer and histogram work between calls, which lowers contention,
  so the two passes do not describe the same calls: with 4 threads on Dups,
  `Last64` takes 114 ns per call untimed (4 / 35 M ops/s) against a timed
  mean of 43 ns. The tables below label throughput as untimed and latency as
  timed; the live-feed benchmark paces calls like a real feed.
* **Variance.** Single-thread results repeat within a few percent. With 4 or
  more threads advancing one watermark, throughput varies by ±20–30% between
  runs; p99 is more stable.

Workloads (each sequence is dealt round-robin to the threads unless noted):

| Workload | Stream |
|---|---|
| Seq | `first+1, first+2, …` |
| Gaps | 80% `+1`, 15% `+1..N/4`, 5% `+N..2N` (history reset) |
| Late | Seq shuffled within blocks of 16 (constant late arrivals) |
| Dups | every thread sees the whole stream (A/B feed arbitration); with 1 thread, each sequence arrives twice |

**Seq, Gaps and Late with more than one thread** are worst-case contention
microbenchmarks: all threads race to advance one watermark, and the losers
fall behind, so many of their operations return a cheap `TooOld` (the
New/Dup/Old mix is in the full tables). **Dups is the realistic multi-writer
case.**

**Baseline.** `HashSetN` is an `scc::HashSet<u64>` plus an `AtomicU64`
watermark; the thread that advances the watermark removes the entries that
fall out of the window (`benches/common/hashset.rs`). It gives the same
answers when there are no races, but weaker ones under races:

* a late arrival can be reported `New` after it has fallen out of the window;
* its snapshot is not from a single logical instant.

## Single writer

Untimed throughput in M ops/s:

| Workload | Latest | Last64 | Last128 | Last256 | Last512 | HashSet64 | HashSet512 |
|---|---|---|---|---|---|---|---|
| Seq | 85 | 75 | 99 | 94 | 89 | 21 | 21 |
| Gaps | 85 | 75 | 89 | 83 | 81 | 6.0 | 1.1 |
| Late | 132* | 76 | 87 | 83 | 78 | 20 | 20 |
| Dups | 110 | 98 | 165 | 155 | 141 | 28 | 28 |

\* `Latest` rejects 79% of Late as `TooOld`.

* A call costs 10–13 ns for `Last64` and 6–13 ns for the slot modes, with
  a timed p99 at or below 40 ns: **3.5–6× faster than the hash set**.
* `Latest` and `Last64` are slower than the slot modes on one thread
  because they read the header with a locked RMW, which pays off only under
  contention (see "Header load" below).
* **With gaps the hash set collapses**, because removing old entries is
  O(gap): `Last64` is 12× faster than `HashSet64`, and `Last512` 74× faster
  than `HashSet512`. Its p99 is 2.3 µs (N = 64) and 17 µs (N = 512), against
  20–30 ns for the bitmap modes.

## A/B arbitration (Dups), multiple writers

Untimed M ops/s, all threads combined (timed p99 ns per call):

| Threads | Last64 | Last128 | Last256 | Last512 | HashSet64 | HashSet512 |
|---|---|---|---|---|---|---|
| 2 | 34 (50) | 37 (110) | 41 (120) | 37 (120) | 12 (310) | 14 (290) |
| 4 | 35 (50) | 34 (220) | 34 (240) | 34 (180) | 16 (820) | 18 (600) |
| 8 | 45 (140)* | 38 (480) | 39 (470) | 38 (320) | 24 (2,490) | 28 (1,370) |
| 16 | 44 (310)* | 40 (910) | 38 (910) | 39 (640) | 45 (3,010) | 50 (2,770) |

\* With 8 and 16 threads, 38–39% of `Last64` calls return `TooOld`: the
thread holding the header line runs more than 64 ids ahead of the others,
and their copies fall out of the window. Every id is still `New` exactly
once, but those calls skip the CAS, so the throughput is not comparable.

* Up to 8 threads every bitmap mode beats the hash set on throughput, by
  1.3–3.4×.
* p99 is 2.4–3× lower for the slot modes at every thread count, and 6–12×
  lower for `Last64`.
* At 16 threads the hash set has the higher throughput; see below.

## Worst-case contention (Seq, every thread advancing)

Untimed M ops/s (timed p99 ns, CAS retries per op):

| Threads | Last64 | Last128 | Last256 | Last512 | HashSet512 |
|---|---|---|---|---|---|
| 2 | 34 (40, 0.3) | 29 (230, 0.5) | 25 (250, 0.6) | 18 (210, 0.8) | 14 (250) |
| 4 | 34 (50, 0.4) | 11 (850, 2.5) | 11 (840, 2.6) | 9.5 (910, 2.9) | 14 (560) |
| 16 | 43 (330, 0.1) | 3.3 (15,700, 11.9) | 3.4 (22,200, 11.6) | 3.8 (17,100, 10.5) | 11 (4,000) |

**This is the weak spot of the slot modes.** The window between the header
load and the CAS is long: read the source slot (a line just written on
another core) and write the destination slot. So retries grow with the
thread count. `Last64` keeps everything in the header line, takes it exclusive with
its first (locked) load, and holds up. If
several threads must advance one watermark at high rate, prefer `Last64`, or
front the deduper with per-feed sequencing.

## Why the hash set wins Dups at 16 threads

Both give the same answers. The difference is in how the cores contend.

1. **The hash set is not lock-free.** In `perf record` of `HashSet512` Dups
   with 16 threads, 37% of cycles are in `saa::wait_queue::WaitQueue::spin_until`:
   `insert_sync` takes a per-bucket lock, and waiting threads spin in L1.
   Its p99 is 2.8 µs, against 0.64 µs for `Last512`.
2. **Its contention is spread out; ours is on one line.**
   * `ls_dmnd_fills_from_sys.local_ccx` (fills from L3 or another core's L2)
     per op: `HashSet512` 4.7, `Last512` 2.0, `Last64` 1.0.
   * So the hash set moves *more* lines between cores, but they are
     different bucket lines and the transfers overlap.
   * In the bitmap modes, every `New` must take the one header line
     exclusive.
3. **Lockstep losers each pay a locked RMW.** In A/B arbitration all 16
   threads reach sequence `k` together and see `high = k - 1`. All of them
   build the new slot and CAS; one wins.
   * `Last512` makes 0.66 failed CASes per op while only 6.25% of ops are
     `New`: about 11 failed `LOCK CMPXCHG16B`s per new sequence.
   * Each failure takes the header line exclusive in turn, so they
     serialise.
   * The slot modes also have a dependent second miss on the slot line
     (header → slot id → slot).
   * `Last64` fails only 0.09 CASes per op, but its locked loads serialise
     the same way: 44 M ops/s, with 38% of calls falling behind the window
     as `TooOld`.
   * The losers cannot avoid the CAS: re-reading the header first
     (`recheck_before_cas`, below) is slower, because the winner's CAS lands
     too late for them to notice.

**Practical reading:** at 2–8 threads the bitmap modes win on both
throughput and tail. At 16 lockstep feeds, `Last512` gives 78% of the hash
set's throughput with 23% of its p99, and it is lock-free. Going further
would need a design that does not route every writer through one CAS, such
as combining, or an inline header for the newest 64 bits with slots only for
older history.

## Tuning experiments

Single-thread numbers are stable; the 4-thread p99 shows the effect under
contention. Full tables, including 2 threads, are in
`bench-results/epyc-9845-experiments.md`.

### Slot protection (`Tuning::protection`)

| | Seq 1T | Gaps 1T | Dups 1T | Seq 4T p99 |
|---|---|---|---|---|
| Last128, hazard | 88 | 80 | 146 | 2,210 ns |
| Last128, optimistic | 99 | 88 | 166 | 910 ns |
| Last256, hazard | 84 | 75 | 141 | 2,150 ns |
| Last256, optimistic | 94 | 84 | 157 | 1,000 ns |
| Last512, hazard | 80 | 68 | 124 | 1,600 ns |
| Last512, optimistic | 83 | 72 | 130 | 920 ns |
| Last512, hazard + VBMI2 | 82 | 76 | 132 | 1,920 ns |
| Last512, optimistic + VBMI2 (default) | 89 | 81 | 141 | 870 ns |

* **Optimistic validation is 4–14% faster single-threaded** and cuts the
  4-thread Seq p99 by 43–59%: no hazard `XCHG`, no revalidation load, no
  reclaim scans.
* **Default: optimistic.**

### Shift (`Tuning::shift`)

| | Seq 1T | Gaps 1T | Late 1T |
|---|---|---|---|
| Last256 scalar | 94 | 84 | 83 |
| Last256 AVX2 | 89 | 84 | 81 |
| Last256 AVX-512F | 89 | 84 | 81 |
| Last256 VBMI2 | 90 | 85 | 80 |
| Last512 scalar | 84 | 71 | 80 |
| Last512 AVX2 | 88 | 81 | 79 |
| Last512 AVX-512F | 88 | 82 | 78 |
| Last512 VBMI2 | 89 | 82 | 80 |

* **Last512:** SIMD gains 5–6% on Seq and 14–15% on Gaps. The three SIMD
  variants are within 2%; VBMI2 is preferred because it keeps the whole
  history in one register (one load, one store, no guard words).
* **Last256:** scalar is fastest for Seq and Late; Gaps is a tie.
* Late arrivals copy the slot and never shift, so they are unaffected.
* **Default (`Auto`):** VBMI2, else AVX2, for Last512; scalar otherwise.

### `seq == high + 1` fast path (`Tuning::delta1_fast_path`)

| | off | on |
|---|---|---|
| Last64, Seq 1T | 75 | 75 |
| Last128, Seq 1T | 94 | 99 |
| Last512, Seq 1T | 87 | 89 |

* +2–5% for the slot modes. `Last64` is branch-free (`unbounded_shl`), so
  the knob does not apply to it.
* **Default: on.**

### Header load (`Tuning::header_load`)

M ops/s (p99 ns):

Untimed M ops/s (timed p99 ns):

| | Seq 1T | Dups 1T | Seq 4T | Dups 2T | Dups 4T |
|---|---|---|---|---|---|
| Latest, `LOCK CMPXCHG` (default) | 85 | 110 | 65 (60) | 68 (50) | 68 (60) |
| Latest, plain `MOV` | 157 | 270 | 70 (160) | 76 (70) | 72 (160) |
| Last64, `LOCK CMPXCHG16B` (default) | 75 | 99 | 34 (60) | 35 (40) | 35 (60) |
| Last64, `VMOVDQA` | 125 | 203 | 21 (60) | 53 (90) | 52 (160) |
| Last64, `PREFETCHW` + `VMOVDQA` | 125 | 204 | 22 (60) | 56 (90) | 50 (160) |
| Last512, `LOCK CMPXCHG16B` | 57 | 75 | 9.7 (720) | 36 (120)* | 21 (170)* |
| Last512, `VMOVDQA` (default) | 89 | 142 | 9.0 (860) | 37 (110) | 31 (180) |
| Last512, `PREFETCHW` + `VMOVDQA` | 89 | 142 | 9.1 (1,030) | 34 (110) | 29 (220) |

\* 20–48% `TooOld`: one thread runs ahead of the window.

* **The locked load costs a full RMW:** the plain load is 1.6–2.5× faster
  single-threaded.
* **`Latest` and `Last64` under contention:** with 2 and 4 threads the
  locked load cuts the Dups p99 by 1.4–2.7× (to 40–60 ns) and the CAS
  retries on Seq by 4–13×. It costs 6–11% of the Dups throughput for
  `Latest` and a third for `Last64`. In both, almost every load is
  followed by a CAS on the same line, and a locked load makes the
  load-to-CAS window a single ownership of the line. `PREFETCHW`, which
  also requests the line exclusive, does not get the same effect, so the
  locked operation itself (it holds the line until it completes) seems to
  be what matters.
* **The slot modes** also reload the header to validate every `Duplicate`;
  a locked load turns those reads into writes, so it loses there.
* `PREFETCHW` brings nothing measurable.
* **Default (`Auto`):** a locked load for `Latest` and `Last64`, which are
  tuned for concurrency; `VMOVDQA` (single-copy atomic on AVX-capable CPUs)
  for the slot modes.

### Hazard pre-publication (`Tuning::pre_protect`, hazard protection only)

| | off | on |
|---|---|---|
| Last128, Seq 1T | 59 | 88 |
| Last128, Dups 1T | 103 | 150 |
| Last512, Seq 1T | 57 | 82 |
| Last512, Dups 1T | 96 | 131 |

* When the same writer advances again, it skips the hazard `XCHG` and the
  revalidation load: +36–49% single-threaded. It relies on the ABA-free
  header (see `DESIGN.md`).
* **Default: on.** It has no effect with optimistic protection.

### Recheck before CAS (`Tuning::recheck_before_cas`)

| | off | on |
|---|---|---|
| Last64, Seq 1T | 75 | 52 |
| Last64, Seq 4T | 34 | 19 |
| Last512, Seq 4T (p99) | 8.9 (810 ns) | 7.8 (2,190 ns) |

* Re-reading the header before the CAS and skipping it if it changed is
  slower everywhere: the window in which a loser could notice the winner is
  a few ns, so the extra load is pure overhead.
* **Default: off.**

### Slot pool size (`Tuning::reclaim_batch`)

Last512, Seq, M ops/s:

| R | 1T | 2T | 4T |
|---|---|---|---|
| 1 | 75 | 19 | 8.9 |
| 4 | 87 | 20 | 11 |
| 16 | 91 | 19 | 11 |
| 64 | 92 | 19 | 11 |

* With optimistic protection each participant owns `R` slots and reuses
  them oldest-first; with hazards it owns `P − 1 + R` and scans hazards every
  `R` updates.
* R = 1 is 14–18% slower single-threaded; beyond 4 the gain is a few
  percent.
* **Default: R = 8.** Memory is `1 + P·R` slots (optimistic) or
  `1 + P(P − 1 + R)` (hazards), 64 or 128 B each.

## Untried mitigations for the contended slot-mode advance

* Exponential backoff after a failed CAS.
* Flat combining (one writer applies a batch of pending registrations).
* An inline 128-bit header for the newest 64 bits, with slots only for older
  history.

## Live-feed latency

The paced latency benchmark (2 and 4 redundant streams at 10k, 50k and
200k msg/s, Poisson bursts synchronized across streams) is described with
charts in `README.md` under "Latency under live-feed traffic". The source is
`benches/latency.rs`; the data is in `bench-results/latency-epyc-9845*.csv`.
