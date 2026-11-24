//! Vector math shared by the vector file, the HNSW graph and the search entry point.
//!
//! Rows are int8 codes with one learned scale per file; a query gets its own scale. The hot
//! path is an int8 dot product; f32 originals are used to rescore the final candidates and
//! to build graphs.

/// Largest int8 code; codes are symmetric in -127..=127 so zero is exact and negation is free.
pub(crate) const CODE_MAX: f32 = 127.0;
/// Fraction of |components| a file's range must cover; the rest is clipped.
pub(crate) const RANGE_QUANTILE: f64 = 0.99;
/// Independent accumulators so the compiler can keep several SIMD lanes busy.
const DOT_LANES: usize = 16;

/// Learned range of one file: `absmax` maps to code 127.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct QuantRange {
    absmax: f32,
}

impl QuantRange {
    /// Range from a stored `absmax`; `None` if it is not a positive finite number.
    pub(crate) fn from_absmax(absmax: f32) -> Option<Self> {
        (absmax.is_finite() && absmax > 0.0).then_some(Self { absmax })
    }

    /// Learns the range from a sample of components (reordered in place).
    /// An empty or all-zero sample gets range 1.0: any range encodes zeros exactly.
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "the sample holds at most about a million values and `rank` is within 0..len"
    )]
    pub(crate) fn learn(sample: &mut Vec<f32>) -> Self {
        sample.retain(|v| v.is_finite());
        for v in sample.iter_mut() {
            *v = v.abs();
        }
        if sample.is_empty() {
            return Self { absmax: 1.0 };
        }
        let rank = (((sample.len() - 1) as f64) * RANGE_QUANTILE).round() as usize;
        let (_, &mut value, _) = sample.select_nth_unstable_by(rank, f32::total_cmp);
        Self::from_absmax(value).unwrap_or(Self { absmax: 1.0 })
    }

    pub(crate) const fn absmax(self) -> f32 {
        self.absmax
    }

    /// Codes per unit of value.
    pub(crate) fn scale(self) -> f32 {
        CODE_MAX / self.absmax
    }

    /// Encodes `values` into `out` (cleared first), clipping values outside the range.
    pub(crate) fn encode(self, values: &[f32], out: &mut Vec<u8>) {
        encode_with_scale(values, self.scale(), out);
    }
}

/// A quantised query and its own scale.
#[derive(Debug, Clone)]
pub(crate) struct QueryCodes {
    pub(crate) codes: Vec<u8>,
    pub(crate) scale: f32,
}

/// Quantises a query with its own largest |component|, so it always uses the full code range.
pub(crate) fn quantize_query(query: &[f32]) -> QueryCodes {
    let absmax = query
        .iter()
        .filter(|v| v.is_finite())
        .fold(0.0f32, |max, v| max.max(v.abs()));
    let scale = if absmax > 0.0 { CODE_MAX / absmax } else { 1.0 };
    let mut codes = Vec::with_capacity(query.len());
    encode_with_scale(query, scale, &mut codes);
    QueryCodes { codes, scale }
}

#[allow(
    clippy::cast_possible_truncation,
    reason = "the value is rounded and clamped to -127..=127 first, so it fits an i8"
)]
fn encode_with_scale(values: &[f32], scale: f32, out: &mut Vec<u8>) {
    out.clear();
    out.extend(
        values.iter().map(|&v| {
            let code = if v.is_finite() {
                (v * scale).round().clamp(-CODE_MAX, CODE_MAX)
            } else {
                0.0
            };
            (code as i8).cast_unsigned()
        })
    );
}

