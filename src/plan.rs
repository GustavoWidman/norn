//! the one place that decides what to write, for every kind. pure: live
//! state and declarations in, a [Plan] out, so the whole policy is tested
//! against recorded cloudflare state without a network.

use std::collections::BTreeMap;

use crate::{
    kind::{Blocked, Kind, Live, Ownership, Plan, Step, Want, Why},
    model::{Owner, Owners},
};

/// the most deletes one run carries out. a render that empties a desired
/// file then costs a handful of records, and the rest wait as withheld.
pub(crate) const MAX_DELETES: usize = 10;

/// what becomes of a live object once the plan is applied
#[derive(Clone, PartialEq, Debug)]
enum Fate {
    /// nobody here writes it
    Open,
    /// `owner`'s, as it already is
    Kept(Owner),
    /// `owner`'s after a step: adopted, updated, or a withheld delete
    Written(Owner),
    Deleted,
}

impl Fate {
    fn owner(&self) -> Option<&Owner> {
        match self {
            Fate::Kept(owner) | Fate::Written(owner) => Some(owner),
            Fate::Open | Fate::Deleted => None,
        }
    }
}

/// plans one scope. `deletes_left` is what remains of the run's delete cap,
/// shared by every scope of the run; deletes past it are withheld.
///
/// invariants, each a test in `crate::test::plan`:
/// - an update or delete only ever names an object marked by one of `owners`;
/// - an adopt only ever names an unannotated unmarked object whose value
///   equals the declaration;
/// - a create never lands in a slot holding an object this run does not own,
///   nor beside an object its key excludes;
/// - planning the state that applying this plan leaves yields no steps;
/// - the result does not depend on the order of `wants` or `live`, so every
///   machine on one revision plans the same writes for the fleet's objects.
pub(crate) fn plan<K: Kind>(
    owners: &Owners,
    wants: Vec<Want<K>>,
    mut live: Vec<Live<K>>,
    deletes_left: &mut usize,
) -> Plan<K> {
    live.sort_by(|a, b| a.born.cmp(&b.born));
    let mut fate = vec![Fate::Open; live.len()];

    let mut declared: BTreeMap<(Owner, K::Key), Vec<K::Value>> = BTreeMap::new();
    for want in wants {
        debug_assert!(owners.contains(&want.owner));
        declared
            .entry((want.owner, K::key(&want.value)))
            .or_default()
            .push(want.value);
    }
    let mut held: BTreeMap<(Owner, K::Key), Vec<usize>> = BTreeMap::new();
    for (index, object) in live.iter().enumerate() {
        if let (Ownership::Marked(owner), Some(_)) = (&object.ownership, &object.value)
            && owners.contains(owner)
        {
            held.entry((owner.clone(), object.key.clone()))
                .or_default()
                .push(index);
        }
    }
    let slots: Vec<(Owner, K::Key)> = {
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

    for (owner, key) in slots {
        let mut wanted = declared
            .remove(&(owner.clone(), key.clone()))
            .unwrap_or_default();
        wanted.sort_by_key(|value| K::identity(value));
        let mut mine = held
            .remove(&(owner.clone(), key.clone()))
            .unwrap_or_default();
        mine.sort_by(|&a, &b| K::identity(value(a)).cmp(&K::identity(value(b))));

        // exact identities first: kept, or rewritten in place when another
        // attribute moved. the oldest of duplicates is matched, so every
        // machine deletes the same younger copies.
        let mut unmatched = Vec::new();
        for want in wanted {
            let identity = K::identity(&want);
            let found = mine.iter().position(|&index| {
                fate[index] == Fate::Open && K::identity(value(index)) == identity
            });
            match found {
                Some(position) => {
                    let index = mine.remove(position);
                    if *value(index) == want {
                        fate[index] = Fate::Kept(owner.clone());
                    } else {
                        fate[index] = Fate::Written(owner.clone());
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

        // an object of the same identity this owner does not hold: adopt it
        // when it is an unannotated unmarked twin, otherwise wait for a person
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
                fate[index] = Fate::Written(owner.clone());
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

        // what is left pairs up in identity order: a changed address is one
        // rewrite, never a window without a record
        let mut leftover = mine.into_iter();
        for want in free {
            match leftover.next() {
                Some(index) => {
                    fate[index] = Fate::Written(owner.clone());
                    steps.push(Step::Update {
                        owner: owner.clone(),
                        live: live[index].clone(),
                        want,
                    });
                }
                None => creates.push((owner.clone(), want)),
            }
        }
        deletes.extend(leftover.map(|index| (owner.clone(), index)));
    }

    let mut withheld = Vec::new();
    for (owner, index) in deletes {
        let step = Step::Delete {
            owner: owner.clone(),
            live: live[index].clone(),
        };
        if *deletes_left == 0 {
            fate[index] = Fate::Written(owner);
            withheld.push(step);
        } else {
            *deletes_left -= 1;
            fate[index] = Fate::Deleted;
            steps.push(step);
        }
    }

    for (owner, want) in creates {
        let key = K::key(&want);
        let stays = |index: &usize| fate[*index] != Fate::Deleted;
        let occupant = (0..live.len())
            .filter(stays)
            .find(|&index| live[index].key == key && fate[index].owner() != Some(&owner));
        let excluder = (0..live.len())
            .filter(stays)
            .find(|&index| live[index].key != key && K::excludes(&key, &live[index].key));
        match (occupant, excluder) {
            (Some(index), _) => {
                blocked.push(Blocked {
                    owner,
                    want,
                    by: live[index].clone(),
                    why: Why::Occupied,
                });
            }
            (None, Some(index)) => {
                blocked.push(Blocked {
                    owner,
                    want,
                    by: live[index].clone(),
                    why: Why::Excludes,
                });
            }
            (None, None) => steps.push(Step::Create { owner, want }),
        }
    }

    let order = |step: &Step<K>| {
        let rank = match step {
            Step::Adopt { .. } => 0,
            Step::Update { .. } => 1,
            Step::Delete { .. } => 2,
            Step::Create { .. } => 3,
        };
        let identity = match step {
            Step::Adopt { live, .. } | Step::Update { live, .. } | Step::Delete { live, .. } => {
                live.value.as_ref().map(K::identity)
            }
            Step::Create { want, .. } => Some(K::identity(want)),
        };
        (rank, step.key(), identity)
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
            (Fate::Kept(_), _) => kept.push(object),
            (Fate::Written(_) | Fate::Deleted, _) => {}
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
