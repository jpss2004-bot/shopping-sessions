//! Alternative indexes for the identity graph. Both return the same sparse
//! canonical map as the scored implementation, so downstream lookups are identical.
use crate::{
    identity,
    input::{Key, Keys, Names, hash},
};
use ahash::AHashMap;
use rayon::prelude::*;

type Edges = Vec<Vec<(Key, Key)>>;
type Aliases = AHashMap<Key, Key>;
const BUCKETS: usize = 32;

#[derive(Clone, Copy, Default)]
struct Slot {
    key: Key,
    // Negative component size at roots, otherwise the slot of the parent.
    parent: i32,
    minimum: u32,
}

fn find_slot(slots: &mut [Slot], key: Key) -> usize {
    let mask = slots.len() - 1;
    let mut i = hash(key) as usize & mask;
    loop {
        if slots[i].key == key {
            return i;
        }
        if slots[i].key == 0 {
            slots[i] = Slot {
                key,
                parent: -1,
                minimum: i as u32,
            };
            return i;
        }
        i = (i + 1) & mask;
    }
}

fn root(slots: &mut [Slot], mut i: usize) -> usize {
    while slots[i].parent >= 0 {
        let parent = slots[i].parent as usize;
        let grandparent = slots[parent].parent;
        if grandparent < 0 {
            return parent;
        }
        slots[i].parent = grandparent;
        i = grandparent as usize;
    }
    i
}

fn join(slots: &mut [Slot], a: usize, b: usize, names: &Names) {
    let (mut a, mut b) = (root(slots, a), root(slots, b));
    if a == b {
        return;
    }
    if slots[a].parent > slots[b].parent {
        std::mem::swap(&mut a, &mut b);
    }
    slots[a].parent += slots[b].parent;
    slots[b].parent = a as i32;
    let left = slots[a].minimum as usize;
    let right = slots[b].minimum as usize;
    if names.less(slots[right].key, slots[left].key) {
        slots[a].minimum = right as u32;
    }
}

fn aliases(mut slots: Vec<Slot>, edge_count: usize) -> Aliases {
    let mut aliases = Aliases::with_capacity(edge_count);
    for i in 0..slots.len() {
        let key = slots[i].key;
        if key != 0 {
            let r = root(&mut slots, i);
            let smallest = slots[slots[r].minimum as usize].key;
            if key != smallest {
                aliases.insert(key, smallest);
            }
        }
    }
    aliases
}

pub(super) fn compact(edges: Edges, keys: &Keys) -> Aliases {
    let count: usize = edges.iter().map(Vec::len).sum();
    // Two endpoints per edge, plus room for the probe to encounter an empty slot.
    // Cap the flat table at 256 MiB. Larger inputs use the general hash-based path.
    let Some(capacity) = count
        .checked_mul(2)
        .and_then(|n| n.checked_add(n / 8 + 1))
        .and_then(usize::checked_next_power_of_two)
        .filter(|&n| n <= (1 << 24))
    else {
        return identity::build(edges, keys);
    };
    let mut slots = vec![Slot::default(); capacity];
    let names = keys.names();
    for (a, b) in edges.into_iter().flatten() {
        if a != b && a != 0 && b != 0 {
            let a = find_slot(&mut slots, a);
            let b = find_slot(&mut slots, b);
            join(&mut slots, a, b, &names);
        }
    }
    aliases(slots, count)
}

#[inline]
fn bucket(key: Key) -> usize {
    hash(key) as usize & (BUCKETS - 1)
}

pub(super) fn bucketed(edges: Edges, keys: &Keys) -> Aliases {
    let count: usize = edges.iter().map(Vec::len).sum();
    if count > i32::MAX as usize / 2 {
        return identity::build(edges, keys);
    }
    // Each task writes private chunks. A bucket has exactly one writer while
    // assigning indices. Later lookups share its finished, immutable map.
    let blocks: Vec<_> = edges
        .par_iter()
        .map(|part| {
            let mut lists: [Vec<Key>; BUCKETS] = std::array::from_fn(|_| Vec::new());
            for &(a, b) in part {
                if a != b && a != 0 && b != 0 {
                    lists[bucket(a)].push(a);
                    lists[bucket(b)].push(b);
                }
            }
            lists
        })
        .collect();
    let mut buckets: [Vec<Vec<Key>>; BUCKETS] = std::array::from_fn(|_| Vec::new());
    for block in blocks {
        for (dest, src) in buckets.iter_mut().zip(block) {
            dest.push(src);
        }
    }
    let indexes: Vec<_> = buckets
        .into_par_iter()
        .map(|lists| {
            let mut map = AHashMap::with_capacity(lists.iter().map(Vec::len).sum());
            let mut unique = Vec::new();
            for key in lists.into_iter().flatten() {
                map.entry(key).or_insert_with(|| {
                    let index = unique.len() as u32;
                    unique.push(key);
                    index
                });
            }
            (map, unique)
        })
        .collect();
    let mut slots = Vec::with_capacity(indexes.iter().map(|(_, u)| u.len()).sum());
    let mut lookup = Vec::with_capacity(BUCKETS);
    for (index, unique) in indexes {
        let start = slots.len() as u32;
        for key in unique {
            let minimum = slots.len() as u32;
            slots.push(Slot {
                key,
                parent: -1,
                minimum,
            });
        }
        lookup.push((index, start));
    }
    let dense_id = |key| {
        let (index, start) = &lookup[bucket(key)];
        start + index[&key]
    };
    let encoded: Vec<Vec<(u32, u32)>> = edges
        .into_par_iter()
        .map(|part| {
            part.into_iter()
                .filter(|(a, b)| a != b && *a != 0 && *b != 0)
                .map(|(a, b)| (dense_id(a), dense_id(b)))
                .collect()
        })
        .collect();
    drop(lookup);
    let names = keys.names();
    for (a, b) in encoded.into_iter().flatten() {
        join(&mut slots, a as usize, b as usize, &names);
    }
    aliases(slots, count)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn both_indexes_match_stock_on_mixed_identifiers_and_probe_collisions() {
        let keys = Keys::default();
        let ids: Vec<_> = (0..3500)
            .map(|i| {
                let name = match i % 5 {
                    0 => format!("wide identifier {i}"),
                    1 => format!("Ü-{i}"),
                    _ => format!("u{i:08}"),
                };
                keys.id(name.as_bytes())
            })
            .collect();
        let mut edges: Edges = (0..11).map(|_| Vec::new()).collect();
        for i in 1..ids.len() {
            if i % 13 != 0 {
                edges[i % 11].push((ids[i], ids[i - 1]));
            }
            edges[(i + 5) % 11].push((ids[i], ids[i]));
        }
        edges[0].extend([(ids[25], ids[100]), (0, ids[3]), (ids[100], ids[25])]);
        let stock = identity::build(edges.clone(), &keys);
        assert_eq!(compact(edges.clone(), &keys), stock);
        assert_eq!(bucketed(edges, &keys), stock);
    }
}
