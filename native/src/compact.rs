use crate::{
    CopyOfEvent, SHARDS,
    input::{BAD, Key},
    singletons::location,
    tables::Event,
};
use ahash::AHashMap;
use std::collections::hash_map::Entry;

// Two distinct markers leave offsets 0..=4_294_967_293 available.
const INVALID: u32 = u32::MAX - 1;
const WIDE: u32 = u32::MAX;

#[derive(Clone, Copy)]
struct Packed {
    id: Key,
    user_or_slot: u64,
    event_ms: u32,
    ingest_ms: i32,
}
#[derive(Clone, Copy)]
struct Extended {
    event: Event,
    ingest: i64,
}
struct OpaqueSpan {
    row: usize,
    end: usize,
}
pub(crate) struct Part {
    rows: Vec<Packed>,
    overflow: Vec<Extended>,
    opaque: Vec<OpaqueSpan>,
    opaque_bytes: Vec<u8>,
}

const _: () = assert!(size_of::<Packed>() == 24);
const _: () = assert!(size_of::<Extended>() == 32);

impl Part {
    fn with_capacity(capacity: usize) -> Self {
        Self {
            rows: Vec::with_capacity(capacity),
            overflow: Vec::new(),
            opaque: Vec::new(),
            opaque_bytes: Vec::new(),
        }
    }
    pub(crate) fn len(&self) -> usize {
        self.rows.len()
    }

    #[inline(always)]
    pub(crate) fn push(&mut self, id: Key, ingest: i64, event: Event, origin: i64) {
        let ingest_delta = ingest.wrapping_sub(origin);
        let event_delta = event.ts.wrapping_sub(origin) as u64;
        let row = if ingest_delta == i64::from(ingest_delta as i32)
            && (event.ts == BAD || event_delta < u64::from(INVALID))
            && event.price == 0
            && event.category == 0
        {
            Packed {
                id,
                user_or_slot: event.user,
                event_ms: if event.ts == BAD {
                    INVALID
                } else {
                    event_delta as u32
                },
                ingest_ms: ingest_delta as i32,
            }
        } else {
            let slot = self.overflow.len();
            self.overflow.push(Extended { event, ingest });
            Packed {
                id,
                user_or_slot: slot as u64,
                event_ms: WIDE,
                ingest_ms: 0,
            }
        };
        self.rows.push(row);
    }

    pub(crate) fn push_opaque(
        &mut self,
        fingerprint: Key,
        bytes: &[u8],
        ingest: i64,
        event: Event,
        origin: i64,
    ) {
        let row = self.rows.len();
        self.opaque_bytes.extend_from_slice(bytes);
        self.opaque.push(OpaqueSpan {
            row,
            end: self.opaque_bytes.len(),
        });
        self.push(fingerprint, ingest, event, origin);
    }

    #[inline(always)]
    fn ingest(&self, row: &Packed, origin: i64) -> i64 {
        if row.event_ms == WIDE {
            self.overflow[row.user_or_slot as usize].ingest
        } else {
            origin.wrapping_add(i64::from(row.ingest_ms))
        }
    }

    #[inline(always)]
    fn event(&self, row: &Packed, origin: i64) -> Event {
        if row.event_ms == WIDE {
            return self.overflow[row.user_or_slot as usize].event;
        }
        let ts = if row.event_ms == INVALID {
            BAD
        } else {
            origin.wrapping_add(i64::from(row.event_ms))
        };
        Event {
            user: row.user_or_slot,
            ts,
            price: 0,
            category: 0,
        }
    }

    pub(crate) fn expand(&self, origin: i64) -> Vec<CopyOfEvent> {
        self.rows
            .iter()
            .map(|row| CopyOfEvent {
                id: row.id,
                ingest: self.ingest(row, origin),
                event: self.event(row, origin),
            })
            .collect()
    }
}

pub(crate) fn buckets(estimated: usize) -> Vec<Part> {
    (0..SHARDS)
        .map(|_| Part::with_capacity(estimated.div_ceil(SHARDS)))
        .collect()
}

