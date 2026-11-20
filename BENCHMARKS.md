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
  mean of 68 ns. The tables below label throughput as untimed and latency as
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
| Seq | 156 | 75 | 99 | 93 | 89 | 21 | 21 |
| Gaps | 156 | 76 | 88 | 82 | 81 | 6.0 | 1.1 |
| Late | 216* | 75 | 87 | 85 | 78 | 20 | 20 |
| Dups | 269 | 96 | 166 | 154 | 141 | 28 | 28 |

\* `Latest` rejects 79% of Late as `TooOld`.

* A call costs 10–13 ns for `Last64` and 6–13 ns for the slot modes, with
  a timed p99 at or below 40 ns: **3.4–6× faster than the hash set**.
* `Last64` is slower than the slot modes on one thread because it reads its
  header with a locked `LOCK CMPXCHG16B`, which pays off only under
  contention (see "Header load" below).
* **With gaps the hash set collapses**, because removing old entries is
  O(gap): `Last64` is 13× faster than `HashSet64`, and `Last512` 74× faster
  than `HashSet512`. Its p99 is 2.3 µs (N = 64) and 17 µs (N = 512), against
  20–30 ns for the bitmap modes.

## A/B arbitration (Dups), multiple writers

Untimed M ops/s, all threads combined (timed p99 ns per call):

| Threads | Last64 | Last128 | Last256 | Last512 | HashSet64 | HashSet512 |
|---|---|---|---|---|---|---|
| 2 | 34 (40) | 42 (120) | 42 (110) | 32 (120) | 13 (300) | 14 (280) |
| 4 | 35 (70) | 28 (210) | 33 (220) | 35 (170) | 16 (820) | 18 (570) |
| 8 | 40 (140)* | 36 (480) | 38 (470) | 36 (320) | 28 (2,540) | 29 (1,370) |
| 16 | 44 (310)* | 40 (910) | 36 (910) | 40 (660) | 45 (3,030) | 50 (2,760) |

\* With 8 and 16 threads, 32–42% of `Last64` calls return `TooOld`: the
thread holding the header line runs more than 64 ids ahead of the others,
and their copies fall out of the window. Every id is still `New` exactly
once, but those calls skip the CAS, so the throughput is not comparable.

* Up to 8 threads every bitmap mode beats the hash set on throughput, by
  1.2–3×.
* p99 is 2.3–3× lower for the slot modes at every thread count, and 7–10×
  lower for `Last64`.
* At 16 threads the hash set has the higher throughput; see below.

## Worst-case contention (Seq, every thread advancing)

Untimed M ops/s (timed p99 ns, CAS retries per op):

| Threads | Last64 | Last128 | Last256 | Last512 | HashSet512 |
|---|---|---|---|---|---|
| 2 | 34 (40, 0.2) | 25 (240, 0.6) | 25 (240, 0.5) | 18 (200, 0.8) | 14 (250) |
| 4 | 35 (50, 0.3) | 10 (930, 2.6) | 10 (1,040, 2.6) | 9.4 (850, 2.9) | 14 (550) |
| 16 | 43 (330, 0.1) | 3.7 (15,200, 10.6) | 3.0 (17,200, 13.3) | 3.7 (14,700, 10.5) | 11 (3,850) |

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
   Its p99 is 2.8 µs, against 0.66 µs for `Last512`.
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
   * `Last512` makes 0.68 failed CASes per op while only 6.25% of ops are
     `New`: about 11 failed `LOCK CMPXCHG16B`s per new sequence.
   * Each failure takes the header line exclusive in turn, so they
     serialise.
   * The slot modes also have a dependent second miss on the slot line
     (header → slot id → slot).
   * `Last64` fails only 0.06 CASes per op, but its locked loads serialise
     the same way: 44 M ops/s, with 42% of calls falling behind the window
     as `TooOld`.
   * The losers cannot avoid the CAS: re-reading the header first
     (`recheck_before_cas`, below) is slower, because the winner's CAS lands
     too late for them to notice.

**Practical reading:** at 2–8 threads the bitmap modes win on both
throughput and tail. At 16 lockstep feeds, `Last512` gives 80% of the hash
set's throughput with 24% of its p99, and it is lock-free. Going further
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
| Last128, hazard | 88 | 80 | 149 | 1,990 ns |
| Last128, optimistic | 99 | 89 | 166 | 940 ns |
| Last256, hazard | 84 | 76 | 142 | 2,390 ns |
| Last256, optimistic | 94 | 84 | 157 | 910 ns |
| Last512, hazard | 75 | 64 | 123 | 2,110 ns |
| Last512, optimistic | 85 | 72 | 129 | 910 ns |
| Last512, hazard + VBMI2 | 81 | 75 | 131 | 1,700 ns |
| Last512, optimistic + VBMI2 (default) | 89 | 82 | 142 | 970 ns |

* **Optimistic validation is 5–13% faster single-threaded** and cuts the
  4-thread Seq p99 by 43–62%: no hazard `XCHG`, no revalidation load, no
  reclaim scans.
