# Benchmarks

> **Not run on the target CPU.** The plan targets Intel 11th gen. These
> numbers come from two AMD machines:
>
> * **Laptop:** Ryzen 9 4900HS (Zen 2, 8 cores, no AVX-512).
> * **Server:** EPYC 9845 (Zen 5c, 160 cores, AVX-512, 16 cores per L3).
>
> The relative findings should transfer, but rerun on the target before
> drawing conclusions about absolute numbers:
> `cargo bench --bench register`.

## Method

The harness is `benches/register.rs`.

* **Placement and warm-up.** Each thread is pinned to its own physical core
  and spins for 100 ms before a spinning start barrier. Without that warm-up,
  short runs measured the `schedutil` frequency ramp: results were bimodal by
  up to 2×.
* **Two passes per configuration**, each on a fresh deduper with 2 M ops per
  thread:
  * a throughput pass with no timing inside the loop;
  * a latency pass with `lfence; rdtsc` around every call, with the measured
    timer overhead subtracted.
* **Resolution** is about 10 ns, so p50 values of 0–10 ns mean "below timer
  resolution".
* **CAS retries** are counted per worker (`Worker::stats()`).

Workloads (each sequence is dealt round-robin to the threads unless noted):

| Workload | Stream |
|---|---|
| Seq | `first+1, first+2, …` |
| Gaps | 80% `+1`, 15% `+1..N/4`, 5% `+N..2N` (history reset) |
| Late | Seq shuffled within blocks of 16 (constant late arrivals) |
| Dups | every thread sees the whole stream (A/B feed arbitration); with 1 thread, each sequence arrives twice |

**Caveat for Seq, Gaps and Late with more than one thread.** All threads race
to advance one watermark, and the CAS winner tends to keep winning. The
losing threads fall more than N behind, so many of their operations return a
cheap `TooOld`; the New/Dup/Old mix is in the full tables. These are
worst-case contention microbenchmarks, and run-to-run variance is about ±30%.
**Dups is the realistic multi-writer case.**

**Baseline.** `naive-scc/N` is an `scc::HashSet<u64>` plus an `AtomicU64`
watermark. The thread that advances the watermark removes the entries that
fall out of the window (`src/naive.rs`). It gives the same answers when there
are no races, but weaker ones under races:

* a late arrival can be reported `New` after it has fallen out of the window;
* its snapshot is not from a single logical instant.

## Round 2: profiling-driven changes (Zen 5c)

`perf record` / `perf annotate` / raw PMC counts on the EPYC 9845. Raw data is
in `bench-results/epyc-9845-v2.{csv,md}`. The new defaults are optimistic slot
protection, plus the VBMI2 shift for Bits512.

**What the profiles showed** (old defaults, Bits512):

* **Single writer.** About 17% of `register` samples were the
  `LOCK CMPXCHG16B` and about 20% the eight dependent scalar `SHLD`s.
* **Dups, 16 threads.** About 25% went to `protect_current`: the hazard
  `XCHG`, which drains the store buffer, followed by a reload of a header
  line the leader keeps invalidating. Only then could the thread read the
  slot.

**Changes:**

1. **Optimistic slot protection** (`SlotProtection::Optimistic`, now the
   default). The header is ABA-free, so if it is unchanged after a slot read
   (checked by a reload for a `Duplicate` answer, or by the CAS itself for an
   update) the read was of a current, immutable slot. No hazards, no `XCHG`,
   no reclaim scans. See DESIGN.md.
2. **AVX-512 VBMI2 shift** (`ShiftImpl::Avx512Vbmi2`, now the `Auto` choice
   for Bits512).
   * The 512-bit history is one zmm register: masked `VPERMQ` for the word
     shift, `VALIGNQ` + `VPSHLDVQ` for the bit funnel shift, and the old
     watermark bit OR-ed in the register.
   * That is one aligned load and one store; the old version needed two
     unaligned loads that cross into the guard line.
   * Afterwards the shift no longer appears in the profile. The CAS is about
     37% of the slot-update function and nothing else is above 4%.

Throughput, before → after, Mops/s (p99 ns in parentheses):

