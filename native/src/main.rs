mod clockfast;
mod compact;
mod dedup;
mod fastrows;
mod identity;
mod identity_index;
mod input;
mod output;
mod scan;
mod singletons;
mod tables;
mod wide;
use ahash::AHashMap;
use input::{BAD, DAY, Files, Key, Keys, WEEK, hash, rows, stamp};
use rayon::prelude::*;
use std::{
    env, fs,
    path::{Path, PathBuf},
    time::Instant,
};
use tables::{CampaignDays, Click, Event};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;
const SHARDS: usize = 128;

fn link_map(data: &Path, keys: &Keys) -> Result<AHashMap<Key, Key>> {
    let start = Instant::now();
    let files = Files::open(&data.join("links"))?;
    let chunks = files.chunks();
    let edges: Vec<Vec<_>> = chunks
        .par_iter()
        .map(|s| {
            let mut edges = Vec::new();
            rows(s, |[a, b]| {
                if !a.is_empty() && !b.is_empty() {
                    edges.push((keys.id(a), keys.id(b)))
                }
            });
            edges
        })
        .collect();
    let loaded = start.elapsed();
    let result = match env::var("IDENTITY_ENGINE").as_deref() {
        Ok("compact") => identity_index::compact(edges, keys),
        Ok("bucket") => identity_index::bucketed(edges, keys),
        Ok("stock") | Err(std::env::VarError::NotPresent) => identity::build(edges, keys),
        _ => return Err("IDENTITY_ENGINE must be stock, compact or bucket".into()),
    };
    let graph_time = start.elapsed() - loaded;
    if env::var_os("PROFILE").is_some() {
        eprintln!(
            "identity_edges_us={} graph_us={}",
            loaded.as_micros(),
            graph_time.as_micros()
        );
    }
    if env::var_os("LINK_BENCH").is_some() {
        let names = keys.names();
        let mut fingerprint = 0u64;
        for (&key, &canonical) in &result {
            let digest = |k| {
                names
                    .bytes(k, &mut [0; 9])
                    .iter()
                    .fold(0xcbf2_9ce4_8422_2325u64, |h, b| {
                        (h ^ u64::from(*b)).wrapping_mul(0x100_0000_01b3)
                    })
            };
            fingerprint =
                fingerprint.wrapping_add(hash(digest(key) ^ digest(canonical).rotate_left(17)));
        }
        eprintln!(
            "linkbench graph_us={} load_us={} mappings={} fingerprint={:016x}",
            graph_time.as_micros(),
            loaded.as_micros(),
            result.len(),
            fingerprint
        );
    }
    Ok(result)
}
fn buckets<T>() -> Vec<Vec<T>> {
    (0..SHARDS).map(|_| Vec::new()).collect()
}
// Size the initial buffers from input bytes. Rows wider or shorter than the
// estimate use normal Vec growth. The estimate never affects field validation.
fn reserved_buckets<T>(rows: usize) -> Vec<Vec<T>> {
    (0..SHARDS)
        .map(|_| Vec::with_capacity(rows.div_ceil(SHARDS)))
        .collect()
}
fn report_buffers<T>(label: &str, blocks: &[Vec<Vec<T>>]) {
    if env::var_os("PROFILE_MEMORY").is_some() {
        let (mut used, mut capacity, mut fragments) = (0, 0, 0);
        for part in blocks.iter().flatten() {
            used += part.len();
            capacity += part.capacity();
            fragments += usize::from(part.capacity() > 0);
        }
        let width = std::mem::size_of::<T>();
        eprintln!(
            "{label} live_bytes={} capacity_bytes={} allocated_buffers={fragments}",
            used * width,
            capacity * width
        );
    }
}
fn redistribute<T>(blocks: Vec<Vec<Vec<T>>>) -> Vec<Vec<Vec<T>>> {
    let mut shards = buckets();
    for block in blocks {
        for (shard, items) in shards.iter_mut().zip(block) {
            if !items.is_empty() {
                shard.push(items)
            }
        }
    }
    shards
}
fn gather<T>(parts: Vec<Vec<T>>) -> Vec<T> {
    let mut items = Vec::with_capacity(parts.iter().map(Vec::len).sum());
    for part in parts {
        items.extend(part)
    }
    items
}
fn shard(id: Key) -> usize {
    (hash(id) as usize) & (SHARDS - 1)
}
#[derive(Copy, Clone)]
struct CopyOfEvent {
    id: Key,
    ingest: i64,
    event: Event,
}
fn winners(
    packed: Vec<compact::Part>,
    origin: i64,
    mode: &str,
    canon: &AHashMap<Key, Key>,
) -> Vec<Vec<Event>> {
    let mut out = reserved_buckets(packed.iter().map(compact::Part::len).sum());
    let mut emit = |mut e: Event| {
        if e.ts != BAD {
            e.user = *canon.get(&e.user).unwrap_or(&e.user);
            out[shard(e.user)].push(e);
        }
    };
    if mode == "singletons" {
        compact::select(&packed, origin, emit);
        return out;
    }
    let mut copies: Vec<CopyOfEvent> = packed
        .into_iter()
        .flat_map(|part| part.expand(origin))
        .collect();
    if matches!(mode, "radix" | "compact-hash") {
        dedup::select(&copies, mode, emit);
    } else if mode == "hash" {
        let mut index = AHashMap::with_capacity(copies.len());
        for row in copies {
            let winner = index.entry(row.id).or_insert(row);
            if row.ingest < winner.ingest {
                *winner = row
            }
        }
        for winner in index.into_values() {
            emit(winner.event)
        }
    } else {
        copies.sort_unstable_by_key(|e| e.id);
        for group in copies.chunk_by(|a, b| a.id == b.id) {
            let winner = group.iter().min_by_key(|e| e.ingest).unwrap();
            emit(winner.event);
        }
    }
    out
}
fn merge_campaigns(shards: impl IntoIterator<Item = CampaignDays>) -> CampaignDays {
    let mut totals = CampaignDays::new();
    for days in shards {
        for (key, v) in days {
            let d = totals.entry(key).or_default();
            d.sessions += v.sessions;
            d.purchases += v.purchases;
            d.revenue += v.revenue;
            d.assists += v.assists;
        }
    }
    totals
}