* **Default: optimistic.**

### Shift (`Tuning::shift`)

| | Seq 1T | Gaps 1T | Late 1T |
|---|---|---|---|
| Last256 scalar | 92 | 85 | 86 |
| Last256 AVX2 | 90 | 83 | 81 |
| Last256 AVX-512F | 90 | 83 | 81 |
| Last256 VBMI2 | 90 | 84 | 80 |
| Last512 scalar | 85 | 72 | 80 |
| Last512 AVX2 | 88 | 81 | 79 |
| Last512 AVX-512F | 89 | 82 | 79 |
| Last512 VBMI2 | 89 | 80 | 77 |

* **Last512:** SIMD gains 4–5% on Seq and 11–14% on Gaps. The three SIMD
  variants are within 3%; VBMI2 is preferred because it keeps the whole
  history in one register (one load, one store, no guard words).
* **Last256:** scalar is fastest on all three workloads.
* Late arrivals copy the slot and never shift, so they are unaffected.
* **Default (`Auto`):** VBMI2, else AVX2, for Last512; scalar otherwise.

### `seq == high + 1` fast path (`Tuning::delta1_fast_path`)

| | off | on |
|---|---|---|
| Last64, Seq 1T | 75 | 76 |
| Last128, Seq 1T | 94 | 99 |
| Last512, Seq 1T | 87 | 89 |

* +2–5% for the slot modes. `Last64` is branch-free (`unbounded_shl`), so
  the knob does not apply to it.
* **Default: on.**

### Header load (`Tuning::header_load`)

M ops/s (p99 ns):

| | Seq 1T | Dups 1T | Seq 4T | Dups 2T | Dups 4T |
|---|---|---|---|---|---|
| Last64, `LOCK CMPXCHG16B` (default) | 75 | 98 | 34 (50) | 35 (40) | 34 (50) |
| Last64, `VMOVDQA` | 126 | 204 | 22 (60) | 55 (90) | 52 (160) |
| Last64, `PREFETCHW` + `VMOVDQA` | 126 | 203 | 24 (80) | 55 (90) | 46 (170) |
| Last512, `LOCK CMPXCHG16B` | 57 | 75 | 9.2 (750) | 34 (140)* | 24 (170)* |
| Last512, `VMOVDQA` (default) | 89 | 142 | 8.9 (1,020) | 38 (110) | 31 (180) |
| Last512, `PREFETCHW` + `VMOVDQA` | 89 | 142 | 12 (1,040) | 35 (110) | 28 (230) |

\* 42–46% `TooOld`: one thread runs ahead of the window.

* **The locked load costs a full RMW:** `VMOVDQA` is 1.6–2.1× faster
  single-threaded.
* **`Last64` under contention:** with 2 and 4 threads the locked load cuts
  the Dups p99 by 2–3× (to 40–50 ns) and the CAS retries on Seq by 6–20×,
  at the cost of 35% of the Dups throughput. Almost every `Last64` load is
  followed by a CAS on the same line, and a locked load makes the
  load-to-CAS window a single ownership of the line. `PREFETCHW`, which
  also requests the line exclusive, does not get the same effect, so the
  locked operation itself (it holds the line until it completes) seems to
  be what matters.
* **The slot modes** also reload the header to validate every `Duplicate`;
  a locked load turns those reads into writes, so it loses there.
* `PREFETCHW` brings nothing measurable.
* **Default (`Auto`):** `LOCK CMPXCHG16B` for `Last64`, which is tuned for
  concurrency; `VMOVDQA` (single-copy atomic on AVX-capable CPUs) for the
  slot modes.

### Hazard pre-publication (`Tuning::pre_protect`, hazard protection only)

| | off | on |
|---|---|---|
| Last128, Seq 1T | 59 | 87 |
| Last128, Dups 1T | 103 | 149 |
| Last512, Seq 1T | 57 | 82 |
| Last512, Dups 1T | 97 | 132 |

* When the same writer advances again, it skips the hazard `XCHG` and the
  revalidation load: +36–47% single-threaded. It relies on the ABA-free
  header (see `DESIGN.md`).
* **Default: on.** It has no effect with optimistic protection.

### Recheck before CAS (`Tuning::recheck_before_cas`)

| | off | on |
|---|---|---|
| Last64, Seq 1T | 75 | 52 |
| Last64, Seq 4T | 34 | 19 |
| Last512, Seq 4T (p99) | 9.1 (940 ns) | 7.6 (2,080 ns) |

* Re-reading the header before the CAS and skipping it if it changed is
  slower everywhere: the window in which a loser could notice the winner is
  a few ns, so the extra load is pure overhead.
* **Default: off.**

### Slot pool size (`Tuning::reclaim_batch`)

Last512, Seq, M ops/s:

| R | 1T | 2T | 4T |
|---|---|---|---|
| 1 | 75 | 15 | 8.9 |
| 4 | 87 | 18 | 11 |
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
