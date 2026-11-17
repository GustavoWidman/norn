use std::fmt::{Debug, Display};

use chrono::{DateTime, Utc};

use crate::{
    cloudflare::{Api, ApiError, Capability, Write},
    gate::{Deletes, Hold},
    model::{HostName, Owner, Revision},
};

pub(crate) trait Kind: Sized + Clone + Debug {
    const NAME: &'static str;

    type Scope: Ord + Clone + Display + Debug;
    type Place: Clone + Debug;
    type Key: Ord + Clone + Display + Debug;
    type Name: Ord + Clone + Debug;
    type Identity: Ord + Clone + Debug;
    type Value: Eq + Clone + Display + Debug;
    type Handle: Clone + PartialEq + Debug;

    fn key(value: &Self::Value) -> Self::Key;
    fn name(key: &Self::Key) -> Self::Name;
    fn identity(value: &Self::Value) -> Self::Identity;

    fn excludes(want: &Self::Key, other: &Self::Key) -> bool;

    fn read<C: Capability>(
        api: &Api<C>,
        scope: &Self::Scope,
    ) -> Result<(Self::Place, Vec<Live<Self>>), ApiError>;

    /// sends one step; every object it creates or rewrites carries `mark`
    fn write(
        api: &Api<Write>,
        place: &Self::Place,
        step: &Step<Self>,
        mark: &Mark,
    ) -> Result<Sent, ApiError>;
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Sent {
    Applied,
    /// another machine wrote the same shared object first
    Raced,
}

#[derive(Clone, Debug)]
pub(crate) struct Live<K: Kind> {
    pub handle: K::Handle,
    pub key: K::Key,
    pub value: Option<K::Value>,
    pub ownership: Ownership,
    pub born: Born,
}

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub(crate) struct Born {
    pub created: DateTime<Utc>,
    pub id: String,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) enum Ownership {
    Host(HostName),
    Fleet(Stamp),
    Unmarked { annotated: bool },
    Unreadable(String),
}

impl Ownership {
    pub fn owner(&self) -> Option<Owner> {
        match self {
            Ownership::Host(host) => Some(Owner::Host(host.clone())),
            Ownership::Fleet(_) => Some(Owner::Fleet),
            Ownership::Unmarked { .. } | Ownership::Unreadable(_) => None,
        }
    }
}

/// a shared object's mark: the revision of the configuration that last wrote
/// it, and whether that configuration dropped it
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Stamp {
    pub revision: Revision,
    pub retiring: bool,
}

/// the mark a write leaves on what it creates or rewrites
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) enum Mark {
    Host(HostName),
    Fleet(Revision),
}

#[derive(Clone, Debug)]
pub(crate) struct Want<K: Kind> {
    pub scope: K::Scope,
    pub owner: Owner,
    pub value: K::Value,
}

