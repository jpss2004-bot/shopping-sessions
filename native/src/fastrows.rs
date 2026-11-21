//! Classify a whole short row before extracting its fields. The dialect makes
//! every comma structural, even when a malformed row contains unmatched quotes.
use crate::scan::scalar_rows;
use memchr::memchr_iter;

#[inline(always)]
fn byte_mask(bytes: u64, needle: u8) -> u8 {
    const LOW: u64 = 0x7f7f_7f7f_7f7f_7f7f;
    let x = bytes ^ (u64::from(needle) * 0x0101_0101_0101_0101);
    // Each high bit is set exactly when its original byte was zero. Mask the
    // low bits before addition so an overflowing lane cannot affect its neighbor.
    let high = !(((x & LOW) + LOW) | x | LOW);
    ((high >> 7).wrapping_mul(0x0102_0408_1020_4080) >> 56) as u8
}

#[inline(always)]
fn portable_masks(block: &[u8; 32]) -> (u32, u32) {
    let mut commas = 0;
    let mut quotes = 0;
    for (i, bytes) in block.chunks_exact(8).enumerate() {
        let word = u64::from_le_bytes(bytes.try_into().unwrap());
        commas |= u32::from(byte_mask(word, b',')) << (8 * i);
        quotes |= u32::from(byte_mask(word, b'"')) << (8 * i);
    }
    (commas, quotes)
}

#[cfg(target_arch = "x86_64")]
#[inline(always)]
unsafe fn intel_mask_words(block: &[u8; 32]) -> (u32, u32) {
    use std::arch::x86_64::*;
    // The reference owns all 32 readable bytes. Callers checked AVX2 support.
    unsafe {
        let data = _mm256_loadu_si256(block.as_ptr().cast());
        let commas = _mm256_cmpeq_epi8(data, _mm256_set1_epi8(b',' as i8));
        let quotes = _mm256_cmpeq_epi8(data, _mm256_set1_epi8(b'"' as i8));
        (
            _mm256_movemask_epi8(commas) as u32,
            _mm256_movemask_epi8(quotes) as u32,
        )
    }
}

#[inline(always)]
pub(super) fn fields<'a, const N: usize>(
    line: &'a [u8],
    mut commas: u128,
    quotes: u128,
) -> Option<[&'a [u8]; N]> {
    if N == 0 || commas.count_ones() as usize != N - 1 {
        return None;
    }
    let mut start = 0;
    if quotes != 0 {
        // The sentinel terminates the last field just like a comma. An
        // opening quote subtracts from the first delimiter to its right.
        // Each field has its own delimiter, so the borrows cannot overlap.
        let delimiters = commas | (1u128 << line.len());
        let opens = quotes & ((commas << 1) | 1);
        let closes = quotes & (delimiters >> 1);
        let borrowed = delimiters & !(delimiters.wrapping_sub(opens));
        if (opens | closes) != quotes || opens & closes != 0 || borrowed != (closes << 1) {
            return None;
        }
    }
    let mut output = [&line[..0]; N];
    for (i, field) in output.iter_mut().enumerate() {
        let end = if i + 1 == N {
            line.len()
        } else {
            let index = commas.trailing_zeros() as usize;
            commas &= commas - 1;
            index
        };
        let raw = &line[start..end];
        let trim = usize::from(quotes != 0 && raw.first() == Some(&b'"'));
        *field = &raw[trim..raw.len() - trim];
        start = end + 1;
    }
    Some(output)
}

