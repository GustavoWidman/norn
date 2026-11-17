use std::fmt::{Debug, Display};

use chrono::{DateTime, Utc};

use crate::{
    cloudflare::{Api, ApiError, Capability, Write},
    model::HostName,
};

pub(crate) trait Kind: Sized + Clone + Debug {
    const NAME: &'static str;

    type Scope: Ord + Clone + Display + Debug;
    type Place: Clone + Debug;
    type Key: Ord + Clone + Display + Debug;
    type Identity: Ord + Clone + Debug;
    type Value: Eq + Clone + Display + Debug;
    type Handle: Clone + Debug;

    fn key(value: &Self::Value) -> Self::Key;
    fn identity(value: &Self::Value) -> Self::Identity;

    fn excludes(want: &Self::Key, other: &Self::Key) -> bool;

    fn read<C: Capability>(
        api: &Api<C>,
        scope: &Self::Scope,
    ) -> Result<(Self::Place, Vec<Live<Self>>), ApiError>;

    fn write(api: &Api<Write>, place: &Self::Place, step: &Step<Self>) -> Result<(), ApiError>;
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
    Marked(HostName),
    Unmarked { annotated: bool },
    Unreadable(String),
}

#[derive(Clone, Debug)]
pub(crate) struct Want<K: Kind> {
    pub scope: K::Scope,
    pub owner: HostName,
    pub value: K::Value,
}

#[derive(Clone, Debug)]
pub(crate) enum Step<K: Kind> {
    Adopt {
        owner: HostName,
        live: Live<K>,
    },
    Update {
        owner: HostName,
        live: Live<K>,
        want: K::Value,
    },
    Delete {
        owner: HostName,
        live: Live<K>,
    },
    Create {
        owner: HostName,
        want: K::Value,
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
            Step::Adopt { .. } => Rung::Adopt,
            Step::Update { .. } | Step::Delete { .. } | Step::Create { .. } => Rung::Enforce,
        }
    }

    pub fn owner(&self) -> &HostName {
        match self {
            Step::Adopt { owner, .. }
            | Step::Update { owner, .. }
            | Step::Delete { owner, .. }
            | Step::Create { owner, .. } => owner,
        }
    }

    pub fn verb(&self) -> &'static str {
        match self {
            Step::Adopt { .. } => "adopt",
            Step::Update { .. } => "update",
            Step::Delete { .. } => "delete",
            Step::Create { .. } => "create",
        }
    }

    pub fn key(&self) -> K::Key {
        match self {
            Step::Adopt { live, .. } | Step::Update { live, .. } | Step::Delete { live, .. } => {
                live.key.clone()
            }
            Step::Create { want, .. } => K::key(want),
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
            Step::Adopt { live, .. } | Step::Delete { live, .. } => shown(live),
            Step::Update { live, want, .. } => format!("{} -> {want}", shown(live)),
            Step::Create { want, .. } => want.to_string(),
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct Blocked<K: Kind> {
    pub owner: HostName,
    pub want: K::Value,
    pub by: Live<K>,
    pub why: Why,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) enum Why {
    Differs,
    Annotated,
    Owned(HostName),
    Unreadable,
    Occupied,
    Excludes,
}

impl Why {
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
            Why::Owned(owner) => format!("{owner} owns an object with this identity"),
            Why::Unreadable => {
                "an object with this identity carries a mark this norn cannot read".into()
            }
            Why::Occupied => {
                "an object norn does not own sits in this slot; adopt or remove it first".into()
            }
            Why::Excludes => {
                "a CNAME shares its name with nothing, and something else is there".into()
            }
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct Plan<K: Kind> {
    pub steps: Vec<Step<K>>,
    pub blocked: Vec<Blocked<K>>,
    pub withheld: Vec<Step<K>>,
    pub kept: Vec<Live<K>>,
    pub untouched: Vec<Live<K>>,
    pub foreign: Vec<Live<K>>,
}