/// Dot product of two int8 code vectors stored as bytes, in i32.
/// max |sum| = dims × 127², so it cannot overflow for dims < `130_000`.
#[inline]
pub(crate) fn dot_i8(a: &[u8], b: &[u8]) -> i32 {
    debug_assert_eq!(a.len(), b.len(), "dot product of vectors with different dims");
    #[cfg(target_arch = "x86_64")]
    {
        if
            is_x86_feature_detected!("avx512f") &&
            is_x86_feature_detected!("avx512bw") &&
            is_x86_feature_detected!("avx512vnni")
        {
            // SAFETY: every feature the kernel enables was just detected.
            return unsafe {
                x86::dot_i8_avx512_vnni(a, b)
            };
        }
        if is_x86_feature_detected!("avx2") {
            // SAFETY: AVX2 was just detected.
            return unsafe {
                x86::dot_i8_avx2(a, b)
            };
        }
    }
    #[cfg(all(target_arch = "aarch64", target_feature = "neon"))]
    {
        // SAFETY: this block is compiled only with target_feature = "neon" enabled.
        return unsafe {
            neon::dot_i8(a, b)
        };
    }
    #[allow(unreachable_code)]
    dot_i8_portable(a, b)
}

/// Portable int8 dot product: independent accumulators the compiler can vectorise.
/// The SIMD kernels also use it for the bytes left over after their last full block.
fn dot_i8_portable(a: &[u8], b: &[u8]) -> i32 {
    let mut lanes = [0i32; DOT_LANES];
    let (a_blocks, a_rest) = a.as_chunks::<DOT_LANES>();
    let (b_blocks, b_rest) = b.as_chunks::<DOT_LANES>();
    let tail: i32 = a_rest
        .iter()
        .zip(b_rest)
        .map(|(&x, &y)| i32::from(x.cast_signed()) * i32::from(y.cast_signed()))
        .sum();
    for (x, y) in a_blocks.iter().zip(b_blocks) {
        for ((lane, &xv), &yv) in lanes.iter_mut().zip(x).zip(y) {
            *lane += i32::from(xv.cast_signed()) * i32::from(yv.cast_signed());
        }
    }
    lanes.iter().sum::<i32>() + tail
}

/// Approximate cosine of a row and a query from their codes (both vectors L2-normalized).
#[inline]
#[allow(
    clippy::cast_precision_loss,
    reason = "the dot product only ranks candidates; f32 rounding past 2^24 is harmless"
)]
pub(crate) fn approx_cosine(dot: i32, row_scale: f32, query_scale: f32) -> f32 {
    (dot as f32) / (row_scale * query_scale)
}
/// Exact dot product of two f32 vectors (cosine when both are L2-normalized).
///
/// Dispatches like [`dot_i8`]: AVX2 with FMA on x86-64, NEON on aarch64, the portable loop
/// everywhere else. The SIMD kernels add in a different order and use fused multiply-add, so
/// the last bits of a score can differ between CPUs. A process always uses the same kernel,
/// so its own scores are repeatable.
#[inline]
pub(crate) fn dot_f32(a: &[f32], b: &[f32]) -> f32 {
    debug_assert_eq!(a.len(), b.len(), "dot product of vectors with different dims");
    #[cfg(target_arch = "x86_64")]
    {
        if is_x86_feature_detected!("avx2") && is_x86_feature_detected!("fma") {
            // SAFETY: AVX2 and FMA were just detected.
            return unsafe {
                x86::dot_f32_avx2(a, b)
            };
        }
    }
    #[cfg(all(target_arch = "aarch64", target_feature = "neon"))]
    {
        return unsafe { neon::dot_f32(a, b) };
    }
    #[allow(unreachable_code)]
    dot_f32_portable(a, b)
}

/// Portable f32 dot product: independent accumulators the compiler can vectorise.
/// The SIMD kernels also use it for the values left over after their last full block.
fn dot_f32_portable(a: &[f32], b: &[f32]) -> f32 {
    let mut lanes = [0f32; DOT_LANES];
    let (a_blocks, a_rest) = a.as_chunks::<DOT_LANES>();
    let (b_blocks, b_rest) = b.as_chunks::<DOT_LANES>();
    let tail: f32 = a_rest
        .iter()
        .zip(b_rest)
        .map(|(x, y)| x * y)
        .sum();
    for (x, y) in a_blocks.iter().zip(b_blocks) {
        for ((lane, xv), yv) in lanes.iter_mut().zip(x).zip(y) {
            *lane += xv * yv;
        }
    }
    lanes.iter().sum::<f32>() + tail
}

