//! Scan a batch of non-overlapping cache lines once for all three CSV markers.
use crate::{fastrows::fields, scan::scalar_rows};

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Masks {
    commas: u64,
    linefeeds: u64,
    quotes: u64,
}

#[cfg(all(target_arch = "x86_64", target_os = "linux"))]
std::arch::global_asm!(include_str!("wide_masks.S"));

#[cfg(all(target_arch = "x86_64", target_os = "linux"))]
unsafe extern "C" {
    fn shopping_wide_masks(input: *const u8, output: *mut Masks, count: usize);
}

pub(super) fn available() -> bool {
    #[cfg(all(target_arch = "x86_64", target_os = "linux"))]
    {
        std::is_x86_feature_detected!("avx512f")
            && std::is_x86_feature_detected!("avx512bw")
            && std::is_x86_feature_detected!("bmi1")
            && std::is_x86_feature_detected!("bmi2")
            && std::is_x86_feature_detected!("popcnt")
    }
    #[cfg(not(all(target_arch = "x86_64", target_os = "linux")))]
    {
        false
    }
}

fn scalar_mask(input: &[u8]) -> Masks {
    let mut m = Masks::default();
    for (i, &byte) in input.iter().enumerate() {
        match byte {
            b',' => m.commas |= 1 << i,
            b'\n' => m.linefeeds |= 1 << i,
            b'"' => m.quotes |= 1 << i,
            _ => {}
        }
    }
    m
}

#[inline(always)]
fn aligned_mask(
    previous: u64,
    older: u64,
    current: u64,
    base: usize,
    start: usize,
    len: usize,
) -> u128 {
    let bits = if start >= base {
        u128::from(current >> (start - base))
    } else {
        let distance = base - start;
        // A short line touches at most three consecutive cache lines. The low
        // bits of this pair end immediately before the current cache line.
        let previous_pair = u128::from(older) | (u128::from(previous) << 64);
        (previous_pair >> (128 - distance)) | (u128::from(current) << distance)
    };
    bits & ((1u128 << len) - 1)
}

#[derive(Default)]
struct RowState {
    previous: Masks,
    older: Masks,
    base: usize,
    start: usize,
}

impl RowState {
    #[inline(always)]
    fn consume<'a, const N: usize>(
        &mut self,
        m: Masks,
        data: &'a [u8],
        visit: &mut impl FnMut([&'a [u8]; N]),
    ) {
        let mut lfs = m.linefeeds;
        while lfs != 0 {
            let end = self.base + lfs.trailing_zeros() as usize;
            lfs &= lfs - 1;
            let len = end - self.start;
            if len > 127 {
                scalar_rows(&data[self.start..end], &mut *visit);
            } else if len != 0 {
                let commas = aligned_mask(
                    self.previous.commas,
                    self.older.commas,
                    m.commas,
                    self.base,
                    self.start,
                    len,
                );
                let quotes = if (self.previous.quotes | self.older.quotes | m.quotes) == 0 {
                    0
                } else {
                    aligned_mask(
                        self.previous.quotes,
                        self.older.quotes,
                        m.quotes,
                        self.base,
                        self.start,
                        len,
                    )
                };
                if let Some(row) = fields(&data[self.start..end], commas, quotes) {
                    visit(row);
                }
            }
            self.start = end + 1;
        }
        self.older = self.previous;
        self.previous = m;
        self.base += 64;
    }

    // Keep the row mask alignment, field-count popcounts and bit extraction
    // together under the same CPU-feature boundary.
    #[cfg(all(target_arch = "x86_64", target_os = "linux"))]
    #[target_feature(enable = "bmi1,bmi2,popcnt")]
    unsafe fn consume_tuned<'a, const N: usize>(
        &mut self,
        m: Masks,
        data: &'a [u8],
        visit: &mut impl FnMut([&'a [u8]; N]),
    ) {
        self.consume(m, data, visit);
    }
}

fn decode<'a, const N: usize, V: FnMut([&'a [u8]; N])>(
    data: &'a [u8],
    mut visit: V,
    mut classify: impl FnMut(&[u8], &mut [Masks]),
    mut consume: impl FnMut(&mut RowState, Masks, &'a [u8], &mut V),
) {
    let mut state = RowState::default();
    const BATCH: usize = 32;
    let full = data.len() / 64 * 64;
    let mut masks = [Masks::default(); BATCH];
    for bytes in data[..full].chunks(BATCH * 64) {
        let out = &mut masks[..bytes.len() / 64];
        classify(bytes, out);
        for &mask in out.iter() {
            consume(&mut state, mask, data, &mut visit);
        }
    }
    let tail = &data[full..];
    let mut m = scalar_mask(tail);
    // Every complete input chunk ends at an LF or EOF. The synthetic LF
    // handles an unterminated last record and requires no out-of-bounds load.
    m.linefeeds |= 1 << tail.len();
    consume(&mut state, m, data, &mut visit);
}

#[cfg(all(target_arch = "x86_64", target_os = "linux"))]
pub(super) unsafe fn rows<'a, const N: usize>(data: &'a [u8], visit: impl FnMut([&'a [u8]; N])) {
    decode(
        data,
        visit,
        |input, output| {
            // The process-wide dispatch checks AVX-512F/BW. Each output element
            // corresponds to one fully readable 64-byte block. count is positive.
            unsafe { shopping_wide_masks(input.as_ptr(), output.as_mut_ptr(), output.len()) }
        },
        |state, m, data, v| unsafe { state.consume_tuned(m, data, v) },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check<const N: usize>(data: &[u8]) {
        let mut expected = Vec::new();
        scalar_rows(data, |v: [&[u8]; N]| expected.push(v.map(<[u8]>::to_vec)));
        let mut emulated = Vec::new();
        decode(
            data,
            |v: [&[u8]; N]| emulated.push(v.map(<[u8]>::to_vec)),
            |input, out| {
                for (segment, mask) in input.chunks_exact(64).zip(out) {
                    *mask = scalar_mask(segment);
                }
            },
            RowState::consume,
        );
        assert_eq!(emulated, expected);
        #[cfg(all(target_arch = "x86_64", target_os = "linux"))]
        if available() {
            let mut actual = Vec::new();
            unsafe { rows(data, |v: [&[u8]; N]| actual.push(v.map(<[u8]>::to_vec))) };
            assert_eq!(actual, expected);
        }
    }

    #[test]
    fn boundaries_quotes_oversized_and_unterminated_records() {
        for offset in 0..64 {
            let mut chunk = vec![b'\n'; offset];
            for len in 0..=160 {
                chunk.extend(std::iter::repeat_n(b'a', len));
                chunk.extend_from_slice(b",\"b\"\n\"c\",\"d\"\n\"e\",\"x\"f\n");
            }
            chunk.extend(std::iter::repeat_n(b'x', 4096));
            chunk.extend_from_slice(b",\0\r\nlast,");
            check::<2>(&chunk);
        }
    }

    #[test]
    fn randomized_structural_masks_and_all_arities() {
        let mut s = 91u64;
        let mut data = Vec::new();
        for i in 0..80_000 {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            data.push(match s % 64 {
                0..=6 => b',',
                7..=8 => b'"',
                9 => b'\n',
                10 => 0,
                11 => b'\r',
                _ => s as u8,
            });
            if i % 1000 == 0 {
                data.extend_from_slice(b"\na,b,c,d,e,f,g\n1,2,3,4\nx,y\n");
            }
        }
        check::<1>(&data);
        check::<2>(&data);
        check::<4>(&data);
        check::<7>(&data);
    }
}
