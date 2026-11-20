//! 128-bit atomic header for x86-64 with `CX16`.
//!
//! Rust's `AtomicU128` is unstable, and the usual fallbacks compile a 16-byte
//! atomic into a runtime-library call that may take a lock, so the two
//! primitives we need are written directly in inline assembly. Both are
//! lock-free by construction: each is a single instruction with no loop and
//! no library call.
//!
//! The implementation is compiled only when the target enables `cmpxchg16b`
//! (e.g. `-C target-feature=+cmpxchg16b`, `-C target-cpu=x86-64-v2` or
//! `native`). Otherwise `new_workers` refuses every 128-bit mode with
//! `InitError::MissingTargetFeature`.
//!
//! * CAS: `LOCK CMPXCHG16B`. A locked RMW with full-barrier (SeqCst) semantics.
//! * Load, two strategies (benchmarked against each other):
//!   - `LOCK CMPXCHG16B` with `expected == desired == 0`. Architecturally
//!     guaranteed atomic on every CPU with `CX16`, but it is a write: it takes
//!     the line exclusive and serializes like any locked op.
//!   - `VMOVDQA` (16-byte aligned VEX load). Intel SDM Vol. 3A §9.1.1 and AMD
//!     APM Vol. 2 §7.3.2 guarantee that aligned 16-byte SSE/AVX loads are
//!     single-copy atomic on processors that enumerate AVX. A plain load on
//!     x86 TSO has acquire semantics.
//!
//! Neither asm block uses `nomem`/`readonly`, so each is also a compiler
//! barrier: the compiler cannot move other memory accesses across it.

use core::cell::UnsafeCell;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Pair {
    /// High watermark.
    pub lo: u64,
    /// Inline mask (`Last64`) or slot id (`Last128+`).
    pub hi: u64,
}

/// The header gets its own pair of cache lines (Intel's adjacent-line
/// prefetcher pulls lines in 128-byte pairs).
#[repr(C, align(128))]
pub(crate) struct Header {
    cell: UnsafeCell<[u64; 2]>,
}

// SAFETY: every access after construction goes through the atomic asm below.
unsafe impl Sync for Header {}
unsafe impl Send for Header {}

impl Header {
    pub fn new(v: Pair) -> Self {
        Header {
            cell: UnsafeCell::new([v.lo, v.hi]),
        }
    }

    /// `LOCK CMPXCHG16B`. On failure returns the value that was observed.
    #[inline(always)]
    pub fn cas(&self, current: Pair, new: Pair) -> Result<(), Pair> {
        imp::cas(self.cell.get() as *mut u64, current, new)
    }

    #[inline(always)]
    pub fn load_locked(&self) -> Pair {
        let z = Pair { lo: 0, hi: 0 };
        match self.cas(z, z) {
            Ok(()) => z,
            Err(v) => v,
        }
    }

    /// Caller must have checked that the CPU supports AVX.
    #[inline(always)]
    pub unsafe fn load_avx(&self) -> Pair {
        unsafe { imp::load_avx(self.cell.get() as *const u64) }
    }

    /// Caller must have checked AVX and PRFCHW.
    #[inline(always)]
    pub unsafe fn load_prefetchw_avx(&self) -> Pair {
        unsafe { imp::load_prefetchw_avx(self.cell.get() as *const u64) }
    }
}

#[cfg(all(target_arch = "x86_64", target_feature = "cmpxchg16b"))]
mod imp {
    use super::Pair;
    use core::arch::asm;

    #[inline(always)]
    pub fn cas(ptr: *mut u64, current: Pair, new: Pair) -> Result<(), Pair> {
        let prev_lo: u64;
        let prev_hi: u64;
        // SAFETY: `ptr` points to a live, 16-byte aligned `[u64; 2]` (the
        // `Header` is align(128)). LLVM reserves RBX, so the low half of the
        // new value is swapped into RBX around the instruction and restored.
        //
        // RBX hazard: a `reg` operand may itself be allocated to RBX, and the
        // swap/restore would clobber it. So no operand other than `rbx_tmp`
        // may be a free `reg`: the pointer is pinned to RDI, and success is
        // derived from the returned value instead of a `sete` output (an
        // output in BL was overwritten by the restore). Both bugs were found
        // by mutation testing; see tests::cas_register_allocation.
        //
        // CMPXCHG16B leaves RDX:RAX == expected on success and loads the
        // differing current value on failure, so `prev == current` <=> success.
        // Only compiled when the target enables cmpxchg16b.
        unsafe {
            asm!(
                "xchg {rbx_tmp}, rbx",
                "lock cmpxchg16b xmmword ptr [rdi]",
                "mov rbx, {rbx_tmp}",
                in("rdi") ptr,
                rbx_tmp = inout(reg) new.lo => _,
                inout("rax") current.lo => prev_lo,
                inout("rdx") current.hi => prev_hi,
                in("rcx") new.hi,
                options(nostack),
            );
        }
        let prev = Pair {
            lo: prev_lo,
            hi: prev_hi,
        };
        if prev == current { Ok(()) } else { Err(prev) }
    }

    #[inline(always)]
    pub unsafe fn load_avx(ptr: *const u64) -> Pair {
        let lo: u64;
        let hi: u64;
        // SAFETY: 16-byte aligned, live; caller checked AVX.
        unsafe {
            asm!(
                "vmovdqa {x}, xmmword ptr [{ptr}]",
                "vmovq {lo}, {x}",
                "vpextrq {hi}, {x}, 1",
                ptr = in(reg) ptr,
                x = out(xmm_reg) _,
                lo = out(reg) lo,
                hi = out(reg) hi,
                options(nostack, preserves_flags),
            );
        }
        Pair { lo, hi }
    }

    #[inline(always)]
    pub unsafe fn load_prefetchw_avx(ptr: *const u64) -> Pair {
        let lo: u64;
        let hi: u64;
        // SAFETY: as load_avx; caller checked PRFCHW.
        unsafe {
            asm!(
                "prefetchw byte ptr [{ptr}]",
                "vmovdqa {x}, xmmword ptr [{ptr}]",
                "vmovq {lo}, {x}",
                "vpextrq {hi}, {x}, 1",
                ptr = in(reg) ptr,
                x = out(xmm_reg) _,
                lo = out(reg) lo,
                hi = out(reg) hi,
                options(nostack, preserves_flags),
            );
        }
        Pair { lo, hi }
    }
}

#[cfg(not(all(target_arch = "x86_64", target_feature = "cmpxchg16b")))]
mod imp {
    use super::Pair;
    // new_workers refuses every 128-bit mode unless the target enables
    // cmpxchg16b, so these are never reached.
    pub fn cas(_: *mut u64, _: Pair, _: Pair) -> Result<(), Pair> {
        unreachable!("128-bit modes require target_feature = \"cmpxchg16b\"")
    }
    pub unsafe fn load_avx(_: *const u64) -> Pair {
        unreachable!("128-bit modes require target_feature = \"cmpxchg16b\"")
    }
    pub unsafe fn load_prefetchw_avx(_: *const u64) -> Pair {
        unreachable!("128-bit modes require target_feature = \"cmpxchg16b\"")
    }
}