/// x86-64 kernels. Each needs CPU features the baseline build doesn't assume, so each is
/// `unsafe` to call, and the dispatchers call it only after detecting those features.
#[cfg(target_arch = "x86_64")]
mod x86 {
    use super::{ dot_f32_portable, dot_i8_portable };
    use std::arch::x86_64::{
        __m256,
        __m256i,
        __m512i,
        __mmask64,
        _mm_add_epi32,
        _mm_add_ps,
        _mm_add_ss,
        _mm_cvtsi128_si32,
        _mm_cvtss_f32,
        _mm_movehl_ps,
        _mm_shuffle_epi32,
        _mm_shuffle_ps,
        _mm256_add_epi32,
        _mm256_add_ps,
        _mm256_castps256_ps128,
        _mm256_castsi256_si128,
        _mm256_extractf128_ps,
        _mm256_extracti128_si256,
        _mm256_fmadd_ps,
        _mm256_loadu_ps,
        _mm256_loadu_si256,
        _mm256_madd_epi16,
        _mm256_maddubs_epi16,
        _mm256_set1_epi16,
        _mm256_setzero_ps,
        _mm256_setzero_si256,
        _mm256_sign_epi8,
        _mm512_abs_epi8,
        _mm512_add_epi32,
        _mm512_dpbusd_epi32,
        _mm512_loadu_si512,
        _mm512_mask_sub_epi8,
        _mm512_maskz_loadu_epi8,
        _mm512_movepi8_mask,
        _mm512_reduce_add_epi32,
        _mm512_setzero_si512,
    };

    #[target_feature(enable = "avx2")]
    pub(super) unsafe fn dot_i8_avx2(a: &[u8], b: &[u8]) -> i32 {
        let a_chunks = a.chunks_exact(32);
        let b_chunks = b.chunks_exact(32);
        let tail = dot_i8_portable(a_chunks.remainder(), b_chunks.remainder());
        let ones = _mm256_set1_epi16(1);
        let mut acc = _mm256_setzero_si256();
        for (x, y) in a_chunks.zip(b_chunks) {
            // SAFETY: each chunk is exactly 32 bytes, and `loadu` accepts unaligned pointers.
            let (xv, yv) = unsafe {
                (_mm256_loadu_si256(x.as_ptr().cast()), _mm256_loadu_si256(y.as_ptr().cast()))
            };
            let pairs = _mm256_maddubs_epi16(_mm256_sign_epi8(xv, xv), _mm256_sign_epi8(yv, xv));
            acc = _mm256_add_epi32(acc, _mm256_madd_epi16(pairs, ones));
        }
        hsum_epi32(acc) + tail
    }

    /// int8 dot product, 64 bytes per step, using AVX-512 VNNI (`vpdpbusd`).
    ///
    /// Like [`dot_i8_avx2`], it multiplies unsigned by signed bytes, so the signs of `a` move
    /// onto `b`. The last partial block uses a masked load, so there is no scalar tail.
    ///
    /// # Safety
    /// The CPU must support AVX-512F, AVX-512BW and AVX-512 VNNI.
    #[target_feature(enable = "avx512f,avx512bw,avx512vnni")]
    pub(super) unsafe fn dot_i8_avx512_vnni(a: &[u8], b: &[u8]) -> i32 {
        let a_chunks = a.chunks_exact(64);
        let b_chunks = b.chunks_exact(64);
        let a_rest = a_chunks.remainder();
        let b_rest = b_chunks.remainder();
        let mut acc = _mm512_setzero_si512();
        for (x, y) in a_chunks.zip(b_chunks) {
            // SAFETY: each chunk is exactly 64 bytes, and `loadu` accepts unaligned pointers.
            let (xv, yv) = unsafe {
                (_mm512_loadu_si512(x.as_ptr().cast()), _mm512_loadu_si512(y.as_ptr().cast()))
            };
            acc = _mm512_add_epi32(acc, vnni_products(xv, yv));
        }
        let rest = a_rest.len().min(b_rest.len());
        if rest > 0 {
            // `rest < 64`, so the shift cannot overflow. Masked-off bytes load as zero.
            let mask: __mmask64 = (1u64 << rest) - 1;
            // SAFETY: the mask selects only the first `rest` bytes, which both slices hold, and a
            // masked load never touches the bytes it leaves out.
            let (xv, yv) = unsafe {
                (
                    _mm512_maskz_loadu_epi8(mask, a_rest.as_ptr().cast()),
                    _mm512_maskz_loadu_epi8(mask, b_rest.as_ptr().cast()),
                )
            };
            acc = _mm512_add_epi32(acc, vnni_products(xv, yv));
        }
        _mm512_reduce_add_epi32(acc)
    }

