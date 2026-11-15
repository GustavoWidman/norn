//! the shapes every kind of cloudflare object shares.

use crate::model::Owner;

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
