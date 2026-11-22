pub use crate::scan::rows;
use ahash::AHashMap;
use memchr::memchr;
use memmap2::Mmap;
use std::{
    fs::{self, File},
    io,
    path::Path,
    sync::{Arc, Mutex},
};

pub const BAD: i64 = i64::MIN;
pub const DAY: i64 = 86_400_000;
pub const WEEK: i64 = 7 * DAY;
pub type Key = u64;
const LONG: Key = 1 << 63;

const NAME_SHARDS: usize = 128;

#[derive(Default)]
struct Dictionary {
    ids: AHashMap<Arc<[u8]>, usize>,
    names: Vec<Arc<[u8]>>,
}

pub struct Keys {
    shards: [Mutex<Dictionary>; NAME_SHARDS],
    hash: ahash::RandomState,
    event_hash: ahash::RandomState,
}

impl Default for Keys {
    fn default() -> Self {
        Self {
            shards: std::array::from_fn(|_| Mutex::default()),
            hash: ahash::RandomState::new(),
            event_hash: ahash::RandomState::with_seeds(
                0x29a7_411b_208d_e311,
                0xba12_03e9_7915_d455,
                0x4e82_32af_810d_0071,
                0x6d89_5ea3_fa00_b907,
            ),
        }
    }
}

pub struct Names([Vec<Arc<[u8]>>; NAME_SHARDS]);

impl Keys {
    pub fn id(&self, s: &[u8]) -> Key {
        Self::inline_id(s).unwrap_or_else(|| self.name_id(s))
    }

    // Fingerprints route opaque event bytes. Equality is established from the
    // original bytes in the shard owner whenever a fingerprint is ambiguous.
    #[inline]
    pub(crate) fn event_fingerprint(&self, s: &[u8]) -> Key {
        LONG | (self.event_hash.hash_one(s) & !LONG)
    }

    #[inline]
    pub(crate) fn inline_id(s: &[u8]) -> Option<Key> {
        if s.is_empty() {
            return Some(0);
        }
        if s.len() == 9 && s[0] > 0 && s[0] < 128 {
            let bytes = u64::from_be_bytes(s[1..].try_into().unwrap());
            let high = 0x8080_8080_8080_8080;
            let zeros = bytes.wrapping_sub(0x0101_0101_0101_0101) & !bytes & high;
            if bytes & high == 0 && zeros == 0 {
                #[cfg(all(target_arch = "x86_64", target_feature = "bmi2"))]
                {
                    // PEXT retains the seven payload bits in each byte in order.
                    let payload = unsafe { std::arch::x86_64::_pext_u64(bytes, !high) };
                    return Some((u64::from(s[0]) << 56) | payload);
                }
                #[cfg(not(all(target_arch = "x86_64", target_feature = "bmi2")))]
                {
                    // Remove each byte's zero high bit, then combine pairs, quads and octets.
                    let pairs =
                        (bytes & 0x007f_007f_007f_007f) | ((bytes & 0x7f00_7f00_7f00_7f00) >> 1);
                    let quads =
                        (pairs & 0x0000_3fff_0000_3fff) | ((pairs & 0x3fff_0000_3fff_0000) >> 2);
                    return Some(
                        (u64::from(s[0]) << 56) | (quads & 0x0fff_ffff) | ((quads >> 32) << 28),
                    );
                }
            }
        }
        if s.len() <= 9 {
            let mut value = 0u64;
            let mut fits = true;
            for &byte in s {
                fits &= byte != 0 && byte < 128;
                value = (value << 7) | u64::from(byte);
            }
            if fits {
                return Some(value << (7 * (9 - s.len())));
            }
        }
        None
    }

    fn name_id(&self, s: &[u8]) -> Key {
        let shard = self.hash.hash_one(s) as usize % NAME_SHARDS;
        let mut dict = self.shards[shard].lock().unwrap();
        let index = match dict.ids.get(s) {
            Some(&index) => index,
            None => {
                let index = dict.names.len();
                let bytes = Arc::<[u8]>::from(s);
                dict.names.push(Arc::clone(&bytes));
                dict.ids.insert(bytes, index);
                index
            }
        };
        LONG | ((index as Key) << NAME_SHARDS.trailing_zeros()) | shard as Key
    }