    /// Per 32-bit lane, the sum of four `a × b` byte products, with the signs of `a` moved
    /// onto `b`. It starts from zero instead of the running total, so the slow `vpdpbusd`
    /// stays off the loop's dependency chain.
    #[target_feature(enable = "avx512f,avx512bw,avx512vnni")]
    fn vnni_products(xv: __m512i, yv: __m512i) -> __m512i {
        let x_negative = _mm512_movepi8_mask(xv);
        let y_signed = _mm512_mask_sub_epi8(yv, x_negative, _mm512_setzero_si512(), yv);
        _mm512_dpbusd_epi32(_mm512_setzero_si512(), _mm512_abs_epi8(xv), y_signed)
    }

    /// f32 dot product, 32 floats per step across four fused multiply-add accumulators, so
    /// one step doesn't wait for the previous one.
    ///
    /// # Safety
    /// The CPU must support AVX2 and FMA.
    #[target_feature(enable = "avx2,fma")]
    pub(super) unsafe fn dot_f32_avx2(a: &[f32], b: &[f32]) -> f32 {
        let a_blocks = a.chunks_exact(32);
        let b_blocks = b.chunks_exact(32);
        let a_rest = a_blocks.remainder();
        let b_rest = b_blocks.remainder();
        let mut acc = [_mm256_setzero_ps(); 4];
        for (x, y) in a_blocks.zip(b_blocks) {
            for (part, slot) in acc.iter_mut().enumerate() {
                // SAFETY: each block holds 32 floats and `part * 8 + 8 <= 32`.
                let (xv, yv) = unsafe {
                    (
                        _mm256_loadu_ps(x.as_ptr().add(part * 8)),
                        _mm256_loadu_ps(y.as_ptr().add(part * 8)),
                    )
                };
                *slot = _mm256_fmadd_ps(xv, yv, *slot);
            }
        }
        let a_vecs = a_rest.chunks_exact(8);
        let b_vecs = b_rest.chunks_exact(8);
        let tail = dot_f32_portable(a_vecs.remainder(), b_vecs.remainder());
        for (x, y) in a_vecs.zip(b_vecs) {
            // SAFETY: each chunk holds exactly 8 floats.
            let (xv, yv) = unsafe { (_mm256_loadu_ps(x.as_ptr()), _mm256_loadu_ps(y.as_ptr())) };
            acc[0] = _mm256_fmadd_ps(xv, yv, acc[0]);
        }
        let sum = _mm256_add_ps(_mm256_add_ps(acc[0], acc[1]), _mm256_add_ps(acc[2], acc[3]));
        hsum_ps(sum) + tail
    }

    /// Sum of the eight i32 lanes.
    #[target_feature(enable = "avx2")]
    fn hsum_epi32(v: __m256i) -> i32 {
        let quad = _mm_add_epi32(_mm256_castsi256_si128(v), _mm256_extracti128_si256::<1>(v));
        let pair = _mm_add_epi32(quad, _mm_shuffle_epi32::<0b0100_1110>(quad));
        let one = _mm_add_epi32(pair, _mm_shuffle_epi32::<0b1011_0001>(pair));
        _mm_cvtsi128_si32(one)
    }

    /// Sum of the eight f32 lanes.
    #[target_feature(enable = "avx2")]
    fn hsum_ps(v: __m256) -> f32 {
        let quad = _mm_add_ps(_mm256_castps256_ps128(v), _mm256_extractf128_ps::<1>(v));
        let pair = _mm_add_ps(quad, _mm_movehl_ps(quad, quad));
        let one = _mm_add_ss(pair, _mm_shuffle_ps::<0b01>(pair, pair));
        _mm_cvtss_f32(one)
    }
}

