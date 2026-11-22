//! Index each link endpoint once. Immutable bucket maps outlive union-find
//! and serve the event and click lookups directly.
use crate::input::{Key, Keys, Names, hash};
use crate::scan::scalar_rows;
use ahash::AHashMap;
use memchr::{memchr_iter, memchr2};
use rayon::prelude::*;
use std::{
    sync::Mutex,
    time::{Duration, Instant},
};

type Map = AHashMap<Key, Key>;
pub enum Canon {
    Flat(Map),
    Buckets(Vec<Map>),
}
impl Canon {
    #[inline]
    pub fn resolve(&self, key: Key) -> Key {
        let map = match self {
            Self::Flat(map) => map,
            Self::Buckets(maps) => &maps[hash(key) as usize & (maps.len() - 1)],
        };
        map.get(&key).copied().unwrap_or(key)
    }
    pub fn maps(&self) -> &[Map] {
        match self {
            Self::Flat(map) => std::slice::from_ref(map),
            Self::Buckets(maps) => maps,
        }
    }
}

pub fn rss_kib() -> u64 {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("VmRSS:"))?
                .split_whitespace()
                .nth(1)?
                .parse()
                .ok()
        })
        .unwrap_or(0)
}
fn thread_cpu_ns() -> Option<u64> {
    std::fs::read_to_string("/proc/thread-self/schedstat")
        .ok()?
        .split_whitespace()
        .next()?
        .parse()
        .ok()
}
pub fn timed_chunks<T: Send>(
    chunks: &[&[u8]],
    profile: bool,
    run: impl Fn(&[u8]) -> T + Sync,
) -> Vec<T> {
    let dispatch = Instant::now();
    let work: Vec<_> = chunks
        .par_iter()
        .enumerate()
        .map(|(i, bytes)| {
            if !profile {
                return (run(bytes), None);
            }
            let cpu = thread_cpu_ns();
            let begin = dispatch.elapsed();
            let result = run(bytes);
            let end = dispatch.elapsed();
            let cpu_ns = cpu.zip(thread_cpu_ns()).map(|(a, b)| b.saturating_sub(a));
            (result, Some((i, bytes.len(), begin, end, cpu_ns)))
        })
        .collect();
    let returned = dispatch.elapsed();
    if profile {
        let last = work
            .iter()
            .filter_map(|(_, sample)| sample.as_ref().map(|(_, _, _, end, _)| *end))
            .max()
            .unwrap_or_default();
        eprintln!(
            "link_dispatch collect_us={} last_worker_done_us={} resume_gap_us={}",
            returned.as_micros(),
            last.as_micros(),
            returned.saturating_sub(last).as_micros()
        );
    }
    work.into_iter()
        .map(|(result, sample)| {
            if let Some((i, bytes, begin, end, cpu)) = sample {
                eprintln!(
                    "link_chunk id={i} bytes={bytes} dispatch_us={} elapsed_us={} thread_cpu_us={}",
                    begin.as_micros(),
                    (end - begin).as_micros(),
                    cpu.map_or(-1, |n| (n / 1000) as i64)
                );
            }
            result
        })
        .collect()
}

fn pairs<'a>(chunk: &'a [u8], mut visit: impl FnMut([&'a [u8]; 2])) {
    let mut start = 0;
    for end in memchr_iter(b'\n', chunk).chain(std::iter::once(chunk.len())) {
        if end > start {
            let line = &chunk[start..end];
            match memchr2(b',', b'"', line) {
                Some(i) if line[i] == b',' && memchr2(b',', b'"', &line[i + 1..]).is_none() => {
                    visit([&line[..i], &line[i + 1..]])
                }
                _ => scalar_rows(line, &mut visit),
            }
        }
        start = end + 1;
    }
}

#[derive(Default)]
struct Index {
    lookup: Map,
    keys: Vec<Key>,
}
#[derive(Default)]
struct Edges {
    within: Vec<Vec<[usize; 2]>>,
    cross: Vec<[usize; 2]>,
}
struct Pending<'a> {
    indexes: &'a [Mutex<Index>],
    // Each bucket carries a key and its destination in the ordered pairs.
    requests: Vec<Vec<(Key, usize)>>,
    endpoints: Vec<usize>,
    edges: Edges,
}
impl<'a> Pending<'a> {
    fn new(indexes: &'a [Mutex<Index>]) -> Self {
        Self {
            indexes,
            requests: vec![Vec::new(); indexes.len()],
            endpoints: Vec::with_capacity(8192),
            edges: Edges {
                within: vec![Vec::new(); indexes.len()],
                cross: Vec::new(),
            },
        }
    }
    fn pair(&mut self, a: Key, b: Key) {
        if a == b || a == 0 || b == 0 {
            return;
        }
        let mask = self.indexes.len() - 1;
        for k in [a, b] {
            self.requests[hash(k) as usize & mask].push((k, self.endpoints.len()));
            self.endpoints.push(0);
        }
        if self.endpoints.len() == 8192 {
            self.flush();
        }
    }
    fn flush(&mut self) {
        let width = self.indexes.len();
        for (b, requests) in self.requests.iter_mut().enumerate() {
            if requests.is_empty() {
                continue;
            }
            let mut guard = self.indexes[b].lock().unwrap();
            let Index { lookup, keys } = &mut *guard;
            for (key, position) in requests.drain(..) {
                let local = *lookup.entry(key).or_insert_with(|| {
                    let index = keys.len() as Key;
                    keys.push(key);
                    index
                });
                self.endpoints[position] = (local as usize) * width + b;
            }
        }
        for pair in self.endpoints.chunks_exact(2) {
            let (a, b) = (pair[0], pair[1]);
            if (a ^ b) & (width - 1) == 0 {
                self.edges.within[a & (width - 1)].push([a / width, b / width]);
            } else {
                self.edges.cross.push([a, b]);
            }
        }
        self.endpoints.clear();
    }
}