fn run() -> Result<()> {
    let start = Instant::now();
    let profile = env::var_os("PROFILE").is_some();
    let mark = |name: &str| {
        if profile {
            eprintln!("{name} {:.6}", start.elapsed().as_secs_f64())
        }
    };
    let data = PathBuf::from(env::var_os("DATA_DIR").unwrap_or_else(|| "/data".into()));
    let output = PathBuf::from(env::var_os("OUTPUT_DIR").unwrap_or_else(|| "/output".into()));
    if env::var_os("LINK_BENCH").is_some() {
        let threads = env::var("NCPU")
            .unwrap_or_else(|_| "8".into())
            .parse::<usize>()?
            .max(1);
        rayon::ThreadPoolBuilder::new()
            .num_threads(threads)
            .build_global()?;
        link_map(&data, &Keys::default())?;
        return Ok(());
    }
    let manifest: serde_json::Value =
        serde_json::from_slice(&fs::read(data.join("manifest.json"))?)?;
    let timestamp = |field: &str| -> Result<i64> {
        let ts = stamp(
            manifest[field]
                .as_str()
                .ok_or("missing manifest timestamp")?
                .as_bytes(),
        );
        if ts == BAD {
            return Err("invalid manifest timestamp".into());
        }
        Ok(ts)
    };
    let (lo, hi) = (timestamp("window_start")?, timestamp("window_end")?);
    let month = input::Month::from_start(manifest["window_start"].as_str().unwrap().as_bytes());
    let threads = env::var("NCPU")
        .unwrap_or_else(|_| "8".into())
        .parse::<usize>()?
        .max(1);
    rayon::ThreadPoolBuilder::new()
        .num_threads(threads)
        .build_global()?;
    let keys = Keys::default();
    let unknown = keys.id(b"unknown");
    let organic = keys.id(b"organic");
    let mut categories = vec![unknown];
    let mut category_ids = AHashMap::from_iter([(unknown, 0u32)]);
    let mut products = AHashMap::new();
    input::dimension(&data.join("products.csv"), |id, category| {
        let name = keys.id(category);
        let index = *category_ids.entry(name).or_insert_with(|| {
            categories.push(name);
            (categories.len() - 1) as u32
        });
        products.insert(keys.id(id), index);
    })?;
    let mut campaign_names = vec![(organic, organic)];
    let mut campaigns = AHashMap::new();
    input::dimension(&data.join("campaigns.csv"), |id, channel| {
        let id = keys.id(id);
        campaigns.insert(id, campaign_names.len() as u32);
        campaign_names.push((id, keys.id(channel)));
    })?;
    let events = Files::open(&data.join("events"))?;
    let group_size = env::var("INPUT_GROUP")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(4)
        .max(1);
    mark("join_start");
    let (canon, parsed) = rayon::join(
        || {
            mark("links_start");
            let canon = link_map(&data, &keys);
            mark("links_ready");
            canon
        },
        || {
            mark("events_start");
            let parsed = events
                .chunks()
                .par_chunks(group_size)
                .map(|group| {
                    let mut parts =
                        compact::buckets(group.iter().map(|c| c.len()).sum::<usize>() / 65);
                    for chunk in group {
                        rows(chunk, |[id, user, ts, ingest, kind, product, price]| {
                            if id.is_empty() {
                                return;
                            }
                            let id = keys.id(id);
                            let ts = clockfast::event(&month, ts);
                            let ingest = month.ingest(ingest);
                            let purchase = kind == b"purchase";
                            let price = if purchase {
                                input::digits(price, 10_000_000)
                            } else {
                                0
                            };
                            let valid = ts >= lo
                                && ts < hi
                                && !user.is_empty()
                                && !product.is_empty()
                                && (kind == b"view" || kind == b"cart" || purchase && price > 0)
                                && ingest != BAD
                                && ingest - ts <= DAY;
                            let event = if valid {
                                let category = if purchase {
                                    *products.get(&keys.id(product)).unwrap_or(&0)
                                } else {
                                    0
                                };
                                Event {
                                    user: keys.id(user),
                                    ts,
                                    price: price as u32,
                                    category,
                                }
                            } else {
                                Event {
                                    user: 0,
                                    ts: BAD,
                                    price: 0,
                                    category: 0,
                                }
                            };
                            parts[shard(id)].push(id, ingest, event, lo);
                        });
                    }
                    parts
                })
                .collect::<Vec<_>>();
            mark("events_ready");
            parsed
        },
    );
    mark("join_released");
    let canon = canon?;
    drop(events);
    mark("parsed events and resolved identities");
    let mode = env::var("EVENT_DEDUP").unwrap_or_else(|_| "singletons".into());
    if !matches!(
        mode.as_str(),
        "sort" | "hash" | "radix" | "compact-hash" | "singletons"
    ) {
        return Err("unknown EVENT_DEDUP".into());
    }
    if env::var_os("PROFILE_MEMORY").is_some() {
        compact::report(&parsed);
    }
    let first_winner = std::sync::atomic::AtomicBool::new(true);
    let events = redistribute(
        compact::transpose(parsed)
            .into_par_iter()
            .map(|parts| {
                if profile && first_winner.swap(false, std::sync::atomic::Ordering::Relaxed) {
                    mark("first_dedup_task");
                }
                winners(parts, lo, &mode, &canon)
            })
            .collect(),
    );
    report_buffers("linked events", &events);
    mark("deduplicated and linked events");
    let clicks = Files::open(&data.join("clicks"))?;
    let ads: Vec<_> = clicks
        .chunks()
        .par_chunks(group_size)
        .map(|group| {
            let mut parts = reserved_buckets(group.iter().map(|c| c.len()).sum::<usize>() / 38);
            for chunk in group {
                rows(chunk, |[id, user, campaign, ts]| {
                    if id.is_empty() || user.is_empty() {
                        return;
                    }
                    let ts = clockfast::event(&month, ts);
                    if ts < lo - WEEK || ts >= hi {
                        return;
                    }
                    let Some(&campaign) = campaigns.get(&keys.id(campaign)) else {
                        return;
                    };
                    let user = keys.id(user);
                    let user = *canon.get(&user).unwrap_or(&user);
                    parts[shard(user)].push(Click {
                        user,
                        ts,
                        id: keys.id(id),
                        campaign,
                    });
                });
            }
            parts
        })
        .collect();
    report_buffers("clicks", &ads);
    drop(clicks);
    drop(canon);
    mark("parsed clicks");
    let names = keys.names();
    let users = output.join("user_daily");
    let campaign_dir = output.join("campaign_daily");
    fs::create_dir_all(&users)?;
    fs::create_dir_all(&campaign_dir)?;
    let partitions = threads.min(SHARDS);
    let group_size = SHARDS.div_ceil(partitions);
    let shards = events.into_par_iter().zip(redistribute(ads));
    let summarize = |(e, c)| tables::summarize(gather(e), gather(c), &names, &categories);
    let path = |part| users.join(format!("part-{part}.parquet"));

    let totals = match env::var("USER_WRITE_SCHEDULE").as_deref() {
        Ok("early") => {
            // The writer pulls each shard after it is summarized. A partition owns
            // its file and can close row groups while the other partitions aggregate.
            let groups: Vec<Vec<CampaignDays>> = shards
                .chunks(group_size)
                .enumerate()
                .map(|(part, group)| -> Result<_> {
                    let mut days = Vec::with_capacity(group.len());
                    let mut first_ready = None;
                    let mut last_ready = None;
                    let rows = group.into_iter().map(summarize).map(|(rows, credits)| {
                        if profile {
                            let ready = start.elapsed();
                            first_ready.get_or_insert(ready);
                            last_ready = Some(ready);
                        }
                        days.push(credits);
                        rows
                    });
                    output::users(&path(part), rows, &names, &categories)?;
                    if let (Some(first), Some(last)) = (first_ready, last_ready) {
                        eprintln!(
                            "user_partition {part} first_ready={:.6} last_ready={:.6} closed={:.6}",
                            first.as_secs_f64(),
                            last.as_secs_f64(),
                            start.elapsed().as_secs_f64()
                        );
                    }
                    Ok(days)
                })
                .collect::<Result<_>>()?;
            mark("user_files_closed");
            merge_campaigns(groups.into_iter().flatten())
        }
        Ok("deferred") | Err(std::env::VarError::NotPresent) => {
            let results: Vec<_> = shards.map(summarize).collect();
            let (rows, days): (Vec<_>, Vec<_>) = results.into_iter().unzip();
            mark("all_user_rows_ready");
            // Keep the official order, including the merge before opening user files.
            let totals = merge_campaigns(days);
            rows.par_chunks(group_size)
                .enumerate()
                .try_for_each(|(part, group)| {
                    output::users(&path(part), group, &names, &categories)
                })?;
            mark("user_files_closed");
            totals
        }
        _ => return Err("USER_WRITE_SCHEDULE must be early or deferred".into()),
    };
    output::campaigns(
        &campaign_dir.join("part-0.parquet"),
        totals,
        &names,
        &campaign_names,
    )?;
    mark("wrote parquet");
    Ok(())
}
fn main() {
    if let Err(e) = run() {
        eprintln!("{e}");
        std::process::exit(1)
    }
}
