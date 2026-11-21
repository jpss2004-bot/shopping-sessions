//! CSV scanning for the contest's LF-only dialect. Every comma is a field boundary.
use memchr::memchr_iter;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Scanner {
    Scalar,
    WholeRow,
    WholeIntel,
    Wide,
    Avx2,
}

impl Scanner {
    fn select(request: &str, avx2: bool, bmi_popcnt: bool) -> Self {
        match request {
            "scalar" => Self::Scalar,
            "whole" => Self::WholeRow,
            "whole-intel" if avx2 && bmi_popcnt => Self::WholeIntel,
            "whole-intel" => Self::WholeRow,
            "auto" | "avx2" => {
                if avx2 {
                    Self::Avx2
                } else {
                    Self::Scalar
                }
            }
            _ => panic!("SHOPPING_SCAN must be auto, scalar, whole, whole-intel, wide, or avx2"),
        }
    }

    fn for_process() -> Self {
        use std::sync::OnceLock;
        static SCANNER: OnceLock<Scanner> = OnceLock::new();
        *SCANNER.get_or_init(|| {
            #[cfg(target_arch = "x86_64")]
            let avx2 = std::is_x86_feature_detected!("avx2");
            #[cfg(not(target_arch = "x86_64"))]
            let avx2 = false;
            #[cfg(target_arch = "x86_64")]
            let bmi_popcnt = std::is_x86_feature_detected!("bmi1")
                && std::is_x86_feature_detected!("bmi2")
                && std::is_x86_feature_detected!("popcnt");
            #[cfg(not(target_arch = "x86_64"))]
            let bmi_popcnt = false;
            let request = std::env::var("SHOPPING_SCAN").unwrap_or_else(|_| "wide".into());
            if request == "wide" {
                if crate::wide::available() {
                    return Scanner::Wide;
                }
                return Self::select("whole-intel", avx2, bmi_popcnt);
            }
            Self::select(&request, avx2, bmi_popcnt)
        })
    }
}

pub fn rows<const N: usize>(chunk: &[u8], visit: impl FnMut([&[u8]; N])) {
    let selected = Scanner::for_process();
    #[cfg(all(target_arch = "x86_64", target_os = "linux"))]
    if selected == Scanner::Wide {
        return unsafe { crate::wide::rows(chunk, visit) };
    }
    #[cfg(target_arch = "x86_64")]
    if selected == Scanner::WholeIntel {
        // The process-wide dispatch above checks every enabled feature.
        return unsafe { crate::fastrows::intel_rows_tuned(chunk, visit) };
    }
    if selected == Scanner::WholeRow {
        return crate::fastrows::rows(chunk, visit);
    }
    if selected == Scanner::Avx2 {
        #[cfg(target_arch = "x86_64")]
        // Scanner selection checks the CPU and OS support before this call.
        return unsafe { avx2_rows(chunk, visit) };
    }
    scalar_rows(chunk, visit);
}

