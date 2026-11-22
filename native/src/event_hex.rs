//! A lossless representation for literal e followed by 32 lowercase hex digits.
#[inline]
pub(crate) fn pack(bytes: &[u8]) -> Option<(u64, u64)> {
    if bytes.len() != 33 || bytes[0] != b'e' {
        return None;
    }
    #[cfg(target_arch = "x86_64")]
    if std::is_x86_feature_detected!("ssse3") {
        // Length and prefix were checked above; this reads exactly32 payload bytes.
        return unsafe { simd(bytes) };
    }
    scalar(bytes)
}
#[inline]
fn scalar(bytes: &[u8]) -> Option<(u64, u64)> {
    if bytes.len() != 33 || bytes[0] != b'e' {
        return None;
    }
    let mut decoded = [0u8; 16];
    let nibble = |c: u8| match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        _ => None,
    };
    for (i, out) in decoded.iter_mut().enumerate() {
        *out = (nibble(bytes[1 + i * 2])? << 4) | nibble(bytes[2 + i * 2])?;
    }
    let first = u64::from_le_bytes(decoded[..8].try_into().unwrap());
    let second = u64::from_le_bytes(decoded[8..].try_into().unwrap());
    Some((first ^ second.rotate_left(17), second))
}
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "ssse3")]
unsafe fn simd(bytes: &[u8]) -> Option<(u64, u64)> {
    use std::arch::x86_64::*;
    unsafe {
        let a = _mm_loadu_si128(bytes.as_ptr().add(1).cast());
        let b = _mm_loadu_si128(bytes.as_ptr().add(17).cast());
        let digit = |v| {
            _mm_and_si128(
                _mm_cmpgt_epi8(v, _mm_set1_epi8(b'0' as i8 - 1)),
                _mm_cmplt_epi8(v, _mm_set1_epi8(b'9' as i8 + 1)),
            )
        };
        let alpha = |v| {
            _mm_and_si128(
                _mm_cmpgt_epi8(v, _mm_set1_epi8(b'a' as i8 - 1)),
                _mm_cmplt_epi8(v, _mm_set1_epi8(b'f' as i8 + 1)),
            )
        };
        let (da, db, aa, ab) = (digit(a), digit(b), alpha(a), alpha(b));
        if _mm_movemask_epi8(_mm_or_si128(da, aa)) != 65535
            || _mm_movemask_epi8(_mm_or_si128(db, ab)) != 65535
        {
            return None;
        }
        let decode = |v, letters| {
            let d = _mm_sub_epi8(v, _mm_set1_epi8(b'0' as i8));
            let h = _mm_sub_epi8(v, _mm_set1_epi8(b'a' as i8 - 10));
            _mm_or_si128(_mm_and_si128(letters, h), _mm_andnot_si128(letters, d))
        };
        let a = _mm_maddubs_epi16(decode(a, aa), _mm_set1_epi16(0x0110));
        let b = _mm_maddubs_epi16(decode(b, ab), _mm_set1_epi16(0x0110));
        let joined = _mm_packus_epi16(a, b);
        let first = _mm_cvtsi128_si64(joined) as u64;
        let second = _mm_cvtsi128_si64(_mm_srli_si128::<8>(joined)) as u64;
        Some((first ^ second.rotate_left(17), second))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn exact_shape_all_byte_mutations_and_round_trip() {
        let input = *b"e0123456789abcdef0011223344556677";
        let expected = scalar(&input).unwrap();
        let first = expected.0 ^ expected.1.rotate_left(17);
        let raw = [first.to_le_bytes(), expected.1.to_le_bytes()].concat();
        assert_eq!(
            raw,
            [
                0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef, 0x00, 0x11, 0x22, 0x33, 0x44, 0x55,
                0x66, 0x77
            ]
        );
        assert_eq!(pack(&input), Some(expected));
        for at in 0..input.len() {
            for byte in 0..=255u8 {
                let mut changed = input;
                changed[at] = byte;
                assert_eq!(pack(&changed), scalar(&changed), "at {at} byte {byte}");
            }
        }
        for n in 0..33 {
            assert_eq!(pack(&input[..n]), None);
        }
        let mut longer = input.to_vec();
        longer.push(b'0');
        assert_eq!(pack(&longer), None);
        assert_eq!(pack(b"E0123456789abcdef0011223344556677"), None);
        assert_eq!(pack(b"e0123456789ABCDEF0011223344556677"), None);
    }
}