    pub fn names(&self) -> Names {
        Names(std::array::from_fn(|shard| {
            self.shards[shard].lock().unwrap().names.clone()
        }))
    }
}
impl Names {
    pub fn bytes<'a>(&'a self, key: Key, buf: &'a mut [u8; 9]) -> &'a [u8] {
        if key & LONG != 0 {
            let shard = key as usize % NAME_SHARDS;
            let index = ((key ^ LONG) >> NAME_SHARDS.trailing_zeros()) as usize;
            return &self.0[shard][index];
        }
        let mut k = key;
        let mut len = 0;
        while k != 0 {
            buf[len] = (k >> 56) as u8;
            k = (k << 7) & !LONG;
            len += 1;
        }
        &buf[..len]
    }
    pub fn less(&self, a: Key, b: Key) -> bool {
        if (a | b) & LONG == 0 {
            return a < b;
        }
        self.bytes(a, &mut [0; 9]) < self.bytes(b, &mut [0; 9])
    }
}
pub struct Files(Vec<Mmap>);
impl Files {
    pub fn open(dir: &Path) -> io::Result<Self> {
        let mut maps = Vec::new();
        for entry in fs::read_dir(dir)? {
            let path = entry?.path();
            if path.extension().and_then(|s| s.to_str()) != Some("csv") {
                continue;
            }
            let f = File::open(path)?;
            if f.metadata()?.len() != 0 {
                // Scorer inputs stay read-only for the lifetime of the map.
                maps.push(unsafe { Mmap::map(&f)? });
            }
        }
        Ok(Self(maps))
    }
    pub fn chunks(&self) -> Vec<&[u8]> {
        let mut chunks = Vec::new();
        for map in &self.0 {
            if let Some(lf) = memchr(b'\n', map) {
                let mut s = &map[lf + 1..];
                while !s.is_empty() {
                    let cut = 1 << 21;
                    let end = s
                        .get(cut..)
                        .and_then(|tail| memchr(b'\n', tail))
                        .map_or(s.len(), |pos| cut + pos + 1);
                    chunks.push(&s[..end]);
                    s = &s[end..];
                }
            }
        }
        chunks
    }
}
pub fn dimension(path: &Path, mut visit: impl FnMut(&[u8], &[u8])) -> io::Result<()> {
    let bytes = fs::read(path)?;
    if let Some(header) = memchr(b'\n', &bytes) {
        rows(&bytes[header + 1..], |[a, b]| visit(a, b));
    }
    Ok(())
}
pub fn digits(s: &[u8], limit: i64) -> i64 {
    if s.is_empty() {
        return BAD;
    }
    if s.len() <= 18 {
        let mut value = 0i64;
        let mut valid = true;
        for &b in s {
            let digit = b.wrapping_sub(b'0');
            valid &= digit < 10;
            value = value.wrapping_mul(10).wrapping_add(i64::from(digit));
        }
        return if valid && value <= limit { value } else { BAD };
    }
    let mut value = 0;
    for &b in s {
        if !b.is_ascii_digit() {
            return BAD;
        }
        let d = i64::from(b - b'0');
        if value > (limit - d) / 10 {
            return BAD;
        }
        value = value * 10 + d;
    }
    value
}
#[inline(always)]
fn pair(s: &[u8], i: usize) -> i64 {
    let a = s[i].wrapping_sub(b'0');
    let b = s[i + 1].wrapping_sub(b'0');
    if a > 9 || b > 9 {
        100
    } else {
        i64::from(a * 10 + b)
    }
}
pub fn stamp(s: &[u8]) -> i64 {
    if !matches!(s.len(), 20 | 25) || s[4] != b'-' {
        return digits(s, i64::MAX);
    }
    if s[7] != b'-' || s[10] != b'T' || s[13] != b':' || s[16] != b':' {
        return BAD;
    }
    let (century, yy, m, d) = (pair(s, 0), pair(s, 2), pair(s, 5), pair(s, 8));
    let (h, min, sec) = (pair(s, 11), pair(s, 14), pair(s, 17));
    if century > 99 || yy > 99 || !(1..=12).contains(&m) || h >= 24 || min >= 60 || sec >= 60 {
        return BAD;
    }
    let y = century * 100 + yy;
    let lengths = [
        31,
        28 + i64::from(y % 4 == 0 && (y % 100 != 0 || y % 400 == 0)),
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    if d < 1 || d > lengths[(m - 1) as usize] {
        return BAD;
    }
    let offset = if s.len() == 20 {
        if s[19] != b'Z' {
            return BAD;
        }
        0
    } else {
        let (oh, om) = (pair(s, 20), pair(s, 23));
        if oh >= 24 || om >= 60 || s[22] != b':' {
            return BAD;
        }
        match s[19] {
            b'+' => oh * 60 + om,
            b'-' => -oh * 60 - om,
            _ => return BAD,
        }
    };
    let year = y - i64::from(m <= 2);
    let era = year.div_euclid(400);
    let yo = year - 400 * era;
    let days = era * 146097 + yo * 365 + yo / 4 - yo / 100
        + (153 * (m + if m > 2 { -3 } else { 9 }) + 2) / 5
        + d
        - 1
        - 719468;
    ((days * 1440 + h * 60 + min - offset) * 60 + sec) * 1000
}
pub fn hash(mut x: Key) -> u64 {
    x ^= x >> 30;
    x = x.wrapping_mul(0xbf58476d1ce4e5b9);
    x ^= x >> 27;
    x = x.wrapping_mul(0x94d049bb133111eb);
    x ^ (x >> 31)
}

// Ingest timestamps are guaranteed to be valid UTC whole seconds. Cache the
// window's starting month while retaining the general parser for other months.
pub struct Month {
    pub(crate) prefix: [u8; 8],
    pub(crate) base: i64,
    pub(crate) length: i64,
}
impl Month {
    pub fn from_start(s: &[u8]) -> Self {
        let mut first = *b"2000-01-01T00:00:00Z";
        first[..8].copy_from_slice(&s[..8]);
        let base = stamp(&first);
        first[8..10].copy_from_slice(b"31");
        let length = if stamp(&first) != BAD {
            31
        } else {
            first[8..10].copy_from_slice(b"30");
            if stamp(&first) != BAD {
                30
            } else {
                first[8..10].copy_from_slice(b"29");
                if stamp(&first) != BAD { 29 } else { 28 }
            }
        };
        Self {
            prefix: s[..8].try_into().unwrap(),
            base,
            length,
        }
    }
    pub fn ingest(&self, s: &[u8]) -> i64 {
        if s.len() != 20 || s[..8] != self.prefix {
            return stamp(s);
        }
        let two = |at: usize| i64::from(s[at] - b'0') * 10 + i64::from(s[at + 1] - b'0');
        self.base + (two(8) - 1) * DAY + ((two(11) * 60 + two(14)) * 60 + two(17)) * 1000
    }
    pub fn event(&self, s: &[u8]) -> i64 {
        if s.len() != 20 || s[..8] != self.prefix {
            return stamp(s);
        }
        if s[10] != b'T' || s[13] != b':' || s[16] != b':' || s[19] != b'Z' {
            return BAD;
        }
        let (d, h, m, sec) = (pair(s, 8), pair(s, 11), pair(s, 14), pair(s, 17));
        if d == 0 || d > self.length || h >= 24 || m >= 60 || sec >= 60 {
            return BAD;
        }
        self.base + (d - 1) * DAY + ((h * 60 + m) * 60 + sec) * 1000
    }
}

#[cfg(test)]
mod dictionary_tests {
    use super::*;

    #[test]
    fn opaque_fingerprints_have_a_namespace_separate_from_inline_ids() {
        let a = Keys::default();
        let b = Keys::default();
        for bytes in [
            b"abc".as_slice(),
            b"ninechars",
            b"a long ID including a comma,",
            b"\0",
            &[255],
        ] {
            if let Some(inline) = Keys::inline_id(bytes) {
                assert_eq!(inline & LONG, 0);
            }
            assert_ne!(a.event_fingerprint(bytes) & LONG, 0);
            assert_eq!(a.event_fingerprint(bytes), b.event_fingerprint(bytes));
        }
    }

    #[test]
    fn long_ids_round_trip_when_shards_grow_concurrently() {
        let keys = Keys::default();
        let ids: Vec<_> = (0..2048)
            .map(|i| format!("user-with-a-long-common-prefix-{i}"))
            .collect();
        std::thread::scope(|scope| {
            for offset in 0..4 {
                let keys = &keys;
                let ids = &ids;
                scope.spawn(move || {
                    for i in 0..ids.len() {
                        keys.id(ids[(i + offset * 23) % ids.len()].as_bytes());
                    }
                });
            }
        });
        let snapshot = keys.names();
        let mut buf = [0; 9];
        for value in ids {
            assert_eq!(
                snapshot.bytes(keys.id(value.as_bytes()), &mut buf),
                value.as_bytes()
            );
        }
        let a = keys.id(b"foo");
        let b = keys.id(b"foo with a space");
        let names = keys.names();
        assert!(names.less(a, b));
        assert!(!names.less(b, a));
    }
}