pub fn scalar_rows<'a, const N: usize>(chunk: &'a [u8], mut visit: impl FnMut([&'a [u8]; N])) {
    let mut start = 0;
    for end in memchr_iter(b'\n', chunk).chain(std::iter::once(chunk.len())) {
        if end > start {
            let mut parts = chunk[start..end].split(|&b| b == b',');
            let mut fields = [b"".as_slice(); N];
            let valid = fields.iter_mut().all(|field| {
                let Some(mut value) = parts.next() else {
                    return false;
                };
                if value.len() >= 2 && value[0] == b'"' && value[value.len() - 1] == b'"' {
                    value = &value[1..value.len() - 1];
                }
                *field = value;
                !value.contains(&b'"')
            });
            if valid && parts.next().is_none() {
                visit(fields);
            }
        }
        start = end + 1;
    }
}

#[cfg(any(target_arch = "x86_64", test))]
struct CsvRow<'a, const N: usize> {
    data: &'a [u8],
    fields: [&'a [u8]; N],
    columns: usize,
    field_start: usize,
    row_start: usize,
    pending_quotes: usize,
    valid: bool,
}

#[cfg(any(target_arch = "x86_64", test))]
impl<'a, const N: usize> CsvRow<'a, N> {
    fn new(data: &'a [u8]) -> Self {
        Self {
            data,
            fields: [&[]; N],
            columns: 0,
            field_start: 0,
            row_start: 0,
            pending_quotes: 0,
            valid: true,
        }
    }

    #[inline(always)]
    fn boundary(&mut self, end: usize, newline: bool, visit: &mut impl FnMut([&'a [u8]; N])) {
        if let Some(field) = self.fields.get_mut(self.columns) {
            let bytes = &self.data[self.field_start..end];
            match self.pending_quotes {
                0 => *field = bytes,
                2 if bytes.len() >= 2 && bytes[0] == b'"' && bytes[bytes.len() - 1] == b'"' => {
                    *field = &bytes[1..bytes.len() - 1];
                }
                _ => self.valid = false,
            }
        }
        self.columns += 1;
        self.pending_quotes = 0;
        self.field_start = end + 1;
        if newline {
            if self.columns == N && self.valid && end != self.row_start {
                visit(self.fields);
            }
            self.row_start = end + 1;
            self.columns = 0;
            self.valid = true;
        }
    }

    #[inline(always)]
    fn block(
        &mut self,
        base: usize,
        mut ends: u32,
        newlines: u32,
        mut quotes: u32,
        visit: &mut impl FnMut([&'a [u8]; N]),
    ) {
        while ends != 0 {
            let pos = ends.trailing_zeros();
            let bit = 1 << pos;
            let before = bit - 1;
            self.pending_quotes += (quotes & before).count_ones() as usize;
            quotes &= !before;
            self.boundary(base + pos as usize, newlines & bit != 0, visit);
            ends &= ends - 1;
        }
        self.pending_quotes += quotes.count_ones() as usize;
    }

    fn tail(&mut self, base: usize, visit: &mut impl FnMut([&'a [u8]; N])) {
        for (i, &byte) in self.data[base..].iter().enumerate() {
            match byte {
                b'"' => self.pending_quotes += 1,
                b',' | b'\n' => self.boundary(base + i, byte == b'\n', visit),
                _ => {}
            }
        }
        if self.row_start < self.data.len() {
            self.boundary(self.data.len(), true, visit);
        }
    }
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn avx2_rows<'a, const N: usize>(data: &'a [u8], mut visit: impl FnMut([&'a [u8]; N])) {
    use std::arch::x86_64::*;
    let mut row = CsvRow::<N>::new(data);
    let mut base = 0;
    while data.len() - base >= 32 {
        // The loop condition guarantees all 32 bytes belong to the input slice.
        let (ends, newlines, quotes) = unsafe {
            let bytes = _mm256_loadu_si256(data.as_ptr().add(base).cast());
            let commas = _mm256_cmpeq_epi8(bytes, _mm256_set1_epi8(b',' as i8));
            let lfs = _mm256_cmpeq_epi8(bytes, _mm256_set1_epi8(b'\n' as i8));
            let quotes = _mm256_cmpeq_epi8(bytes, _mm256_set1_epi8(b'"' as i8));
            (
                _mm256_movemask_epi8(_mm256_or_si256(commas, lfs)) as u32,
                _mm256_movemask_epi8(lfs) as u32,
                _mm256_movemask_epi8(quotes) as u32,
            )
        };
        row.block(base, ends, newlines, quotes, &mut visit);
        base += 32;
    }
    row.tail(base, &mut visit);
}

// Exercise the same 32-byte decoder on architectures without AVX2.
#[cfg(test)]
pub fn emulated_rows<'a, const N: usize>(data: &'a [u8], mut visit: impl FnMut([&'a [u8]; N])) {
    let mut row = CsvRow::<N>::new(data);
    let mut base = 0;
    while data.len() - base >= 32 {
        let mut ends = 0;
        let mut lfs = 0;
        let mut quotes = 0;
        for (i, &b) in data[base..base + 32].iter().enumerate() {
            ends |= u32::from(b == b',' || b == b'\n') << i;
            lfs |= u32::from(b == b'\n') << i;
            quotes |= u32::from(b == b'"') << i;
        }
        row.block(base, ends, lfs, quotes, &mut visit);
        base += 32;
    }
    row.tail(base, &mut visit);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selection_respects_scalar_override_and_available_instructions() {
        assert_eq!(
            Scanner::select("whole-intel", false, true),
            Scanner::WholeRow
        );
        assert_eq!(
            Scanner::select("whole-intel", true, false),
            Scanner::WholeRow
        );
        assert_eq!(
            Scanner::select("whole-intel", true, true),
            Scanner::WholeIntel
        );
        assert_eq!(Scanner::select("auto", false, false), Scanner::Scalar);
        assert_eq!(Scanner::select("avx2", false, false), Scanner::Scalar);
        assert_eq!(Scanner::select("scalar", true, false), Scanner::Scalar);
        assert_eq!(Scanner::select("auto", true, false), Scanner::Avx2);
        assert_eq!(Scanner::select("avx2", true, false), Scanner::Avx2);
    }

    #[test]
    fn dialect_and_trailing_fields_survive_each_vector_alignment() {
        for prefix in 0..64 {
            let padding = vec![b'x'; prefix];
            let mut data = padding.clone();
            data.extend_from_slice(
                b",first\n\"\",\"\"\nbad,ab\"c\n\"left\",right,extra\n\n\"x\\y\",v\0\r\nlast,",
            );
            let expected = vec![
                (padding, b"first".to_vec()),
                (vec![], vec![]),
                (b"x\\y".to_vec(), b"v\0\r".to_vec()),
                (b"last".to_vec(), vec![]),
            ];
            for mode in 0..3 {
                let mut result = Vec::new();
                let mut visit = |[a, b]: [&[u8]; 2]| result.push((a.to_vec(), b.to_vec()));
                match mode {
                    0 => scalar_rows(&data, &mut visit),
                    1 => emulated_rows(&data, &mut visit),
                    _ => rows(&data, &mut visit),
                }
                assert_eq!(result, expected, "alignment {prefix}, scanner {mode}");
            }
        }
    }
}