| Bits512 | 1 thread | 4 threads | 16 threads |
|---|---|---|---|
| Seq | 77.1 → **89.4** (+16%) | 10.0 → 10.5 (p99 2810 → 890) | 2.6 → 3.8 (p99 27 µs → 14 µs) |
| Gaps | 68.4 → **81.5** (+19%) | 14.2 → 16.4 | 12.5 → 15.1 |
| Late | 76.2 → 78.2 (+3%) | 9.2 → 10.3 | 2.6 → 3.6 |
| Dups | 123 → **142** (+15%) | 28.0 → 36.5 (+31%) | 33.0 → 36.7 (p99 1450 → 650) |

* **Bits128 and Bits256** gain +5–12% single-writer and +11–34% at 4
  threads. They use the scalar shift, so the gain comes from optimistic
  protection alone.
* **Late arrivals** gain least, because the late path copies the slot and
  never shifts.

**Separating the two effects** (Bits512, Zen 5c, `protection` and `shift`
experiments):

| | Seq 1T | Dups 1T | Dups 16T (p99) |
|---|---|---|---|
| hazard + scalar (old default) | 77.1 | 121 | 36.0 (1360) |
| hazard + VBMI2 | 81.9 | 131 | 33.1 (1400) |
| optimistic + scalar | 85.3 | 134 | 41.0 (910) |
| optimistic + VBMI2 (new default) | 90.2 | 141 | 39.8 (650) |

* **VBMI2 vs the other shifts** (single-writer Seq): scalar 74.7, AVX2 80.0,
  AVX-512F 77.7, VBMI2 81.1.
  * VBMI2 is about as fast as AVX2. It is preferred because it reads no guard
    words.
  * For Bits256 no SIMD variant beats scalar.
* **Optimistic protection on Zen 2** (laptop, earlier the same day): +15%
  for Bits512 Seq 1T (37 → 43) and +17% for Bits128 Seq 1T (49 → 57). Later
  laptop runs were disturbed by background load and were discarded.

**Tried and rejected:**

* **`recheck_before_cas`**, which re-reads the header before the CAS and
  skips the CAS if it changed. It is worse everywhere: Bits512 Dups 16T drops
  from 39 to 31, and Seq 4T from 10.1 to 7.2. The window in which a loser
  could notice the winner is a few ns, so the extra load is pure overhead.
  The knob stays, off by default.
* **`PREFETCHW` before the header load** with the new defaults: within noise
  (Dups 16T 36.2 vs 36.8, Seq 16T 4.9 vs 3.6, single-writer identical).

### Why naive scc wins Dups at 16 threads

Both implementations give the same answers. The difference is in how the
cores contend:

1. **scc is not lock-free.** In `perf record` of naive Dups 16T, 44% of
   cycles are in `saa::wait_queue::WaitQueue::spin_until`: `insert_sync`
   takes a per-bucket lock, and waiting threads spin in L1. Its p99 is
   2.8 µs, against 0.65 µs for Bits512.
2. **Its contention is spread out; ours is on one line.**
   * Raw PMC `LsDmndFillsFromSys` (cache-to-cache fills within the CCX) per
     op: naive 4.4, Bits512 2.7, Bits64 1.0.
   * So naive moves *more* lines between cores, but they are different
     bucket lines and the transfers overlap.
   * In the bitmap modes, every New must take the one 16-byte header line
     exclusive.
3. **Lockstep losers each pay a locked RMW.** In A/B arbitration all 16
   threads reach sequence `k` together and see `high = k - 1`. All of them
   build the new slot and CAS; one wins.
   * There are 0.6–0.7 CAS failures per op while only 6.25% of ops are New,
     so each new sequence costs about 10 failed `LOCK CMPXCHG16B`s.
   * Each failure takes the header line exclusive in turn, so they serialise.
   * Bits512 also has a dependent second miss on the slot line (header →
     slot id → slot), which is why Bits64 (61 Mops) beats naive and Bits512
     (37) does not.
   * The losers cannot avoid the CAS: the recheck experiment above shows the
     winner's CAS lands too late for them to notice.

**Practical reading:**

* At 4 threads the bitmap modes are 2× naive (36 vs 18–19).
* At 16 lockstep feeds, Bits512 gives 70% of naive's throughput with a
  quarter of its p99, and it is lock-free.
* Getting past this point would need a design that does not route every
  writer through one CAS, such as combining, or the inline-newest-64-bits
  header below.

## Headline results

