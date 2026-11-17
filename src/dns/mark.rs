//! where a record's owner is written: its comment, the only free-text field
//! cloudflare's free plan gives a record (one per record, at most 100 bytes;
//! tags are paid).

use crate::{
    kind::Ownership,
    model::{HostName, Owner},
};

const PREFIX: &str = "norn ";

pub(super) fn read(comment: Option<&str>) -> Ownership {
    let comment = match comment {
        None | Some("") => return Ownership::Unmarked { annotated: false },
        Some(comment) => comment,
    };
    let Some(rest) = comment.strip_prefix(PREFIX) else {
        return Ownership::Unmarked { annotated: true };
    };
    match rest {
        "v1 fleet" => Ownership::Marked(Owner::Fleet),
        _ => match rest.strip_prefix("v1 host=").map(HostName::parse) {
            Some(Ok(host)) => Ownership::Marked(Owner::Host(host)),
            _ => Ownership::Unreadable(comment.to_owned()),
        },
    }
}

pub(super) fn write(owner: &Owner) -> String {
    match owner {
        Owner::Fleet => format!("{PREFIX}v1 fleet"),
        Owner::Host(host) => format!("{PREFIX}v1 host={host}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_written_mark_reads_back_as_its_owner() {
        for owner in [
            Owner::Fleet,
            Owner::Host(HostName::parse(&"a".repeat(63)).unwrap()),
        ] {
            let mark = write(&owner);
            assert!(mark.len() <= 100);
            assert_eq!(read(Some(&mark)), Ownership::Marked(owner));
        }
    }

    #[test]
    fn a_note_is_unmarked_and_annotated() {
        assert_eq!(
            read(Some("Dummy IP for redirect rule to work")),
            Ownership::Unmarked { annotated: true }
        );
        assert_eq!(read(Some("")), Ownership::Unmarked { annotated: false });
        assert_eq!(read(None), Ownership::Unmarked { annotated: false });
    }

    #[test]
    fn a_mark_norn_cannot_parse_is_unreadable() {
        for comment in [
            "norn v2 host=web",
            "norn v1 host=fleet",
            "norn v1 host=Web",
            "norn v1 fleet ",
        ] {
            assert_eq!(
                read(Some(comment)),
                Ownership::Unreadable(comment.into()),
                "{comment}"
            );
        }
    }
}
