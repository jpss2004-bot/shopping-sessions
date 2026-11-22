use crate::input::{DAY, Key, Names, WEEK};
use ahash::AHashMap;
use std::collections::hash_map::Entry;

#[derive(Clone, Copy)]
pub struct Event {
    pub user: Key,
    pub ts: i64,
    pub price: u32,
    pub category: u32,
}
#[derive(Clone, Copy)]
pub struct Click {
    pub user: Key,
    pub ts: i64,
    pub id: Key,
    pub campaign: u32,
}
pub struct UserDay {
    pub user: Key,
    pub day: i32,
    pub sessions: i64,
    pub events: i64,
    pub revenue: i64,
    pub top: Option<u32>,
}
#[derive(Default)]
pub struct Credit {
    pub sessions: i64,
    pub purchases: i64,
    pub revenue: i64,
    pub assists: i64,
}
pub type CampaignDays = AHashMap<(u32, i32), Credit>;
struct Session {
    ts: i64,
    day: i32,
    revenue: i64,
}

pub fn summarize(
    mut events: Vec<Event>,
    mut clicks: Vec<Click>,
    names: &Names,
    categories: &[Key],
) -> (Vec<UserDay>, CampaignDays) {
    let mut users = Vec::new();
    let mut campaigns = CampaignDays::new();
    let mut sessions = Vec::<Session>::new();
    let mut category_revenue = Vec::<(u32, i64)>::new();
    let mut last_interval = AHashMap::<u32, (i64, i64)>::new();
    let mut add_person = |person: &[Event], ads: &[Click]| {
        let user = person[0].user;
        sessions.clear();
        let mut previous = None;
        let mut daily: Option<UserDay> = None;
        let finish = |mut row: UserDay, sums: &mut Vec<(u32, i64)>, users: &mut Vec<UserDay>| {
            row.top = sums
                .drain(..)
                .max_by(|(a, x), (b, y)| {
                    x.cmp(y).then_with(|| {
                        if names.less(categories[*a as usize], categories[*b as usize]) {
                            std::cmp::Ordering::Greater
                        } else if a == b {
                            std::cmp::Ordering::Equal
                        } else {
                            std::cmp::Ordering::Less
                        }
                    })
                })
                .map(|(c, _)| c);
            users.push(row);
        };
        for e in person {
            if previous.is_none_or(|last| e.ts - last > 1_800_000) {
                let day = e.ts.div_euclid(DAY) as i32;
                sessions.push(Session {
                    ts: e.ts,
                    day,
                    revenue: 0,
                });
                if daily.as_ref().is_none_or(|row| row.day != day) {
                    if let Some(row) = daily.take() {
                        finish(row, &mut category_revenue, &mut users);
                    }
                    daily = Some(UserDay {
                        user,
                        day,
                        sessions: 0,
                        events: 0,
                        revenue: 0,
                        top: None,
                    });
                }
                daily.as_mut().unwrap().sessions += 1;
            }
            previous = Some(e.ts);
            let row = daily.as_mut().unwrap();
            row.events += 1;
            if e.price != 0 {
                let price = i64::from(e.price);
                row.revenue += price;
                sessions.last_mut().unwrap().revenue += price;
                if let Some((_, value)) =
                    category_revenue.iter_mut().find(|(c, _)| *c == e.category)
                {
                    *value += price;
                } else {
                    category_revenue.push((e.category, price));
                }
            }
        }
        finish(daily.unwrap(), &mut category_revenue, &mut users);

        let mut next = 0;
        let mut latest: Option<&Click> = None;
        for session in &sessions {
            while next < ads.len() && ads[next].ts <= session.ts {
                let c = &ads[next];
                if latest.is_none_or(|old| c.ts > old.ts || names.click_less(c.id, old.id)) {
                    latest = Some(c);
                }
                next += 1;
            }
            let id = latest
                .filter(|c| session.ts - c.ts <= WEEK)
                .map_or(0, |c| c.campaign);
            let credit = campaigns
                .entry((id, session.day))
                .or_insert_with(Credit::default);
            credit.sessions += 1;
            credit.revenue += session.revenue;
            credit.purchases += i64::from(session.revenue != 0);
        }
        // Merge the inclusive attribution intervals per campaign, then count the
        // covered session starts by day. Identical click copies are idempotent here
        // and under the latest-time/smallest-ID selection above.
        let mut count_interval = |campaign, lo, hi| {
            let from = sessions.partition_point(|s| s.ts < lo);
            let until = from + sessions[from..].partition_point(|s| s.ts <= hi);
            for starts in sessions[from..until].chunk_by(|a, b| a.day == b.day) {
                campaigns
                    .entry((campaign, starts[0].day))
                    .or_default()
                    .assists += starts.len() as i64;
            }
        };
        for c in ads.iter() {
            let end = c.ts + WEEK;
            match last_interval.entry(c.campaign) {
                Entry::Vacant(v) => {
                    v.insert((c.ts, end));
                }
                Entry::Occupied(mut v) => {
                    let interval = v.get_mut();
                    if c.ts > interval.1 {
                        count_interval(c.campaign, interval.0, interval.1);
                        interval.0 = c.ts;
                    }
                    interval.1 = end;
                }
            }
        }
        for (id, (lo, hi)) in last_interval.drain() {
            count_interval(id, lo, hi)
        }
    };
    if std::env::var("PERSON_GROUP").as_deref() == Ok("sort") {
        events.sort_unstable_by_key(|e| (e.user, e.ts));
        clicks.sort_unstable_by_key(|c| (c.user, c.ts));
        let mut remaining = clicks.as_mut_slice();
        for person in events.chunk_by(|a, b| a.user == b.user) {
            let user = person[0].user;
            let earlier = remaining.partition_point(|c| c.user < user);
            remaining = &mut remaining[earlier..];
            let count = remaining.partition_point(|c| c.user == user);
            let (ads, rest) = remaining.split_at_mut(count);
            remaining = rest;
            add_person(person, ads);
        }
    } else {
        dense_people(events, clicks, add_person);
    }
    (users, campaigns)
}

