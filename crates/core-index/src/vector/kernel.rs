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
        pub(crate) fn learn(sample: &mut Vec<f32>) -> Self {
        sample.retain(|v| v.is_finite());
        sample.iter_mut().for_each(|v| *v = v.abs());
        if sample.is_empty() {
            return Self { absmax: 1.0 };
        }
        let rank = ((sample.len() - 1) as f64 * RANGE_QUANTILE).round() as usize;
        let (_, &mut value, _) = sample.select_nth_unstable_by(rank, f32::total_cmp);
        Self::from_absmax(value).unwrap_or(Self { absmax: 1.0 })
    }

    pub(crate) fn absmax(self) -> f32 {
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

fn encode_with_scale(values: &[f32], scale: f32, out: &mut Vec<u8>) {
    out.clear();
    out.extend(values.iter().map(|&v| {
        let code = if v.is_finite() { (v * scale).round().clamp(-CODE_MAX, CODE_MAX) } else { 0.0 };
        code as i8 as u8
    }));
}

/// Dot product of two int8 code vectors stored as bytes, in i32.
/// max |sum| = dims × 127², so it cannot overflow for dims < 130_000.
#[inline]
pub(crate) fn dot_i8(a: &[u8], b: &[u8]) -> i32 {
    debug_assert_eq!(a.len(), b.len(), "dot product of vectors with different dims");
    let mut lanes = [0i32; DOT_LANES];
    let a_chunks = a.chunks_exact(DOT_LANES);
    let b_chunks = b.chunks_exact(DOT_LANES);
    let tail: i32 = a_chunks
        .remainder()
        .iter()
        .zip(b_chunks.remainder())
        .map(|(&x, &y)| i32::from(x as i8) * i32::from(y as i8))
        .sum();
    for (x, y) in a_chunks.zip(b_chunks) {
        for lane in 0..DOT_LANES {
            lanes[lane] += i32::from(x[lane] as i8) * i32::from(y[lane] as i8);
        }
    }
    lanes.iter().sum::<i32>() + tail
}

/// Approximate cosine of a row and a query from their codes (both vectors L2-normalized).
#[inline]
pub(crate) fn approx_cosine(dot: i32, row_scale: f32, query_scale: f32) -> f32 {
    dot as f32 / (row_scale * query_scale)
}

/// Exact dot product of two f32 vectors (cosine when both are L2-normalized).
#[inline]
pub(crate) fn dot_f32(a: &[f32], b: &[f32]) -> f32 {
    debug_assert_eq!(a.len(), b.len(), "dot product of vectors with different dims");
    let mut lanes = [0f32; DOT_LANES];
    let a_chunks = a.chunks_exact(DOT_LANES);
    let b_chunks = b.chunks_exact(DOT_LANES);
    let tail: f32 = a_chunks
        .remainder()
        .iter()
        .zip(b_chunks.remainder())
        .map(|(x, y)| x * y)
        .sum();
    for (x, y) in a_chunks.zip(b_chunks) {
        for lane in 0..DOT_LANES {
            lanes[lane] += x[lane] * y[lane];
        }
    }
    lanes.iter().sum::<f32>() + tail
}
#[cfg(test)]
mod tests {
    use super::*;

    fn naive_i8(a: &[i8], b: &[i8]) -> i32 {
        a.iter().zip(b).map(|(&x, &y)| i32::from(x) * i32::from(y)).sum()
    }

    #[test]
    fn dot_i8_matches_naive_across_lane_boundaries() {
        for len in [1, 15, 16, 17, 33, 4096] {
            let a: Vec<i8> = (0..len).map(|i| ((i * 37) % 255) as i8).collect();
            let b: Vec<i8> = (0..len).map(|i| ((i * 91 + 7) % 255) as i8).collect();
            let bytes = |v: &[i8]| v.iter().map(|&x| x as u8).collect::<Vec<u8>>();
            assert_eq!(dot_i8(&bytes(&a), &bytes(&b)), naive_i8(&a, &b), "len {len}");
        }
    }

    #[test]
    fn dot_f32_matches_naive() {
        let a: Vec<f32> = (0..37).map(|i| i as f32 * 0.25).collect();
        let b: Vec<f32> = (0..37).map(|i| 1.0 - i as f32 * 0.5).collect();
        let naive: f32 = a.iter().zip(&b).map(|(x, y)| x * y).sum();
        assert!((dot_f32(&a, &b) - naive).abs() < 1e-3);
    }

    #[test]
    fn learned_range_ignores_outliers_and_nan() {
        let mut sample: Vec<f32> = (0..1000).map(|i| (i % 100) as f32 / 1000.0).collect();
        sample.push(50.0);
        sample.push(f32::NAN);
        let range = QuantRange::learn(&mut sample);
        assert!(range.absmax() < 0.1, "outlier set the range: {}", range.absmax());
    }

    #[test]
    fn empty_or_zero_sample_gets_unit_range() {
        assert_eq!(QuantRange::learn(&mut Vec::new()).absmax(), 1.0);
        assert_eq!(QuantRange::learn(&mut vec![0.0; 10]).absmax(), 1.0);
    }

    #[test]
    fn small_components_use_the_full_code_range() {
        // 4096-dim-sized components: a fixed 127 scale would map these to about ±2.
        let mut sample = vec![0.016f32, -0.016, 0.008, -0.004];
        let range = QuantRange::learn(&mut sample);
        let mut codes = Vec::new();
        range.encode(&[0.016, -0.016], &mut codes);
        let signed: Vec<i8> = codes.iter().map(|&c| c as i8).collect();
        assert_eq!(signed, vec![127, -127]);
    }

    #[test]
    fn approx_cosine_is_close_to_exact() {
        let row = [0.6f32, 0.8, 0.0];
        let query = [0.8f32, 0.6, 0.0];
        let range = QuantRange::from_absmax(0.8).expect("valid range");
        let mut row_codes = Vec::new();
        range.encode(&row, &mut row_codes);
        let q = quantize_query(&query);
        let approx = approx_cosine(dot_i8(&row_codes, &q.codes), range.scale(), q.scale);
        assert!((approx - dot_f32(&row, &query)).abs() < 0.01);
    }
}