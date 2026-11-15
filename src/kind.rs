//! the contract a kind of cloudflare object fills in, and the kind-generic
//! shapes the planner, the run and the report share.
//!
//! a kind describes its objects: where they live, which slot one fills, what
//! tells two in a slot apart, and how cloudflare stores them. a kind never
//! decides what to do with them: [crate::plan::plan] does, for every kind,
//! so the rule "a machine only touches objects carrying its own mark" is
//! written once.

use std::fmt::{Debug, Display};

use chrono::{DateTime, Utc};

use crate::{
    cloudflare::{Api, ApiError, Capability, Write},
    model::Owner,
};

pub(crate) trait Kind: Sized + Clone + Debug {
    /// how logs and reports name the kind: `dns record`
    const NAME: &'static str;

    /// where objects live and are listed together: a zone, the account
    type Scope: Ord + Clone + Display + Debug;
    /// the scope as cloudflare addresses it (dns: the zone's id), learned by
    /// [Kind::read] and handed back to [Kind::write]
    type Place: Clone + Debug;
    /// the slot an object fills in its scope (dns: name and type), also for
    /// shapes norn does not model, so they still occupy their slot
    type Key: Ord + Clone + Display + Debug;
    /// what tells apart objects that share a slot (dns: the address, target or text)
    type Identity: Ord + Clone + Debug;
    /// an object's complete state, declared or parsed from cloudflare. `==`
    /// means no write is needed, so its parser folds every spelling cloudflare
    /// treats as equal into one value.
    type Value: Eq + Clone + Display + Debug;
    /// what a write addresses (dns: the record's id)
    type Handle: Clone + Debug;

    fn key(value: &Self::Value) -> Self::Key;
    fn identity(value: &Self::Value) -> Self::Identity;

    /// whether an object wanted in slot `want` cannot be created while some
    /// object occupies the different slot `other`. dns: a CNAME shares its
    /// name with nothing.
    fn excludes(want: &Self::Key, other: &Self::Key) -> bool;

    /// every object in `scope`, its mark read into an [Ownership]
    fn read<C: Capability>(
        api: &Api<C>,
        scope: &Self::Scope,
    ) -> Result<(Self::Place, Vec<Live<Self>>), ApiError>;

    /// carries out one step, writing the step's owner into the object's mark
    /// in the same request as its content. only a writer can call it.
    fn write(api: &Api<Write>, place: &Self::Place, step: &Step<Self>) -> Result<(), ApiError>;
}

/// an object as cloudflare reports it
#[derive(Clone, Debug)]
pub(crate) struct Live<K: Kind> {
    pub handle: K::Handle,
    pub key: K::Key,
    /// `None` for a shape norn does not model, such as an SRV record. such
    /// an object is never adopted, updated or deleted, whatever its mark
    /// says; it only occupies its slot.
    pub value: Option<K::Value>,
    pub ownership: Ownership,
    /// the oldest object wins a tie between duplicates, so every machine
    /// keeps the same one and deletes the same others
    pub born: Born,
}

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub(crate) struct Born {
    pub created: DateTime<Utc>,
    pub id: String,
}

/// what an object's mark says
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) enum Ownership {
    Marked(Owner),
    /// no mark. `annotated` when it carries someone's note, which adopting
    /// would overwrite, so an annotated object is never adopted.
    Unmarked {
        annotated: bool,
    },
    /// starts like a mark but does not parse, such as a later version's.
    /// never touched, so an older binary cannot overwrite a newer mark.
    Unreadable(String),
}

/// one declared object, owned by `owner`, in `scope`
#[derive(Clone, Debug)]
pub(crate) struct Want<K: Kind> {
    pub scope: K::Scope,
    pub owner: Owner,
    pub value: K::Value,
}

/// one write the planner decided on. the variant fixes which mode may carry
/// it out ([Step::rung]); no kind chooses that.
#[derive(Clone, Debug)]
pub(crate) enum Step<K: Kind> {
    /// stamp `owner`'s mark on an unmarked object that already equals the
    /// declaration; nothing else about it changes
    Adopt {
        owner: Owner,
        live: Live<K>,
    },
    /// rewrite one of `owner`'s objects whole, mark included
    Update {
        owner: Owner,
        live: Live<K>,
        want: K::Value,
    },
    Delete {
        owner: Owner,
        live: Live<K>,
    },
    /// a new object, its content and mark in one request
    Create {
        owner: Owner,
        want: K::Value,
    },
}

/// the least mode that carries a step out: adopt carries adopts, enforce carries everything
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub(crate) enum Rung {
    Adopt,
    Enforce,
}

/// how far a writing run goes
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Ladder {
    /// only stamp marks on unmarked objects that already equal their declaration
    Adopt,
    /// create, update and delete marked objects, and adopt as above: an exact
    /// unmarked twin would otherwise block a create forever
    Enforce,
}

impl Ladder {
    /// the single check between adopt and enforce; the rung comes from the
    /// generic step variant, so no kind can label an update as an adopt
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

    pub fn owner(&self) -> &Owner {
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

    /// the slot the step writes
    pub fn key(&self) -> K::Key {
        match self {
            Step::Adopt { live, .. } | Step::Update { live, .. } | Step::Delete { live, .. } => {
                live.key.clone()
            }
            Step::Create { want, .. } => K::key(want),
        }
    }

    /// `old -> new` for an update, the value otherwise
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

/// a declaration no mode will write until a person acts
#[derive(Clone, Debug)]
pub(crate) struct Blocked<K: Kind> {
    pub owner: Owner,
    pub want: K::Value,
    pub by: Live<K>,
    pub why: Why,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) enum Why {
    /// an unmarked object has the same identity but other attributes differ
    Differs,
    /// an unmarked object equals the declaration but carries someone's note
    Annotated,
    /// another owner's mark is on an object with the same identity, e.g. a
    /// name moving between machines whose old owner has not released it yet
    Owned(Owner),
    /// an object with the same identity carries a mark this binary cannot read
    Unreadable,
    /// something this run does not own sits in the slot a create would add
    /// to; enforce never adds a record beside a hand-made one
    Occupied,
    /// [Kind::excludes] holds against an object this run does not remove
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

/// what one scope's live state and declarations come to
#[derive(Clone, Debug)]
pub(crate) struct Plan<K: Kind> {
    /// in execution order: adopts, updates, deletes, creates. a delete frees a
    /// name a later create needs, as when a name turns from A into CNAME.
    pub steps: Vec<Step<K>>,
    pub blocked: Vec<Blocked<K>>,
    /// deletes past the run's cap, left for a person to look at
    pub withheld: Vec<Step<K>>,
    /// owned objects that already equal their declaration
    pub kept: Vec<Live<K>>,
    /// unmarked objects this run neither adopts nor may touch
    pub untouched: Vec<Live<K>>,
    /// objects marked by owners outside this run, marked unreadably, or of
    /// shapes norn does not model
    pub foreign: Vec<Live<K>>,
}