/// NEON kernels. NEON is on for every aarch64 target this server builds for, so these are
/// ordinary safe functions.
#[cfg(all(target_arch = "aarch64", target_feature = "neon"))]
mod neon {
    use super::{ dot_f32_portable, dot_i8_portable };
    use std::arch::aarch64::{
        vaddq_f32,
        vaddq_s32,
        vaddvq_f32,
        vaddvq_s32,
        vdupq_n_f32,
        vdupq_n_s32,
        vfmaq_f32,
        vget_low_s8,
        vld1q_f32,
        vld1q_s8,
        vmull_high_s8,
        vmull_s8,
        vpadalq_s16,
    };

    /// int8 dot product, 16 bytes per step: widening multiply to i16, then pairwise add into
    /// i32. Exact for every input. The low and high halves use separate accumulators so the
    /// additions don't wait on each other.
    #[target_feature(enable = "neon")]
    pub(super) unsafe fn dot_i8(a: &[u8], b: &[u8]) -> i32 {
        let (a_blocks, a_rest) = a.as_chunks::<16>();
        let (b_blocks, b_rest) = b.as_chunks::<16>();
        let tail = dot_i8_portable(a_rest, b_rest);
        let mut acc_low = vdupq_n_s32(0);
        let mut acc_high = vdupq_n_s32(0);
        for (x, y) in a_blocks.iter().zip(b_blocks) {
            // SAFETY: each block is exactly 16 bytes.
            let xv = unsafe { vld1q_s8(x.as_ptr().cast()) };
            let yv = unsafe { vld1q_s8(y.as_ptr().cast()) };
            acc_low = vpadalq_s16(acc_low, vmull_s8(vget_low_s8(xv), vget_low_s8(yv)));
            acc_high = vpadalq_s16(acc_high, vmull_high_s8(xv, yv));
        }
        vaddvq_s32(vaddq_s32(acc_low, acc_high)) + tail
    }

    /// f32 dot product, 16 floats per step across four fused multiply-add accumulators
    /// /// The CPU must support NEON, which every aarch64 CPU does.
    /// f32 dot product, 16 floats per step across four fused multiply-add accumulators.
    #[target_feature(enable = "neon")]
    pub(super) fn dot_f32(a: &[f32], b: &[f32]) -> f32 {
        let (a_blocks, a_rest) = a.as_chunks::<16>();
        let (b_blocks, b_rest) = b.as_chunks::<16>();
        let mut acc = [vdupq_n_f32(0.0); 4];
        for (x, y) in a_blocks.iter().zip(b_blocks) {
            for (part, slot) in acc.iter_mut().enumerate() {
                // SAFETY: each block holds 16 floats and `part * 4 + 4 <= 16`.
                let x_ptr = unsafe { x.as_ptr().add(part * 4) };
                let y_ptr = unsafe { y.as_ptr().add(part * 4) };
                let xv = unsafe { vld1q_f32(x_ptr) };
                let yv = unsafe { vld1q_f32(y_ptr) };
                *slot = vfmaq_f32(*slot, xv, yv);
            }
        }
        let (a_vecs, a_tail) = a_rest.as_chunks::<4>();
        let (b_vecs, b_tail) = b_rest.as_chunks::<4>();
        let tail = dot_f32_portable(a_tail, b_tail);
        for (x, y) in a_vecs.iter().zip(b_vecs) {
            // SAFETY: each chunk holds exactly 4 floats.
            let xv = unsafe { vld1q_f32(x.as_ptr()) };
            let yv = unsafe { vld1q_f32(y.as_ptr()) };
            acc[0] = vfmaq_f32(acc[0], xv, yv);
        }
        let sum = vaddq_f32(vaddq_f32(acc[0], acc[1]), vaddq_f32(acc[2], acc[3]));
        vaddvq_f32(sum) + tail
    }
}
