//! where a record's owner is written: its comment, the only free-text field
//! cloudflare's free plan gives a record (one per record, at most 100 bytes;
//! tags are paid).

use crate::{
    kind::{Mark, Ownership, Stamp},
    model::{HostName, Revision},
};

const PREFIX: &str = "norn ";
const RETIRING: &str = " delete";

pub(super) fn read(comment: Option<&str>) -> Ownership {
    let comment = match comment {
        None | Some("") => return Ownership::Unmarked { annotated: false },
        Some(comment) => comment,
    };
    let Some(rest) = comment.strip_prefix(PREFIX) else {
        return Ownership::Unmarked { annotated: true };
    };
    let read = if let Some(host) = rest.strip_prefix("v1 host=") {
        HostName::parse(host).ok().map(Ownership::Host)
    } else if let Some(stamp) = rest.strip_prefix("v1 fleet t=") {
        let (revision, retiring) = match stamp.strip_suffix(RETIRING) {
            Some(revision) => (revision, true),
            None => (stamp, false),
        };
        revision
            .parse::<u64>()
            .ok()
            .filter(|parsed| parsed.to_string() == revision)
            .map(|revision| {
                Ownership::Fleet(Stamp {
                    revision: Revision(revision),
                    retiring,
                })
            })
    } else {
        None
    };
    read.unwrap_or_else(|| Ownership::Unreadable(comment.to_owned()))
}

/// `retiring` marks a shared record its configuration no longer declares
pub(super) fn write(mark: &Mark, retiring: bool) -> String {
    match mark {
        Mark::Host(host) => format!("{PREFIX}v1 host={host}"),
        Mark::Fleet(revision) => {
            let retiring = if retiring { RETIRING } else { "" };
            format!("{PREFIX}v1 fleet t={revision}{retiring}")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_written_mark_reads_back_as_its_owner() {
        let owner = HostName::parse(&"a".repeat(63)).unwrap();
        let mark = write(&Mark::Host(owner.clone()), false);
        assert!(mark.len() <= 100);
        assert_eq!(read(Some(&mark)), Ownership::Host(owner));
    }

    #[test]
    fn a_fleet_mark_reads_back_as_its_stamp() {
        for retiring in [false, true] {
            let mark = write(&Mark::Fleet(Revision(u64::MAX)), retiring);
            assert!(mark.len() <= 100);
            assert_eq!(
                read(Some(&mark)),
                Ownership::Fleet(Stamp {
                    revision: Revision(u64::MAX),
                    retiring
                })
            );
        }
        assert_eq!(
            write(&Mark::Fleet(Revision(1794942959)), true),
            "norn v1 fleet t=1794942959 delete"
        );
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
            "norn v1 host=Web",
            "norn v1 fleet",
            "norn v1 fleet ",
            "norn v1 fleet t=",
            "norn v1 fleet t=012",
            "norn v1 fleet t=+12",
            "norn v1 fleet t=12 remove",
            "norn v1 fleet t=12 delete ",
        ] {
            assert_eq!(
                read(Some(comment)),
                Ownership::Unreadable(comment.into()),
                "{comment}"
            );
        }
    }
}
