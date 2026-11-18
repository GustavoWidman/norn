//! whether this run may write the fleet's shared objects, of every kind at
//! once. machines run different revisions of the configuration during a
//! rollout, so a machine writes shared objects only while nothing says a
//! newer configuration is already in charge of them.

use std::{
    fmt,
    path::{Path, PathBuf},
    time::Duration,
};

use chrono::{DateTime, Utc};
use serde::Deserialize;

use crate::model::Revision;

/// the file a fleet's deploy tool writes, and how old its last check may be
#[derive(Clone, Debug)]
pub struct RolloutFile {
    pub path: PathBuf,
    pub max_age: Duration,
}

/// the newest configuration the whole reachable fleet runs
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) enum Rollout {
    NotConfigured,
    Unknown(String),
    At(Revision),
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) enum Permit {
    Write {
        revision: Revision,
        deletes: Deletes,
    },
    Hold(Hold),
}

impl fmt::Display for Permit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Permit::Write {
                revision,
                deletes: Deletes::Never,
            } => write!(
                f,
                "write at t={revision}; with no rollout file, no retiring object is deleted"
            ),
            Permit::Write {
                revision,
                deletes: Deletes::UpTo(rolled_out),
            } => write!(
                f,
                "write at t={revision}; retiring objects stamped up to t={rolled_out} are deleted"
            ),
            Permit::Hold(hold) => write!(f, "hold: {}", hold.explain()),
        }
    }
}

/// which retiring shared records a run may delete
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Deletes {
    Never,
    /// those stamped at or below the revision the whole fleet runs
    UpTo(Revision),
}

impl Deletes {
    pub fn permit(self, stamp: Revision) -> bool {
        match self {
            Deletes::Never => false,
            Deletes::UpTo(rolled_out) => stamp <= rolled_out,
        }
    }
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) enum Hold {
    Uncommitted,
    Unread,
    RolloutUnknown(String),
    Behind { rolled_out: Revision },
    Newer(Revision),
}

impl Hold {
    pub fn explain(&self) -> String {
        match self {
            Hold::Uncommitted => {
                "this configuration was built from an uncommitted tree, so it has no revision and writes no shared records".into()
            }
            Hold::Unread => {
                "a scope could not be read, so a newer configuration's shared objects may be there unseen".into()
            }
            Hold::RolloutUnknown(why) => format!("the fleet's rollout is unknown: {why}"),
            Hold::Behind { rolled_out } => format!(
                "the fleet has rolled out t={rolled_out}, newer than this configuration"
            ),
            Hold::Newer(revision) => format!(
                "a shared object carries t={revision}, newer than this configuration, whose machines write the shared objects"
            ),
        }
    }
}

/// `own` is this configuration's revision, `newest` the newest stamp on any
/// shared object of any kind this run read
pub(crate) fn permit(
    own: Option<Revision>,
    newest: Option<Revision>,
    rollout: &Rollout,
    any_scope_unread: bool,
) -> Permit {
    let Some(own) = own else {
        return Permit::Hold(Hold::Uncommitted);
    };
    if any_scope_unread {
        return Permit::Hold(Hold::Unread);
    }
    let deletes = match rollout {
        Rollout::NotConfigured => Deletes::Never,
        Rollout::Unknown(why) => return Permit::Hold(Hold::RolloutUnknown(why.clone())),
        Rollout::At(rolled_out) if own < *rolled_out => {
            return Permit::Hold(Hold::Behind {
                rolled_out: *rolled_out,
            });
        }
        Rollout::At(rolled_out) => Deletes::UpTo(*rolled_out),
    };
    match newest {
        Some(newest) if newest > own => Permit::Hold(Hold::Newer(newest)),
        _ => Permit::Write {
            revision: own,
            deletes,
        },
    }
}

#[derive(Deserialize)]
struct Document {
    committed: u64,
    checked: i64,
}

impl Rollout {
    pub fn read(file: Option<&RolloutFile>, now: DateTime<Utc>) -> Self {
        let Some(file) = file else {
            return Rollout::NotConfigured;
        };
        match Self::parse(&file.path, now, file.max_age) {
            Ok(revision) => Rollout::At(revision),
            Err(why) => Rollout::Unknown(why),
        }
    }

