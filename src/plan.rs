use std::collections::{BTreeMap, BTreeSet};

use crate::{
    kind::{Blocked, Kind, Live, Ownership, Plan, Step, Want, Why, Withheld, Withhold},
    model::HostName,
};

#[derive(Clone, Copy, PartialEq, Debug)]
enum Fate {
    Open,
    Kept,
    Written,
    Deleted,
}

pub(crate) fn plan<K: Kind>(
    owner: &HostName,
    wants: Vec<Want<K>>,
    mut live: Vec<Live<K>>,
    breaker: Option<&Withhold>,
) -> Plan<K> {
    live.sort_by(|a, b| a.born.cmp(&b.born));
    let mut fate = vec![Fate::Open; live.len()];

    let mut declared: BTreeMap<K::Key, Vec<K::Value>> = BTreeMap::new();
    for want in wants {
        debug_assert_eq!(&want.owner, owner);
        declared
            .entry(K::key(&want.value))
            .or_default()
            .push(want.value);
    }
    let mut held: BTreeMap<K::Key, Vec<usize>> = BTreeMap::new();
    for (index, object) in live.iter().enumerate() {
        if let (Ownership::Marked(marked), Some(_)) = (&object.ownership, &object.value)
            && marked == owner
        {
            held.entry(object.key.clone()).or_default().push(index);
        }
    }
    let slots: Vec<K::Key> = {
        let mut slots: Vec<_> = declared.keys().chain(held.keys()).cloned().collect();
        slots.sort();
        slots.dedup();
        slots
    };

    let value = |index: usize| {
        live[index]
            .value
            .as_ref()
            .expect("held objects have values")
    };
    let mut steps = Vec::new();
    let mut blocked = Vec::new();
    let mut deletes = Vec::new();
    let mut creates = Vec::new();

    for key in slots {
        let mut wanted = declared.remove(&key).unwrap_or_default();
        wanted.sort_by_key(|value| K::identity(value));
        let mut mine = held.remove(&key).unwrap_or_default();
        mine.sort_by(|&a, &b| K::identity(value(a)).cmp(&K::identity(value(b))));

        let mut unmatched = Vec::new();
        for want in wanted {
            let identity = K::identity(&want);
            let open = |index: usize| fate[index] == Fate::Open;
            let found = mine
                .iter()
                .position(|&index| open(index) && *value(index) == want)
                .or_else(|| {
                    mine.iter()
                        .position(|&index| open(index) && K::identity(value(index)) == identity)
                });
            match found {
                Some(position) => {
                    let index = mine.remove(position);
                    if *value(index) == want {
                        fate[index] = Fate::Kept;
                    } else {
                        fate[index] = Fate::Written;
                        steps.push(Step::Update {
                            owner: owner.clone(),
                            live: live[index].clone(),
                            want,
                        });
                    }
                }
                None => unmatched.push(want),
            }
        }

        let mut free = Vec::new();
        for want in unmatched {
            let identity = K::identity(&want);
            let same: Vec<usize> = (0..live.len())
                .filter(|&index| {
                    fate[index] == Fate::Open
                        && live[index].key == key
                        && live[index]
                            .value
                            .as_ref()
                            .is_some_and(|v| K::identity(v) == identity)
                })
                .collect();
            let twin = same.iter().copied().find(|&index| {
                live[index].ownership == (Ownership::Unmarked { annotated: false })
                    && live[index].value.as_ref() == Some(&want)
            });
            if let Some(index) = twin {
                fate[index] = Fate::Written;
                steps.push(Step::Adopt {
                    owner: owner.clone(),
                    live: live[index].clone(),
                });
                continue;
            }
            let Some(&index) = same.first() else {
                free.push(want);
                continue;
            };
            let why = match &live[index].ownership {
                Ownership::Unmarked { annotated: true }
                    if live[index].value.as_ref() == Some(&want) =>
                {
                    Why::Annotated
                }
                Ownership::Unmarked { .. } => Why::Differs,
                Ownership::Marked(other) => Why::Owned(other.clone()),
                Ownership::Unreadable(_) => Why::Unreadable,
            };
            blocked.push(Blocked {
                owner: owner.clone(),
                want,
                by: live[index].clone(),
                why,
            });
        }

        let mut leftover = mine.into_iter();
        for want in free {
            match leftover.next() {
                Some(index) => {
                    fate[index] = Fate::Written;
                    steps.push(Step::Update {
                        owner: owner.clone(),
                        live: live[index].clone(),
                        want,
                    });
                }
                None => creates.push(want),
            }
        }
        deletes.extend(leftover);
    }

    let obstacle = |fate: &[Fate], want: &K::Value| {
        let key = K::key(want);
        let stays = |index: &usize| fate[*index] != Fate::Deleted;
        let occupant = (0..live.len())
            .filter(stays)
            .find(|&index| live[index].key == key && fate[index] == Fate::Open);
        let excluder = (0..live.len())
            .filter(stays)
            .find(|&index| live[index].key != key && K::excludes(&key, &live[index].key));
        occupant
            .map(|index| (index, Why::Occupied))
            .or(excluder.map(|index| (index, Why::Excludes)))
    };

    // a delete at a name where this machine's create is blocked would leave
    // the name with neither the old record nor the new one
    let mut deleted = fate.clone();
    for &index in &deletes {
        deleted[index] = Fate::Deleted;
    }
    let blocked_names: BTreeSet<K::Name> = blocked
        .iter()
        .map(|blocked| &blocked.want)
        .chain(
            creates
                .iter()
                .filter(|want| obstacle(&deleted, want).is_some()),
        )
        .map(|want| K::name(&K::key(want)))
        .collect();

    let mut withheld = Vec::new();
    for index in deletes {
        let step = Step::Delete {
            owner: owner.clone(),
            live: live[index].clone(),
        };
        let why = if blocked_names.contains(&K::name(&live[index].key)) {
            Some(Withhold::Blocked)
        } else {
            breaker.cloned()
        };
        match why {
            Some(why) => {
                fate[index] = Fate::Written;
                withheld.push(Withheld { step, why });
            }
            None => {
                fate[index] = Fate::Deleted;
                steps.push(step);
            }
        }
    }

    for want in creates {
        match obstacle(&deleted, &want).or_else(|| obstacle(&fate, &want)) {
            Some((index, why)) => blocked.push(Blocked {
                owner: owner.clone(),
                want,
                by: live[index].clone(),
                why,
            }),
            None => steps.push(Step::Create {
                owner: owner.clone(),
                want,
            }),
        }
    }

    // adopts, then updates, then name by name with a name's deletes right
    // before its creates, so a type change leaves its name empty only between
    // two requests
    let order = |step: &Step<K>| {
        let (group, name, rank) = match step {
            Step::Adopt { .. } => (0, None, 0),
            Step::Update { .. } => (1, None, 0),
            Step::Delete { .. } => (2, Some(K::name(&step.key())), 0),
            Step::Create { .. } => (2, Some(K::name(&step.key())), 1),
        };
        let identity = match step {
            Step::Adopt { live, .. } | Step::Update { live, .. } | Step::Delete { live, .. } => {
                live.value.as_ref().map(K::identity)
            }
            Step::Create { want, .. } => Some(K::identity(want)),
        };
        (group, name, rank, step.key(), identity)
    };
    steps.sort_by_key(order);
    blocked.sort_by(|a, b| {
        (K::key(&a.want), K::identity(&a.want)).cmp(&(K::key(&b.want), K::identity(&b.want)))
    });

    let mut kept = Vec::new();
    let mut untouched = Vec::new();
    let mut foreign = Vec::new();
    for (object, fate) in live.into_iter().zip(fate) {
        match (fate, &object.ownership) {
            (Fate::Kept, _) => kept.push(object),
            (Fate::Written | Fate::Deleted, _) => {}
            (Fate::Open, Ownership::Unmarked { .. }) => untouched.push(object),
            (Fate::Open, _) => foreign.push(object),
        }
    }
    for list in [&mut kept, &mut untouched, &mut foreign] {
        list.sort_by(|a, b| (&a.key, &a.born).cmp(&(&b.key, &b.born)));
    }

    Plan {
        steps,
        blocked,
        withheld,
        kept,
        untouched,
        foreign,
    }
}