const NO_EVENTS: u32 = u32::MAX;

// Convert each group's count into its exclusive end in the returned array.
// The caller can iterate group spans sequentially using those ends directly.
fn scatter<T>(rows: Vec<T>, ordinals: Vec<u32>, mut counts: Vec<usize>) -> (Box<[T]>, Vec<usize>) {
    assert_eq!(rows.len(), ordinals.len());
    let mut total = 0;
    for count in &mut counts {
        let n = *count;
        *count = total;
        total += n;
    }
    let mut result = Box::<[T]>::new_uninit_slice(total);
    for (value, ordinal) in rows.into_iter().zip(ordinals) {
        if ordinal != NO_EVENTS {
            let position = &mut counts[ordinal as usize];
            result[*position].write(value);
            *position += 1;
        }
    }
    // Each included row incremented exactly one group count in the histogram.
    // Prefix sums gave the groups disjoint ranges covering 0..total and the
    // scatter initialized one successive element for each such row.
    (unsafe { result.assume_init() }, counts)
}

fn dense_people(events: Vec<Event>, clicks: Vec<Click>, mut visit: impl FnMut(&[Event], &[Click])) {
    if events.is_empty() {
        return;
    }
    // Group IDs are shard-local. Their order has no effect on either table.
    let mut ids = AHashMap::with_capacity(events.len() / 30);
    let mut event_counts = Vec::new();
    let mut event_ordinals = Vec::with_capacity(events.len());
    for event in &events {
        let group = *ids.entry(event.user).or_insert_with(|| {
            let group = u32::try_from(event_counts.len()).expect("too many people in one shard");
            assert_ne!(group, NO_EVENTS);
            event_counts.push(0);
            group
        });
        event_counts[group as usize] += 1;
        event_ordinals.push(group);
    }
    let mut click_counts = vec![0; ids.len()];
    let mut click_ordinals = Vec::with_capacity(clicks.len());
    for click in &clicks {
        let group = ids.get(&click.user).copied().unwrap_or(NO_EVENTS);
        if group != NO_EVENTS {
            click_counts[group as usize] += 1;
        }
        click_ordinals.push(group);
    }
    drop(ids);
    let (mut events, event_ends) = scatter(events, event_ordinals, event_counts);
    let (mut clicks, click_ends) = scatter(clicks, click_ordinals, click_counts);
    let mut a = 0;
    let mut b = 0;
    for (end_a, end_b) in event_ends.into_iter().zip(click_ends) {
        let person = &mut events[a..end_a];
        let ads = &mut clicks[b..end_b];
        // Whales can dominate one shard. Rayon can lend idle workers to
        // those timestamp sorts after other shards have finished.
        use rayon::prelude::*;
        if person.len() >= 65536 {
            person.par_sort_unstable_by_key(|e| e.ts);
        } else {
            person.sort_unstable_by_key(|e| e.ts);
        }
        if ads.len() >= 65536 {
            ads.par_sort_unstable_by_key(|c| c.ts);
        } else {
            ads.sort_unstable_by_key(|c| c.ts);
        }
        visit(person, ads);
        a = end_a;
        b = end_b;
    }
}