#[derive(Clone, Debug)]
pub(crate) enum Step<K: Kind> {
    Adopt {
        owner: Owner,
        live: Live<K>,
    },
    Update {
        owner: Owner,
        live: Live<K>,
        want: K::Value,
    },
    Delete {
        owner: Owner,
        live: Live<K>,
    },
    Create {
        owner: Owner,
        want: K::Value,
    },
    /// marks a shared object no longer declared as retiring; it keeps serving
    Retire {
        owner: Owner,
        live: Live<K>,
    },
    /// gives a retiring shared object that is declared again its plain mark
    Restore {
        owner: Owner,
        live: Live<K>,
    },
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub(crate) enum Rung {
    Adopt,
    Enforce,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Ladder {
    Adopt,
    Enforce,
}

impl Ladder {
    pub fn permits(self, rung: Rung) -> bool {
        match self {
            Ladder::Adopt => rung == Rung::Adopt,
            Ladder::Enforce => true,
        }
    }
}

impl<K: Kind> Step<K> {
    pub fn rung(&self) -> Rung {
        match self {
            Step::Adopt { .. } | Step::Restore { .. } => Rung::Adopt,
            Step::Update { .. }
            | Step::Delete { .. }
            | Step::Create { .. }
            | Step::Retire { .. } => Rung::Enforce,
        }
    }

    pub fn owner(&self) -> &Owner {
        match self {
            Step::Adopt { owner, .. }
            | Step::Update { owner, .. }
            | Step::Delete { owner, .. }
            | Step::Create { owner, .. }
            | Step::Retire { owner, .. }
            | Step::Restore { owner, .. } => owner,
        }
    }

    pub fn verb(&self) -> &'static str {
        match self {
            Step::Adopt { .. } => "adopt",
            Step::Update { .. } => "update",
            Step::Delete { .. } => "delete",
            Step::Create { .. } => "create",
            Step::Retire { .. } => "retire",
            Step::Restore { .. } => "restore",
        }
    }

    pub fn live(&self) -> Option<&Live<K>> {
        match self {
            Step::Adopt { live, .. }
            | Step::Update { live, .. }
            | Step::Delete { live, .. }
            | Step::Retire { live, .. }
            | Step::Restore { live, .. } => Some(live),
            Step::Create { .. } => None,
        }
    }

    pub fn key(&self) -> K::Key {
        match (self, self.live()) {
            (_, Some(live)) => live.key.clone(),
            (Step::Create { want, .. }, None) => K::key(want),
            (_, None) => unreachable!("only a create has no live object"),
        }
    }

    pub fn describe(&self) -> String {
        let shown = |live: &Live<K>| {
            live.value
                .as_ref()
                .map(ToString::to_string)
                .unwrap_or_default()
        };
        match self {
            Step::Adopt { live, .. }
            | Step::Delete { live, .. }
            | Step::Retire { live, .. }
            | Step::Restore { live, .. } => shown(live),
            Step::Update { live, want, .. } => format!("{} -> {want}", shown(live)),
            Step::Create { want, .. } => want.to_string(),
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct Blocked<K: Kind> {
    pub owner: Owner,
    pub want: K::Value,
    pub by: Live<K>,
    pub why: Why,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) enum Why {
    Differs,
    Annotated,
    Owned(Owner),
    Unreadable,
    Occupied,
    Excludes,
    /// a shared object this plan retires excludes it; it is created in the
    /// run that deletes that object
    Retiring,
}

impl Why {
    /// whether this is the normal course of a rollout, rather than something
    /// a person should look at
    pub fn holds(&self) -> bool {
        matches!(self, Why::Retiring)
    }

    pub fn explain(&self) -> String {
        match self {
            Why::Differs => {
                "an unmarked object with this identity differs; align the declaration or the object"
                    .into()
            }
            Why::Annotated => {
                "an unmarked object equals it but carries a note, which adopting would overwrite"
                    .into()
            }
            Why::Owned(Owner::Host(owner)) => format!("{owner} owns an object with this identity"),
            Why::Owned(Owner::Fleet) => {
                "an object with this identity is shared by the whole fleet".into()
            }
            Why::Unreadable => {
                "an object with this identity carries a mark this norn cannot read".into()
            }
            Why::Occupied => {
                "an object norn does not own sits in this slot; adopt or remove it first".into()
            }
            Why::Excludes => {
                "a CNAME shares its name with nothing, and something else is there".into()
            }
            Why::Retiring => {
                "it waits for the retiring shared record in its way, and is created in the run that deletes it".into()
            }
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct Withheld<K: Kind> {
    pub step: Step<K>,
    pub why: Withhold,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) enum Withhold {
    Breaker {
        planned: usize,
        max: usize,
    },
    Blocked,
    /// the fleet gate holds every shared step this run
    Fleet(Hold),
    /// a retiring shared object waits until the whole fleet runs the
    /// configuration that dropped it, which only the rollout file can tell
    Retiring {
        dropped: Revision,
        deletes: Deletes,
    },
}

impl Withhold {
    /// whether this is the normal course of a rollout, rather than something
    /// a person should look at
    pub fn holds(&self) -> bool {
        matches!(self, Withhold::Fleet(_) | Withhold::Retiring { .. })
    }

    pub fn explain(&self) -> String {
        match self {
            Withhold::Breaker { planned, max } => format!(
                "this run plans {planned} deletes, more than its cap of {max}, so it sends none; to let them through, raise services.norn.maxDeletes (--max-deletes) for one deploy"
            ),
            Withhold::Blocked => {
                "something this machine wants at this name is blocked, so what is there keeps serving"
                    .into()
            }
            Withhold::Fleet(hold) => hold.explain(),
            Withhold::Retiring {
                dropped,
                deletes: Deletes::Never,
            } => format!(
                "no rollout file is configured, so norn never deletes it; delete it by hand once every machine runs t={dropped}, the configuration that dropped it"
            ),
            Withhold::Retiring {
                dropped,
                deletes: Deletes::UpTo(rolled_out),
            } => format!(
                "it is deleted once the whole fleet runs t={dropped}, the configuration that dropped it; the fleet runs t={rolled_out}"
            ),
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct Plan<K: Kind> {
    pub owner: Owner,
    pub steps: Vec<Step<K>>,
    pub blocked: Vec<Blocked<K>>,
    pub withheld: Vec<Withheld<K>>,
    pub kept: Vec<Live<K>>,
    pub untouched: Vec<Live<K>>,
    pub foreign: Vec<Live<K>>,
}

impl<K: Kind> Plan<K> {
    /// withholds every step, for a run that may not write this plan's
    /// objects. a retiring object then waits for the same reason, not for the
    /// rollout
    pub fn hold(&mut self, why: Withhold) {
        for withheld in &mut self.withheld {
            if matches!(withheld.why, Withhold::Retiring { .. }) {
                withheld.why = why.clone();
            }
        }
        let steps = std::mem::take(&mut self.steps);
        self.withheld.splice(
            0..0,
            steps.into_iter().map(|step| Withheld {
                step,
                why: why.clone(),
            }),
        );
    }

    /// whether this plan writes, withholds or keeps the object
    pub fn claims(&self, handle: &K::Handle) -> bool {
        self.steps
            .iter()
            .chain(self.withheld.iter().map(|withheld| &withheld.step))
            .filter_map(Step::live)
            .chain(&self.kept)
            .any(|live| live.handle == *handle)
    }
}