pub(crate) fn transpose(blocks: Vec<Vec<Part>>) -> Vec<Vec<Part>> {
    let mut shards: Vec<Vec<Part>> = (0..SHARDS).map(|_| Vec::new()).collect();
    for block in blocks {
        for (shard, part) in shards.iter_mut().zip(block) {
            if part.len() > 0 {
                shard.push(part);
            }
        }
    }
    shards
}

pub(crate) fn report_opaque(blocks: &[Vec<Part>]) {
    let mut rows = 0;
    let mut bytes = 0;
    let mut capacity = 0;
    for part in blocks.iter().flatten() {
        rows += part.opaque.len();
        bytes += part.opaque_bytes.len();
        capacity += part.opaque.capacity() * size_of::<OpaqueSpan>() + part.opaque_bytes.capacity();
    }
    eprintln!(
        "opaque_event_buffers rows={rows} copied_id_bytes={bytes} live_span_bytes={} allocated_sidecar_bytes={capacity}",
        rows * size_of::<OpaqueSpan>()
    );
}

// Diagnostic algorithms require exact integer keys. Their owner resolves bytes
// before expanding rows, retaining the separate high-bit namespace.
pub(crate) fn resolve_opaque(parts: &mut [Part]) {
    let mut ids = AHashMap::<&[u8], Key>::new();
    for part in parts {
        let mut start = 0;
        for span in &part.opaque {
            let key = &part.opaque_bytes[start..span.end];
            let next = (1u64 << 63) | ids.len() as u64;
            part.rows[span.row].id = *ids.entry(key).or_insert(next);
            start = span.end;
        }
    }
}

pub(crate) fn report(blocks: &[Vec<Part>]) {
    let mut rows = 0;
    let mut extended = 0;
    let mut capacity = 0;
    for part in blocks.iter().flatten() {
        rows += part.len();
        extended += part.overflow.len();
        capacity += part.rows.capacity() * size_of::<Packed>()
            + part.overflow.capacity() * size_of::<Extended>();
    }
    eprintln!(
        "event copies primary_rows={rows} extended_rows={extended} live_bytes={} capacity_bytes={capacity}",
        rows * size_of::<Packed>() + extended * size_of::<Extended>()
    );
}

#[derive(Clone, Copy, Default)]
struct Bits {
    seen: u64,
    repeated: u64,
}

/// Same two-bit singleton filter and exact survivor selection as the full layout.
/// A reference to its fragment travels with an ambiguous row to resolve its
/// optional extended slot. Singleton rows never materialize an ingest timestamp.
pub(crate) fn select(parts: &[Part], origin: i64, mut emit: impl FnMut(Event)) {
    let count: usize = parts.iter().map(Part::len).sum();
    if count == 0 {
        return;
    }
    let words = count.div_ceil(8).next_power_of_two();
    let mask = words - 1;
    let mut bitmap = vec![Bits::default(); words];
    for part in parts {
        for row in &part.rows {
            let (i, b) = location(row.id, mask);
            let w = &mut bitmap[i];
            w.repeated |= w.seen & b;
            w.seen |= b;
        }
    }
    if parts.iter().any(|part| !part.opaque.is_empty()) {
        select_opaque(parts, origin, &bitmap, count, emit);
        return;
    }
    let mut ambiguous = AHashMap::<Key, (&Part, &Packed)>::with_capacity(count / 16);
    for part in parts {
        for row in &part.rows {
            let (i, b) = location(row.id, mask);
            if bitmap[i].repeated & b != b {
                emit(part.event(row, origin));
            } else {
                match ambiguous.entry(row.id) {
                    Entry::Vacant(v) => {
                        v.insert((part, row));
                    }
                    Entry::Occupied(mut v) => {
                        let (previous, old) = *v.get();
                        if part.ingest(row, origin) < previous.ingest(old, origin) {
                            v.insert((part, row));
                        }
                    }
                }
            }
        }
    }
    for (part, row) in ambiguous.into_values() {
        emit(part.event(row, origin));
    }
}

