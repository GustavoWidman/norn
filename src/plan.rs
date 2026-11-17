use std::collections::{BTreeMap, BTreeSet};

use crate::{
    gate::Deletes,
    kind::{Blocked, Kind, Live, Ownership, Plan, Step, Want, Why, Withheld, Withhold},
    model::Owner,
};

#[derive(Clone, Copy, PartialEq, Debug)]
enum Fate {
    Open,
    Kept,
    Written,
    /// a shared object no longer declared, retired or waiting to be deleted;
    /// it keeps serving until then
    Retiring,
    Deleted,
}

#[derive(PartialEq, Eq, PartialOrd, Ord)]
enum Turn<N> {
    Mark,
    Update,
    AtName(N, Change),
}

#[derive(PartialEq, Eq, PartialOrd, Ord)]
enum Change {
    Delete,
    Create,
}

/// brings `owner`'s objects in line with `wants`. objects with any other
/// owner are someone else's. a shared object no longer declared is retired
/// first, and deleted only once `deletes` permits its stamp.
pub(crate) fn plan<K: Kind>(
    owner: &Owner,
    wants: Vec<Want<K>>,
    mut live: Vec<Live<K>>,
    breaker: Option<&Withhold>,
    deletes: Deletes,
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
        if object.value.is_some() && object.ownership.owner().as_ref() == Some(owner) {
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
    let mut withheld = Vec::new();
    let mut doomed = Vec::new();
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
                    let retiring = matches!(
                        live[index].ownership,
                        Ownership::Fleet(stamp) if stamp.retiring
                    );
                    if *value(index) == want && retiring {
                        fate[index] = Fate::Written;
                        steps.push(Step::Restore {
                            owner: owner.clone(),
                            live: live[index].clone(),
                        });
                    } else if *value(index) == want {
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
                Ownership::Host(other) => Why::Owned(Owner::Host(other.clone())),
                Ownership::Fleet(_) => Why::Owned(Owner::Fleet),
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
        for index in leftover {
            match live[index].ownership {
                Ownership::Fleet(stamp) if !stamp.retiring => {
                    fate[index] = Fate::Retiring;
                    steps.push(Step::Retire {
                        owner: owner.clone(),
                        live: live[index].clone(),
                    });
                }
                Ownership::Fleet(stamp) if !deletes.permit(stamp.revision) => {
                    fate[index] = Fate::Retiring;
                    withheld.push(Withheld {
                        step: Step::Delete {
                            owner: owner.clone(),
                            live: live[index].clone(),
                        },
                        why: Withhold::Retiring {
                            dropped: stamp.revision,
                            deletes,
                        },
                    });
                }
                _ => doomed.push(index),
            }
        }
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
        let excludes = |index: usize| match fate[index] {
            Fate::Retiring => (index, Why::Retiring),
            _ => (index, Why::Excludes),
        };
        occupant
            .map(|index| (index, Why::Occupied))
            .or(excluder.map(excludes))
    };

    let mut deleted = fate.clone();
    for &index in &doomed {
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

    for index in doomed {
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

    let order = |step: &Step<K>| {
        let turn = match step {
            Step::Adopt { .. } | Step::Retire { .. } | Step::Restore { .. } => Turn::Mark,
            Step::Update { .. } => Turn::Update,
            Step::Delete { .. } => Turn::AtName(K::name(&step.key()), Change::Delete),
            Step::Create { .. } => Turn::AtName(K::name(&step.key()), Change::Create),
        };
        let identity = match (step, step.live()) {
            (_, Some(live)) => live.value.as_ref().map(K::identity),
            (Step::Create { want, .. }, None) => Some(K::identity(want)),
            (_, None) => None,
        };
        (turn, step.key(), identity)
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
            (Fate::Written | Fate::Retiring | Fate::Deleted, _) => {}
            (Fate::Open, Ownership::Unmarked { .. }) => untouched.push(object),
            (Fate::Open, _) => foreign.push(object),
        }
    }
    for list in [&mut kept, &mut untouched, &mut foreign] {
        list.sort_by(|a, b| (&a.key, &a.born).cmp(&(&b.key, &b.born)));
    }

    Plan {
        owner: owner.clone(),
        steps,
        blocked,
        withheld,
        kept,
        untouched,
        foreign,
    }
}
