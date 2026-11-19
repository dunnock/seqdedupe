//! Multi-word left shift of an immutable bitmap slot into a fresh slot.
//!
//! Every slot is preceded by `W` guard words that are zero forever. Word `j`
//! of `src << d` (with `q = d / 64`, `r = d % 64`) is
//! `src[j - q] << r | src[j - q - 1] >> (64 - r)`, and the guard turns every
//! negative index into a zero read. That removes all branches from the scalar
//! loop and lets the SIMD versions use plain unaligned loads at offset `-q`.
//!
//! All functions require `1 <= d < 64 * W`.

#[cfg(target_arch = "x86_64")]
use core::arch::x86_64::*;

pub(crate) trait Shifter {
    /// # Safety
    /// `src` has `W` readable zero words before it and `W` readable words at
    /// it; `dst` has `W` writable words; `1 <= d < 64 * W`.
    unsafe fn shl<const W: usize>(src: *const u64, dst: *mut u64, d: usize);

    /// `dst = (src << d) | 1 << (d - 1)`: the history after the watermark
    /// advances by `d` (the old watermark lands at bit `d - 1`).
    ///
    /// # Safety
    /// As for [`Shifter::shl`].
    #[inline(always)]
    unsafe fn advance<const W: usize>(src: *const u64, dst: *mut u64, d: usize) {
        unsafe {
            Self::shl::<W>(src, dst, d);
            *dst.add((d - 1) / 64) |= 1u64 << ((d - 1) % 64);
        }
    }
}

pub(crate) struct Scalar;

impl Shifter for Scalar {
    #[inline(always)]
    unsafe fn shl<const W: usize>(src: *const u64, dst: *mut u64, d: usize) {
        let q = (d / 64) as isize;
        let r = (d % 64) as u32;
        for j in 0..W as isize {
            // SAFETY: j - q - 1 >= -W, which lands in the guard.
            unsafe {
                let a = *src.offset(j - q);
                let b = *src.offset(j - q - 1);
                // (b >> 1) >> (63 - r) == b >> (64 - r), and 0 when r == 0.
                *dst.offset(j) = (a << r) | ((b >> 1) >> (63 - r));
            }
        }
    }
}

#[cfg(target_arch = "x86_64")]
pub(crate) struct Avx2;

#[cfg(target_arch = "x86_64")]
impl Shifter for Avx2 {
    #[inline(always)]
    unsafe fn shl<const W: usize>(src: *const u64, dst: *mut u64, d: usize) {
        unsafe { shl_avx2::<W>(src, dst, d) }
    }
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
#[inline]
unsafe fn shl_avx2<const W: usize>(src: *const u64, dst: *mut u64, d: usize) {
    const { assert!(W.is_multiple_of(4)) };
    let q = (d / 64) as isize;
    let r = (d % 64) as i64;
    let cl = _mm_cvtsi64_si128(r);
    // srl by 64 yields zero, which is exactly the r == 0 carry.
    let cr = _mm_cvtsi64_si128(64 - r);
    let mut j = 0isize;
    while j < W as isize {
        unsafe {
            let a = _mm256_loadu_si256(src.offset(j - q) as *const __m256i);
            let b = _mm256_loadu_si256(src.offset(j - q - 1) as *const __m256i);
            let v = _mm256_or_si256(_mm256_sll_epi64(a, cl), _mm256_srl_epi64(b, cr));
            _mm256_storeu_si256(dst.offset(j) as *mut __m256i, v);
        }
        j += 4;
    }
}

#[cfg(target_arch = "x86_64")]
pub(crate) struct Avx512;

#[cfg(target_arch = "x86_64")]
impl Shifter for Avx512 {
    #[inline(always)]
    unsafe fn shl<const W: usize>(src: *const u64, dst: *mut u64, d: usize) {
        unsafe { shl_avx512::<W>(src, dst, d) }
    }
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx512f")]
#[inline]
unsafe fn shl_avx512<const W: usize>(src: *const u64, dst: *mut u64, d: usize) {
    const { assert!(W.is_multiple_of(8)) };
    let q = (d / 64) as isize;
    let r = (d % 64) as i64;
    let cl = _mm_cvtsi64_si128(r);
    let cr = _mm_cvtsi64_si128(64 - r);
    let mut j = 0isize;
    while j < W as isize {
        unsafe {
            let a = _mm512_loadu_si512(src.offset(j - q) as *const _);
            let b = _mm512_loadu_si512(src.offset(j - q - 1) as *const _);
            let v = _mm512_or_si512(_mm512_sll_epi64(a, cl), _mm512_srl_epi64(b, cr));
            _mm512_storeu_si512(dst.offset(j) as *mut _, v);
        }
        j += 8;
    }
}

/// AVX-512 VBMI2 (Ice Lake / Tiger Lake / Rocket Lake, Zen 4+). The whole
/// history lives in one register (zmm for `W = 8`, ymm for `W = 4`):
///
/// * `VPERMQ` with a zero mask moves word `j - q` into lane `j` (word shift);
/// * `VALIGNQ` with zero gives the same vector moved up one more lane;
/// * `VPSHLDVQ` is the per-lane funnel shift `hi << r | lo >> (64 - r)`
///   (and `hi` when `r == 0`);
/// * the old-watermark bit is OR-ed in register, then one store.
///
/// No guard words are read: one aligned load, one store.
#[cfg(target_arch = "x86_64")]
pub(crate) struct Vbmi2;

#[cfg(target_arch = "x86_64")]
impl Shifter for Vbmi2 {
    #[inline(always)]
    unsafe fn shl<const W: usize>(src: *const u64, dst: *mut u64, d: usize) {
        // Only `advance` is specialised; the generic shift is never needed on
        // its own, but keep it correct.
        unsafe { Avx2::shl::<W>(src, dst, d) }
    }