Throughput in Mops/s (all threads combined), from the main matrix with
default tuning. Full tables are in `bench-results/*.md` and raw CSVs in
`bench-results/*.csv`.

> The tables in this section and in "Experiments" below are from round 1
> (hazard protection, scalar shift). Round 2 numbers for the slot modes are
> in the section above and in `bench-results/epyc-9845-v2.md`.

### Single writer

| Workload | WatermarkOnly | Bits64 | Bits128 | Bits256 | Bits512 | naive-scc/64 | naive-scc/512 |
|---|---|---|---|---|---|---|---|
| Seq, Zen 2 | 126 | 121 | 52 | 47 | 39 | 19 | 19 |
| Seq, Zen 5c | 156 | 126 | 89 | 85 | 77 | 21 | 21 |
| Gaps, Zen 2 | 126 | 86 | 49 | 43 | 36 | 4.8 | 0.9 |
| Gaps, Zen 5c | 156 | 106 | 80 | 76 | 68 | 6.0 | 1.1 |
| Late, Zen 5c | 250 | 104 | 81 | 80 | 76 | 21 | 21 |
| Dups, Zen 5c | 266 | 210 | 149 | 138 | 123 | 29 | 29 |

* A single-writer op costs 8–13 ns for Bits64 and 11–26 ns for the slot modes
  (p99 ≤ 60 ns). That is **2–5× faster than the naive set**.
* With gaps the naive set collapses, because rotation is O(gap). The bitmap
  modes stay **10–70× faster**.

### A/B arbitration (Dups), multiple writers

| Threads | Bits64 | Bits128 | Bits256 | Bits512 | naive-scc/512 |
|---|---|---|---|---|---|
| 4, Zen 2 | 42 | 28 | 28 | 29 | 18 |
| 8, Zen 2 | 57 | 34 | 36 | 31 | 21 |
| 4, Zen 5c | 46 | 27 | 29 | 28 | 18 |
| 16, Zen 5c | 61 | 33 | 33 | 33 | **53** |

* p99 latency is 140–260 ns at 4 threads and 640–1570 ns at 16 threads, which
  beats the naive set (2.8 µs at 16 threads).
* At 16 threads the naive set has the better *throughput*. Most of its
  duplicates are rejected inside one hash bucket. In the bitmap modes, every
  duplicate re-reads a header line that the winning writer keeps
  invalidating.

### Worst-case contention (Seq, every thread advancing)

| Threads | Bits64 | Bits128 | Bits512 | naive-scc/512 |
|---|---|---|---|---|
| 4, Zen 5c | 21 (1.8 retries/op) | 8.7 (1.7) | 10.0 (1.4) | 16 |
| 16, Zen 5c | 21 (1.7) | 3.1 (5.9) | 2.6 (6.6) | 14 |

**This is the weak spot of the slot modes.** The window between the header
load and the CAS is long:

* read the source slot, a cache line just written on another core;
* write the destination slot;
* publish the hazard.

So retries grow with the thread count, and p99 reaches 25–30 µs at 16 threads.
Bits64 keeps everything in the header line and holds up. If several threads
must advance one watermark at high rate, prefer Bits64, or front the deduper
with per-feed sequencing. Untried mitigations are listed at the end.

## Experiments

### Scalar vs SIMD shift (`Tuning::shift`)

| | Seq 1T | Gaps 1T | Late 1T | Seq 4T |
|---|---|---|---|---|
| Bits512 scalar, Zen 5c | 76.7 | 67.4 | 75.3 | 7.2 |
| Bits512 AVX2, Zen 5c | 79.3 | 73.4 | 72.9 | 6.9 |
| Bits512 AVX-512, Zen 5c | 80.6 | 73.1 | 72.2 | 7.2 |
| Bits512 scalar, Zen 2 | 39.1 | 36.5 | 42.5 | 7.2 |
| Bits512 AVX2, Zen 2 | 50.5 | 45.8 | 44.5 | 7.3 |

* On Zen 5c the SIMD shift is within about ±5%. The shift is a few
  instructions next to a ~20-cycle `CMPXCHG16B`.
* On Zen 2, AVX2 gains 5–30% single-threaded for Bits512 and 0–9% for
  Bits256.
* Under contention it makes no difference.
* **Default: scalar.** AVX2 is a reasonable choice for Bits512 single-writer
  use; measure it on the target. AVX-512 for Bits256 falls back to AVX2.