// A false singleton hit always enters an exact map. Fingerprint collisions can
// increase work but cannot equate distinct strings. All map keys borrow bytes
// from immutable Parts that remain alive until the map's winners are emitted.
fn select_opaque(
    parts: &[Part],
    origin: i64,
    bitmap: &[Bits],
    count: usize,
    mut emit: impl FnMut(Event),
) {
    let mut inline = AHashMap::<Key, (&Part, &Packed)>::with_capacity(count / 16);
    let mut opaque = AHashMap::<&[u8], (&Part, &Packed)>::with_capacity(count / 16);
    for part in parts {
        let mut next_span = 0;
        let mut bytes_start = 0;
        for (index, row) in part.rows.iter().enumerate() {
            let bytes = if part
                .opaque
                .get(next_span)
                .is_some_and(|span| span.row == index)
            {
                let end = part.opaque[next_span].end;
                let bytes = &part.opaque_bytes[bytes_start..end];
                bytes_start = end;
                next_span += 1;
                Some(bytes)
            } else {
                None
            };
            let (word, bits) = location(row.id, bitmap.len() - 1);
            if bitmap[word].repeated & bits != bits {
                emit(part.event(row, origin));
                continue;
            }
            let entry = if let Some(bytes) = bytes {
                opaque.entry(bytes)
            } else {
                // The inline map has a different key type, so handle it here.
                match inline.entry(row.id) {
                    Entry::Vacant(v) => {
                        v.insert((part, row));
                    }
                    Entry::Occupied(mut v) => {
                        let (old_part, old) = *v.get();
                        if part.ingest(row, origin) < old_part.ingest(old, origin) {
                            v.insert((part, row));
                        }
                    }
                }
                continue;
            };
            match entry {
                Entry::Vacant(v) => {
                    v.insert((part, row));
                }
                Entry::Occupied(mut v) => {
                    let (old_part, old) = *v.get();
                    if part.ingest(row, origin) < old_part.ingest(old, origin) {
                        v.insert((part, row));
                    }
                }
            }
        }
    }
    for (part, row) in inline.into_values().chain(opaque.into_values()) {
        emit(part.event(row, origin));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn event(user: u64, ts: i64, price: u32, category: u32) -> Event {
        Event {
            user,
            ts,
            price,
            category,
        }
    }
    fn collision_parts() -> Vec<Part> {
        let fp = (1u64 << 63) | 17;
        let mut a = Part::with_capacity(0);
        let mut b = Part::with_capacity(0);
        let mut c = Part::with_capacity(0);
        // Same fingerprint, distinct exact bytes. The invalid earlier copy of A
        // arrives in the last fragment and must suppress only A's purchase.
        a.push_opaque(fp, b"opaque-A, with spaces", 900, event(11, 300, 99, 2), 0);
        a.push_opaque(fp, b"opaque-B, with spaces", 900, event(22, 310, 41, 1), 0);
        a.push(17, 200, event(33, 190, 0, 0), 0);
        b.push_opaque(fp, b"opaque-B, with spaces", 800, event(22, 305, 43, 1), 0);
        b.push_opaque(fp, b"opaque-B, with spaces", 800, event(22, 306, 47, 1), 0);
        b.push_opaque(fp, b"opaque-\0C\xff", i64::MAX, event(44, 320, 17, 2), 0);
        c.push_opaque(fp, b"opaque-A, with spaces", 700, event(0, BAD, 0, 0), 0);
        c.push(17, 100, event(33, 90, 0, 0), 0);
        vec![a, b, c]
    }

    #[test]
    fn opaque_hash_collisions_and_late_invalid_copies_use_exact_bytes() {
        let expected = vec![(22, 305, 43, 1), (33, 90, 0, 0), (44, 320, 17, 2)];
        for mode in ["singletons", "sort", "hash", "radix", "compact-hash"] {
            let canon = crate::endpoint::Canon::Flat(AHashMap::new());
            let mut got: Vec<_> = crate::winners(collision_parts(), 0, mode, &canon)
                .into_iter()
                .flatten()
                .map(|e| (e.user, e.ts, e.price, e.category))
                .collect();
            got.sort_unstable();
            assert_eq!(got, expected, "mode {mode}");
        }
    }

    #[test]
    fn unique_opaque_fingerprints_and_empty_fragments_keep_their_payloads() {
        let keys = crate::input::Keys::default();
        let mut part = Part::with_capacity(0);
        for i in 0..5000u64 {
            let bytes = format!("opaque-identifier-{}-é", i);
            part.push_opaque(
                keys.event_fingerprint(bytes.as_bytes()),
                bytes.as_bytes(),
                i as i64,
                event(i, 10_000 + i as i64, (i % 3) as u32, 1),
                0,
            );
        }
        let parts = [Part::with_capacity(0), part, Part::with_capacity(0)];
        let mut got = Vec::new();
        select(&parts, 0, |e| got.push((e.user, e.ts, e.price, e.category)));
        got.sort_unstable();
        let expected: Vec<_> = (0..5000u64)
            .map(|i| (i, 10_000 + i as i64, (i % 3) as u32, 1))
            .collect();
        assert_eq!(got, expected);
    }

    #[test]
    fn layout_and_every_overflow_roundtrip() {
        for origin in [0_i64, 1_800_909_000_000, i64::MAX - 100, i64::MIN + 100] {
            let mut part = Part::with_capacity(0);
            for id in [0, 55, 1 << 63, u64::MAX] {
                for ingest in [
                    origin,
                    origin.wrapping_add(-1),
                    origin.wrapping_add(i64::from(i32::MAX)),
                    origin.wrapping_add(i64::from(i32::MIN)),
                    origin.wrapping_add(i64::from(i32::MAX) + 1),
                    origin.wrapping_add(i64::from(i32::MIN) - 1),
                    i64::MAX,
                    i64::MIN,
                ] {
                    for ts in [
                        origin,
                        origin.wrapping_add(1),
                        origin.wrapping_add(i64::from(INVALID) - 1),
                        origin.wrapping_add(i64::from(INVALID)),
                        origin.wrapping_sub(1),
                        BAD,
                        i64::MAX,
                    ] {
                        for (price, category) in [(0, 0), (1, 0), (0, 1), (u32::MAX, u32::MAX)] {
                            let e = event(u64::MAX - id, ts, price, category);
                            part.push(id, ingest, e, origin);
                            let row = part.rows.last().unwrap();
                            let got = part.event(row, origin);
                            assert_eq!(
                                (
                                    row.id,
                                    part.ingest(row, origin),
                                    got.user,
                                    got.ts,
                                    got.price,
                                    got.category
                                ),
                                (id, ingest, e.user, e.ts, e.price, e.category)
                            );
                        }
                    }
                }
            }
        }
    }
    #[test]
    fn exact_winners_across_fragments_and_representations() {
        let origin = 1_800_000_000_000i64;
        let mut parts: Vec<_> = (0..13).map(|_| Part::with_capacity(0)).collect();
        for i in 0..10000u64 {
            let mixed = crate::input::hash(i);
            let id = crate::input::hash(mixed % 1701);
            let ingest = origin.wrapping_add(i as i64);
            let ts = match mixed % 7 {
                0 => BAD,
                1 => i64::MAX,
                _ => origin + (mixed % 10000) as i64,
            };
            let price = if mixed % 5 == 0 { 20 } else { 0 };
            parts[mixed as usize % 13].push(id, ingest, event(id, ts, price, price), origin);
        }
        let full: Vec<_> = parts.iter().map(|p| p.expand(origin)).collect();
        let mut expected = vec![];
        let mut got = vec![];
        crate::singletons::select(&full, |e| {
            expected.push((e.user, e.ts, e.price, e.category))
        });
        select(&parts, origin, |e| {
            got.push((e.user, e.ts, e.price, e.category))
        });
        expected.sort_unstable();
        got.sort_unstable();
        assert_eq!(got, expected);
    }
}
