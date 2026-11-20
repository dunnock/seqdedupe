# seqdedupe design

## Constraints (from the spec)

* A fixed set of `P` participants. Each holds one `Worker` (`Send`, not
  `Clone`). Snapshot-only threads are participants too.
* `register(seq)` returns `New`, `Duplicate` or `TooOld`, over the full `u64`
  range. `first_seen` counts as already registered, and
  `expected_start <= first_seen` is required.
* Bit `i` of the history means `high - 1 - i` was seen. An advance by `d`
  shifts left by `d`, clears the history if `d >= N`, and marks the old
  watermark at bit `d - 1` if `d <= N`.
* `snapshot()` returns the watermark and the history from one logical
  instant. `missing()` is `None` for `Latest`.
* Lock-free on x86-64 with `CX16` (compiled with `target_feature =
  "cmpxchg16b"`), with preallocated memory only. A stalled
  participant must not block anyone else.

## Invariants

1. **Header.** The shared state is a single atomic value: an `AtomicU64`
   (`Latest`), or a 16-byte header `{high, mask}` (`Last64`) or
   `{high, slot_id | late_updates << 32}` (`Last128/256/512`). Every update is
   one successful CAS on it, and that CAS is the linearization point.
2. **No header value repeats.** Each successful CAS either raises `high`
   (resetting `late_updates` to 0) or keeps `high` and increments
   `late_updates`. So `(high, late_updates)` strictly increases, and an
   unchanged header proves nothing happened in between.
3. **Current slots are immutable.** Only the slot's owner writes it, and only
   before the CAS that publishes it. A displaced slot goes to the displacing
   writer's pool. Ownership changes only on a successful CAS.
4. **Pools are disjoint and fixed in size.** Every slot except the current
   one belongs to exactly one participant's pool, whose size never changes.
5. **Reads are validated before use.** A slot read is acted on only after
   the header is seen unchanged (by the CAS for an update, or a reload for a
   `Duplicate` or a snapshot), or while a validated hazard protects the slot.

## Design choices

| Choice | Why | Alternative kept as a `Tuning` knob |
|---|---|---|
| Inline-asm `LOCK CMPXCHG16B` for the 128-bit CAS | `AtomicU128` is unstable; the CAS must be one locked instruction | none |
| `Last64` loads its header with `LOCK CMPXCHG16B` | Tuned for concurrency: almost every load is followed by a CAS on the same line, and the locked load cuts the 2–4-thread p99 by 2–3×, at the cost of 35–50% of the throughput | `Avx`, `PrefetchwAvx` |
| The slot modes load the header with aligned `VMOVDQA` | Single-copy atomic on AVX CPUs (Intel SDM, AMD APM) and 1.6–1.9× faster than a locked load; the `Duplicate` validation reload must not write | `LockCmpxchg16b`, `PrefetchwAvx` |
| `Last64` keeps its history inline in the header | The whole update is one CAS on one line | none |
| Larger windows keep an immutable bitmap in a preallocated slot, swapped in by CAS | The history cannot fit in the 128-bit atomic | none |
| Optimistic validation (seqlock-style, made sound by invariant 2) | No hazard `XCHG` and no reclaim scans; 5–13% faster single-threaded, 43–62% lower p99 when 4 threads advance | `SlotProtection::Hazard`: one hazard per participant with publish-and-revalidate, `pre_protect`, `reclaim_batch` |
| Pool of `R` slots per participant (`P - 1 + R` with hazards) | Allocation never waits. With hazards, at most `P - 1` slots are pinned, so each scan frees at least `R` | `reclaim_batch` (default 8) |
| Last512 shift: AVX-512 VBMI2 in one zmm register (`VPERMQ` + `VPSHLDVQ`), else AVX2; scalar for smaller windows | +4–5% on Seq and +11–14% on Gaps for Last512; no gain for Last256 | `ShiftImpl` |
| Branch-free `Last64` advance (`unbounded_shl`, `d` clamped to 65) | No branch mispredicts on random jump sizes | none |
| `seq == high + 1` fast path for the slot modes | +2–5% for one writer | `delta1_fast_path` |
| Header and hazard entries are `align(128)` | Avoids false sharing through the adjacent-line prefetcher | none |
| Guard words of zero before each slot | Makes the scalar and AVX2 shift branch-free | none |

**Known caveat.** Under optimistic validation, a stale reader may copy a
slot while its owner rewrites it. In the Rust memory model that is a data
race (the usual seqlock caveat, see RFC 3301). The copy is thrown away unless
the CAS proves the slot did not change.