    fn parse(path: &Path, now: DateTime<Utc>, max_age: Duration) -> Result<Revision, String> {
        let shown = path.display();
        let bytes = std::fs::read(path).map_err(|error| format!("{shown}: {error}"))?;
        let document: Document =
            serde_json::from_slice(&bytes).map_err(|error| format!("{shown}: {error}"))?;
        let age = (now.timestamp() - document.checked).max(0);
        let age = Duration::from_secs(age.unsigned_abs());
        if age > max_age {
            return Err(format!(
                "{shown} was last checked {} ago, longer than {}",
                humantime::format_duration(age),
                humantime::format_duration(max_age)
            ));
        }
        Ok(Revision(document.committed))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const OLD: Revision = Revision(100);
    const OWN: Revision = Revision(200);
    const NEW: Revision = Revision(300);

    fn unknown() -> Rollout {
        Rollout::Unknown("gone".into())
    }

    #[test]
    fn the_gate_checks_in_order() {
        use Hold::*;
        let write = |deletes| Permit::Write {
            revision: OWN,
            deletes,
        };
        let cases = [
            (None, Some(NEW), unknown(), true, Permit::Hold(Uncommitted)),
            (Some(OWN), Some(NEW), unknown(), true, Permit::Hold(Unread)),
            (
                Some(OWN),
                Some(NEW),
                unknown(),
                false,
                Permit::Hold(RolloutUnknown("gone".into())),
            ),
            (
                Some(OWN),
                Some(NEW),
                Rollout::At(NEW),
                false,
                Permit::Hold(Behind { rolled_out: NEW }),
            ),
            (
                Some(OWN),
                Some(NEW),
                Rollout::At(OLD),
                false,
                Permit::Hold(Newer(NEW)),
            ),
            (
                Some(OWN),
                Some(NEW),
                Rollout::NotConfigured,
                false,
                Permit::Hold(Newer(NEW)),
            ),
            (
                Some(OWN),
                Some(OWN),
                Rollout::NotConfigured,
                false,
                write(Deletes::Never),
            ),
            (
                Some(OWN),
                None,
                Rollout::At(OLD),
                false,
                write(Deletes::UpTo(OLD)),
            ),
            (
                Some(OWN),
                Some(OLD),
                Rollout::At(OWN),
                false,
                write(Deletes::UpTo(OWN)),
            ),
        ];
        for (own, newest, rollout, unread, expected) in cases {
            assert_eq!(
                permit(own, newest, &rollout, unread),
                expected,
                "{own:?} {newest:?} {rollout:?} {unread}"
            );
        }
    }

    #[test]
    fn deletes_reach_only_up_to_the_rollout() {
        assert!(!Deletes::Never.permit(OLD));
        assert!(Deletes::UpTo(OWN).permit(OLD));
        assert!(Deletes::UpTo(OWN).permit(OWN));
        assert!(!Deletes::UpTo(OWN).permit(NEW));
    }

    #[test]
    fn a_missing_garbled_or_stale_rollout_file_is_unknown() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rollout.json");
        let now = DateTime::from_timestamp(1_800_000_000, 0).unwrap();
        let file = RolloutFile {
            path: path.clone(),
            max_age: Duration::from_secs(1800),
        };
        let read = || Rollout::read(Some(&file), now);

        assert_eq!(Rollout::read(None, now), Rollout::NotConfigured);
        assert!(matches!(read(), Rollout::Unknown(why) if why.contains("rollout.json")));
        std::fs::write(&path, "{\"committed\": 1}").unwrap();
        assert!(matches!(read(), Rollout::Unknown(why) if why.contains("checked")));

        let written = |checked: i64| {
            std::fs::write(
                &path,
                format!(r#"{{"committed": 1799990000, "checked": {checked}, "fleet": ["web-1"]}}"#),
            )
            .unwrap();
        };
        written(1_800_000_000 - 1800);
        assert_eq!(read(), Rollout::At(Revision(1_799_990_000)));
        written(1_800_000_000 - 1801);
        assert!(matches!(read(), Rollout::Unknown(why) if why.contains("longer than 30m")));
    }
}