#[inline(always)]
fn scan<const N: usize>(
    chunk: &[u8],
    mut visit: impl FnMut([&[u8]; N]),
    masks: impl Fn(&[u8; 32]) -> (u32, u32),
) {
    let mut start = 0;
    for end in memchr_iter(b'\n', chunk).chain(std::iter::once(chunk.len())) {
        if start >= end {
            start = end + 1;
            continue;
        }
        let line = &chunk[start..end];
        start = end + 1;
        if line.len() < 32 || line.len() > 127 {
            scalar_rows(line, &mut visit);
            continue;
        }
        let last = line.len() - 32;
        let (c0, q0) = masks(line[..32].try_into().unwrap());
        let (c1, q1) = masks(line[last..].try_into().unwrap());
        // Overlapping edge loads mark the same bits, which OR combines exactly.
        let mut commas = u128::from(c0) | (u128::from(c1) << last);
        let mut quotes = u128::from(q0) | (u128::from(q1) << last);
        if last > 32 {
            let (c, q) = masks(line[32..64].try_into().unwrap());
            commas |= u128::from(c) << 32;
            quotes |= u128::from(q) << 32;
        }
        if last > 64 {
            let (c, q) = masks(line[64..96].try_into().unwrap());
            commas |= u128::from(c) << 64;
            quotes |= u128::from(q) << 64;
        }
        if let Some(row) = fields(line, commas, quotes) {
            visit(row);
        }
    }
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn intel_rows<const N: usize>(chunk: &[u8], visit: impl FnMut([&[u8]; N])) {
    scan(chunk, visit, |block| unsafe { intel_mask_words(block) });
}

// Keep dispatch outside the row loop. Inlining scan and fields here makes
// delimiter extraction, quote validation and field materialization eligible
// for BMI / POPCNT as well as the vector comparisons in intel_mask_words.
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2,bmi1,bmi2,popcnt")]
pub(super) unsafe fn intel_rows_tuned<const N: usize>(chunk: &[u8], visit: impl FnMut([&[u8]; N])) {
    scan(chunk, visit, |block| unsafe { intel_mask_words(block) });
}

pub fn rows<const N: usize>(chunk: &[u8], visit: impl FnMut([&[u8]; N])) {
    #[cfg(target_arch = "x86_64")]
    if std::is_x86_feature_detected!("avx2") {
        return unsafe { intel_rows(chunk, visit) };
    }
    scan(chunk, visit, portable_masks);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check<const N: usize>(chunk: &[u8]) {
        let mut reference = vec![];
        scalar_rows(chunk, |fields: [&[u8]; N]| {
            reference.push(fields.map(<[u8]>::to_vec))
        });
        let mut portable = vec![];
        scan(
            chunk,
            |fields: [&[u8]; N]| portable.push(fields.map(<[u8]>::to_vec)),
            portable_masks,
        );
        assert_eq!(reference, portable);
        let mut selected = vec![];
        rows(chunk, |fields: [&[u8]; N]| {
            selected.push(fields.map(<[u8]>::to_vec))
        });
        assert_eq!(reference, selected);
    }

    #[test]
    fn every_structural_bit_and_byte_value() {
        for needle in [b',', b'"'] {
            for byte in 0..=255 {
                for position in 0..8 {
                    let mut block = [needle; 8];
                    block[position] = byte;
                    let expected = if byte == needle {
                        255
                    } else {
                        255 ^ (1 << position)
                    };
                    assert_eq!(byte_mask(u64::from_le_bytes(block), needle), expected);
                }
            }
        }
    }

    #[test]
    fn quotes_and_separators_cross_all_block_boundaries() {
        for n in 0..=135 {
            let mut data = vec![b'x'; n];
            data.extend_from_slice(b",b,\\\"c\\\",,,\\\"v\\0\\r\\\",\\\"\\\"\n");
            data.extend_from_slice(&vec![b'q'; n]);
            data.extend_from_slice(b",\\\"bad\\\"x,a,b,c,d,e\n");
            data.extend_from_slice(&vec![b' '; n]);
            data.extend_from_slice(b",u,1234567890123,2028-02-29T23:59:59Z,cart,p1,");
            check::<7>(&data);
        }
    }

    #[test]
    fn all_short_quote_and_delimiter_arrangements() {
        // The prefix takes each case through the vector-sized row path.
        let mut chunk = vec![];
        for mut pattern in 0..3usize.pow(8) {
            chunk.extend_from_slice(&[b'x'; 32]);
            for _ in 0..8 {
                chunk.push(b"p,\""[pattern % 3]);
                pattern /= 3;
            }
            chunk.push(b'\n');
        }
        check::<1>(&chunk);
        check::<2>(&chunk);
        check::<4>(&chunk);
        check::<7>(&chunk);
    }

    #[test]
    fn generated_chunks_equal_scalar() {
        let mut state = 0xadcc_5711_9037_fed0_u64;
        let mut byte = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state as u8
        };
        for n in 0..200 {
            let mut chunk = Vec::new();
            for _ in 0..8 {
                for _ in 0..n {
                    let v = byte();
                    chunk.push(match v % 24 {
                        0 => b',',
                        1 => b'"',
                        2 => b'\r',
                        3 => 0,
                        _ => v,
                    });
                }
                chunk.push(b'\n');
            }
            check::<2>(&chunk);
            check::<4>(&chunk);
            check::<7>(&chunk);
        }
    }
}