    #[inline(always)]
    unsafe fn advance<const W: usize>(src: *const u64, dst: *mut u64, d: usize) {
        unsafe {
            match W {
                8 => advance_vbmi2_512(src, dst, d),
                4 => advance_vbmi2_256(src, dst, d),
                _ => {
                    Scalar::shl::<W>(src, dst, d);
                    *dst.add((d - 1) / 64) |= 1u64 << ((d - 1) % 64);
                }
            }
        }
    }
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx512f,avx512vbmi2")]
#[inline]
unsafe fn advance_vbmi2_512(src: *const u64, dst: *mut u64, d: usize) {
    let a = unsafe { _mm512_loadu_si512(src as *const _) };
    let v = if d == 1 {
        let lo = _mm512_alignr_epi64::<7>(a, _mm512_setzero_si512());
        _mm512_shldi_epi64::<1>(a, lo)
    } else {
        let q = (d / 64) as i64;
        let r = (d % 64) as i64;
        let idx = _mm512_sub_epi64(
            _mm512_set_epi64(7, 6, 5, 4, 3, 2, 1, 0),
            _mm512_set1_epi64(q),
        );
        let hi = _mm512_maskz_permutexvar_epi64((0xffu32 << q) as u8, idx, a);
        let lo = _mm512_alignr_epi64::<7>(hi, _mm512_setzero_si512());
        _mm512_shldv_epi64(hi, lo, _mm512_set1_epi64(r))
    };
    let k = d - 1;
    let bit = _mm512_maskz_set1_epi64(1u8 << (k / 64), (1u64 << (k % 64)) as i64);
    unsafe { _mm512_storeu_si512(dst as *mut _, _mm512_or_si512(v, bit)) };
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx512f,avx512vl,avx512vbmi2")]
#[inline]
unsafe fn advance_vbmi2_256(src: *const u64, dst: *mut u64, d: usize) {
    let a = unsafe { _mm256_loadu_si256(src as *const __m256i) };
    let v = if d == 1 {
        let lo = _mm256_alignr_epi64::<3>(a, _mm256_setzero_si256());
        _mm256_shldi_epi64::<1>(a, lo)
    } else {
        let q = (d / 64) as i64;
        let r = (d % 64) as i64;
        let idx = _mm256_sub_epi64(_mm256_set_epi64x(3, 2, 1, 0), _mm256_set1_epi64x(q));
        let hi = _mm256_maskz_permutexvar_epi64((0xfu32 << q) as u8 & 0xf, idx, a);
        let lo = _mm256_alignr_epi64::<3>(hi, _mm256_setzero_si256());
        _mm256_shldv_epi64(hi, lo, _mm256_set1_epi64x(r))
    };
    let k = d - 1;
    let bit = _mm256_maskz_set1_epi64(1u8 << (k / 64), (1u64 << (k % 64)) as i64);
    unsafe { _mm256_storeu_si256(dst as *mut __m256i, _mm256_or_si256(v, bit)) };
}