### `delta == 1` fast path (`Tuning::delta1_fast_path`)

* **Single-threaded Seq:**
  * Bits64: +6% (Zen 2), ±0 (Zen 5c);
  * Bits512: +7% on both machines.
* **Under contention:** no difference beyond noise.
* **Default: on.**

### Header load (`Tuning::header_load`)

| | Bits64 Seq 1T | Bits64 Dups 1T | Bits512 Seq 1T | Bits64 Dups 4T | Bits512 Seq 4T | Bits512 Dups 4T |
|---|---|---|---|---|---|---|
| `LOCK CMPXCHG16B`, Zen 2 | 68 | 85 | 35 | 24 | 5.9 | 23 |
| `VMOVDQA`, Zen 2 | 121 | 154 | 39 | 42 | 7.2 | 30 |
| `PREFETCHW`+`VMOVDQA`, Zen 2 | 113 | 154 | 39 | 47 | 8.5 | 31 |
| `LOCK CMPXCHG16B`, Zen 5c | 76 | 98 | 54 | 38 | 8.6 | 24 |
| `VMOVDQA`, Zen 5c | 126 | 209 | 78 | 47 | 7.3 | 28 |
| `PREFETCHW`+`VMOVDQA`, Zen 5c | 129 | 208 | 77 | 44 | 9.1 | 34 |

* **The locked load costs a full RMW.** It is 1.4–2× slower single-threaded,
  and 1.2–1.8× slower for duplicate-heavy traffic.
* **`PREFETCHW` helps only the contended slot-mode advance**, by about 20% on
  both machines: it requests ownership early, before the CAS. It is neutral
  elsewhere, within noise.
* **Default: `VMOVDQA`**, which is guaranteed atomic on AVX-capable CPUs.
  `PREFETCHW` is the next candidate to confirm on the target.

### Hazard pre-publication (`Tuning::pre_protect`)

| | Bits128 Seq 1T | Bits512 Seq 1T | Bits512 Dups 4T |
|---|---|---|---|
| off, Zen 2 | 45 | 36 | 24 |
| on, Zen 2 | 53 | 38 | 29 |
| off, Zen 5c | 60 | 54 | 30 |
| on, Zen 5c | **88** | **77** | 35 |

* When the same writer advances again, it skips the hazard `XCHG` and the
  revalidation load: up to +48% single-writer, and +15–20% for contended
  duplicates.
* This was only sound after making the header ABA-free (see `DESIGN.md`).
  The first version lost updates, and the concurrent tests caught it.
* **Default: on.**

### Reclamation batch size (`Tuning::reclaim_batch`)

Each participant owns `P − 1 + R` slots, and a hazard scan runs every ≥ R
successful updates.

| R | Zen 2 1T | Zen 2 4T | Zen 5c 1T | Zen 5c 4T | Zen 5c 16T |
|---|---|---|---|---|---|
| 1 | 33.6 | 6.8 | 65.9 | 6.1 | 2.7 |
| 4 | 39.8 | 7.0 | 75.0 | 6.8 | 2.7 |
| 16 | 40.3 | 7.4 | 75.4 | 8.1 | 2.6 |
| 64 | 30.5 | 8.2 | 75.2 | 10.5 | 2.8 |

* With R = 1, every update scans: −15% single-threaded.
* At R = 64 the pool outgrows L1 on Zen 2: −25% and a longer p99 there.
  Under contention, larger pools help a little because hot slot lines are
  reused less often.
* **Default: R = 8.** Memory is `(1 + P(P − 1 + R))` slots × 64 or 128 B.

## Untried mitigations for the contended slot-mode advance

* Exponential backoff after a failed CAS.
* ~~Re-checking the header before the CAS~~: tried in round 2, slower.
* Flat combining (one writer applies a batch of pending registrations).
* Switching to an inline 128-bit header for the newest 64 bits, with slots
  only for older history.

## Live-feed latency

The paced, latency-focused benchmark (4 redundant streams at 10k, 50k and
200k msg/s, Poisson bursts synchronized across streams) is described with
charts in `README.md` under "Latency under live-feed traffic". The source is
`benches/latency.rs`; the data is in `bench-results/latency-epyc-9845*.csv`.