**Progress.**

* A CAS or a validation retries only when another participant's CAS
  succeeded.
* Header-only answers are wait-free: `TooOld`, or `Duplicate` of `high`
  itself.
* A stalled participant pins nothing with optimistic validation, and at most
  one slot (already budgeted for) with hazards.

## Testing methodology

* **Reference model.** A `BTreeSet` of every sequence that returned `New`
  serves as the model. Random traffic biased towards edge cases goes through
  1–5 workers, round-robin. It runs every mode × every tuning the CPU
  supports: header load, shift, `delta1`, batch, protection, `pre_protect`
  and recheck.
* **Edge cases:**
  * jumps of `N-1`, `N` and `N+1`, and jumps of `2^32 + k` (a shift amount
    truncated to `u32` would leave stale history);
  * sequence 0 and `u64::MAX`;
  * `expected_start` boundaries;
  * invalid bounds and participant limits.
* **Shift agreement.** Every SIMD shift matches the scalar one for every
  `d` in `1..N`.
* **Concurrent tests,** for every protection and SIMD tuning:
  * duplicate race: exactly one `New` per sequence;
  * interleaved late arrivals;
  * snapshots under writes are never torn;
  * random traffic: at most one `New` per sequence;
  * stalled hazard reader: the pinned slot is intact and writers keep
    cycling;
  * a deterministic stale-header revalidation test.
* **Regression test** for the register-allocation hazards in the CAS asm
  (RBX is swapped around `CMPXCHG16B`).
* **Mutation testing.** Every planted bug is caught by at least one test:
  * hazard ignored;
  * revalidation skipped;
  * shift off by one;
  * `delta1` path wrong;
  * immediate slot reuse;
  * `missing()` mask off by one;
  * broken AVX load;
  * ABA counter removed;
  * optimistic reload dropped;
  * snapshot recheck dropped.

  Known gap: replacing the hazard `XCHG` with a plain store is not detected.
* **Build-time gate.** The 128-bit modes compile only with
  `target_feature = "cmpxchg16b"`; a test built with the feature disabled
  checks that `new_workers` then refuses them with
  `InitError::MissingTargetFeature`.

## Benchmarking methodology

* **Placement.** One thread per physical core, pinned. Each thread spins for
  100 ms before a spinning start barrier, so it measures a core at full clock
  rather than the frequency ramp.
* **Timing.** `lfence; rdtsc` around each call, with the measured timer
  overhead subtracted, recorded into preallocated per-thread HDR histograms
  (`hdrhistogram`, 3 significant digits) that are merged after the run. The
  resolution is about 10 ns.
* **Throughput (`benches/register.rs`).**
  * Saturation workloads: Seq, Gaps, Late, and Dups (every thread sees every
    sequence).
  * 1–16 threads, 2 M ops per thread, on a fresh deduper per run.
  * One throughput pass and one latency pass per configuration.
  * Reported: Mops/s, p50/p99/p99.9, CAS retries per op, and the
    New/Dup/Old mix.
  * Experiments switch one `Tuning` knob at a time.
  * Run-to-run variance is about ±20–30% when many threads advance one
    watermark.
* **Live-feed latency (`benches/latency.rs`).**
  * 2 and 4 redundant streams at 10k, 50k and 200k msg/s each, with 1% loss.
  * Poisson bursts with the same message ids on every stream, but per-stream
    exponential jitter and gaps.
  * Each thread times `register()` alone at the scheduled time.
  * p99.9 and below are stable between runs and seeds; p99.99 and above vary
    up to 2× and are order-of-magnitude.
* **Profiling.** `--targets`/`--loads`/`--reps` isolate one configuration
  for `perf record`/`annotate`. `ls_dmnd_fills_from_sys.local_ccx` (fills
  from L3 or another core's L2 in the same CCX; needs a `perf` with Zen 5
  events) measures cache-to-cache transfers. It stays near zero
  single-threaded, and all benchmark threads share one CCX.
* **Baseline.** `HashSetN`, in `benches/common/hashset.rs` (an `scc::HashSet` plus an `AtomicU64`
  watermark) gives the same answers when there are no races. It is not
  lock-free.
* **Hardware.** Results come from an AMD EPYC 9845 (Zen 5c); the Intel
  11th-gen target has not been measured. Results and analysis are in
  `BENCHMARKS.md`, `README.md` and `bench-results/`.