struct Node {
    parent: isize,
    least: Key,
}
fn root(nodes: &mut [Node], mut i: usize) -> usize {
    while nodes[i].parent >= 0 {
        let p = nodes[i].parent as usize;
        if nodes[p].parent < 0 {
            return p;
        }
        nodes[i].parent = nodes[p].parent;
        i = nodes[i].parent as usize;
    }
    i
}
fn join(nodes: &mut [Node], a: usize, b: usize, names: &Names) {
    let (mut a, mut b) = (root(nodes, a), root(nodes, b));
    if a == b {
        return;
    }
    if nodes[a].parent > nodes[b].parent {
        std::mem::swap(&mut a, &mut b);
    }
    nodes[a].parent += nodes[b].parent;
    nodes[b].parent = a as isize;
    if names.less(nodes[b].least, nodes[a].least) {
        nodes[a].least = nodes[b].least;
    }
}
pub fn build(
    chunks: &[&[u8]],
    keys: &Keys,
    width: usize,
    profile: bool,
    start: Instant,
) -> (Canon, Duration) {
    assert!(width.is_power_of_two());
    let indexes: Vec<Mutex<Index>> = (0..width).map(|_| Mutex::default()).collect();
    let blocks = timed_chunks(chunks, profile, |chunk| {
        let mut batch = Pending::new(&indexes);
        pairs(chunk, |[a, b]| {
            if !a.is_empty() && !b.is_empty() {
                batch.pair(keys.id(a), keys.id(b));
            }
        });
        batch.flush();
        batch.edges
    });
    let loaded = start.elapsed();
    let names = keys.names();
    let mut within = vec![Vec::new(); width];
    let mut cross = Vec::with_capacity(blocks.len());
    for block in blocks {
        for (dest, edges) in within.iter_mut().zip(block.within) {
            dest.push(edges);
        }
        cross.push(block.cross);
    }
    let by_bucket: Vec<_> = indexes
        .into_par_iter()
        .zip(within)
        .map(|(index, within)| {
            let Index { lookup, keys } = index.into_inner().unwrap();
            let mut nodes: Vec<_> = keys
                .into_iter()
                .map(|least| Node { parent: -1, least })
                .collect();
            for [a, b] in within.into_iter().flatten() {
                join(&mut nodes, a, b, &names);
            }
            (lookup, nodes)
        })
        .collect();
    let total = by_bucket.iter().map(|(_, nodes)| nodes.len()).sum();
    let mut graph = Vec::with_capacity(total);
    let mut maps = Vec::with_capacity(width);
    let mut offsets = Vec::with_capacity(width);
    for (map, nodes) in by_bucket {
        let base = graph.len();
        offsets.push(base);
        maps.push(map);
        graph.extend(nodes.into_iter().map(|mut n| {
            if n.parent >= 0 {
                n.parent += base as isize;
            }
            n
        }));
    }
    let dense = |n: usize| offsets[n & (width - 1)] + n / width;
    for [a, b] in cross.into_iter().flatten() {
        join(&mut graph, dense(a), dense(b), &names);
    }
    // Resolve each dense slot once. Buckets may then update their hash tables in parallel.
    for i in 0..graph.len() {
        let r = root(&mut graph, i);
        graph[i].least = graph[r].least;
    }
    maps.par_iter_mut().zip(offsets).for_each(|(map, base)| {
        map.retain(|&key, value| {
            *value = graph[base + *value as usize].least;
            *value != key
        });
    });
    if profile {
        let node_bytes = graph.capacity() * std::mem::size_of::<Node>();
        let rss = rss_kib();
        let t = Instant::now();
        drop(graph);
        let us = t.elapsed().as_micros();
        eprintln!(
            "link_workspace nodes_bytes={node_bytes} drop_us={us} rss_before_kib={rss} rss_after_kib={}",
            rss_kib()
        );
    }
    (Canon::Buckets(maps), loaded)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn simple_and_exceptional_pairs_match_reference() {
        let mut input = Vec::new();
        for byte in 0..=255u8 {
            for mut line in [b"abc,def".to_vec(), b"\"ab\",\"cd\"".to_vec()] {
                for i in 0..line.len() {
                    let prev = std::mem::replace(&mut line[i], byte);
                    input.extend_from_slice(&line);
                    input.push(b'\n');
                    line[i] = prev;
                }
            }
        }
        input.extend_from_slice(b",last");
        let (mut actual, mut expected) = (Vec::new(), Vec::new());
        pairs(&input, |r| actual.push(r));
        scalar_rows(&input, |r| expected.push(r));
        assert_eq!(actual, expected);
    }
}
