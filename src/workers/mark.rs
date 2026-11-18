//! where a script's owner is written: its tags, which the script list
//! returns and which change without a new version of the code. cloudflare
//! keeps at most 10 tags on a script. norn's mark takes three, plus one for
//! every 205 bytes of patterns it lists:
//!
//! - `norn v1 fleet t=<revision>`, with ` delete` appended once retiring
//! - `norn v1 module <digest>`, the module it uploaded
//! - `norn v1 date <yyyy-mm-dd>`, the compatibility date it set
//! - `norn v1 routes <pattern> <pattern>`, the routes that run it
//! - `norn v1 bypass <pattern> <pattern>`, the routes beside it that run no
//!   worker
//!
//! the module and the date tell a run whether the script equals its
//! declaration without downloading it. the lists say which routes are the
//! fleet's, since a route has no field of its own to carry a mark.

use std::collections::BTreeSet;

use super::value::{CompatibilityDate, Digest, Module, Pattern, Script, ScriptName};
use crate::{
    kind::{Ownership, Stamp},
    model::Revision,
};

const PREFIX: &str = "norn ";
const FLEET: &str = "norn v1 fleet t=";
const RETIRING: &str = " delete";
const MODULE: &str = "norn v1 module ";
const DATE: &str = "norn v1 date ";
const ROUTES: &str = "norn v1 routes ";
const BYPASS: &str = "norn v1 bypass ";

/// the most tags cloudflare keeps on a script
const MOST: usize = 10;
/// the longest tag cloudflare is known to take
const LONGEST: usize = 205;

/// a script's tags as norn reads them
pub(super) struct Read {
    pub ownership: Ownership,
    pub value: Option<Script>,
    /// tags that are not norn's, kept on every write
    pub others: Vec<String>,
}

pub(super) fn read(name: &ScriptName, tags: &[String]) -> Read {
    let (marks, others): (Vec<&String>, Vec<&String>) =
        tags.iter().partition(|tag| tag.starts_with(PREFIX));
    let others = others.into_iter().cloned().collect();
    if marks.is_empty() {
        return Read {
            ownership: Ownership::Unmarked { annotated: false },
            value: None,
            others,
        };
    }
    match parse(name, &marks) {
        Some((stamp, script)) => Read {
            ownership: Ownership::Fleet(stamp),
            value: Some(script),
            others,
        },
        None => Read {
            ownership: Ownership::Unreadable(
                marks
                    .iter()
                    .map(|tag| tag.as_str())
                    .collect::<Vec<_>>()
                    .join(", "),
            ),
            value: None,
            others,
        },
    }
}

fn parse(name: &ScriptName, marks: &[&String]) -> Option<(Stamp, Script)> {
    let known = [FLEET, MODULE, DATE, ROUTES, BYPASS];
    if !marks
        .iter()
        .all(|tag| known.iter().any(|prefix| tag.starts_with(prefix)))
    {
        return None;
    }
    let one = |prefix: &str| match marks
        .iter()
        .filter_map(|tag| tag.strip_prefix(prefix))
        .collect::<Vec<_>>()
        .as_slice()
    {
        [only] => Some(*only),
        _ => None,
    };
    let listed = |prefix: &str| -> Option<BTreeSet<Pattern>> {
        marks
            .iter()
            .filter_map(|tag| tag.strip_prefix(prefix))
            .flat_map(|list| list.split(' '))
            .map(|pattern| (!pattern.is_empty()).then(|| Pattern::live(pattern)))
            .collect()
    };
    let stamp = one(FLEET)?;
    let (revision, retiring) = match stamp.strip_suffix(RETIRING) {
        Some(revision) => (revision, true),
        None => (stamp, false),
    };
    let revision = revision
        .parse::<u64>()
        .ok()
        .filter(|parsed| parsed.to_string() == revision)?;
    let script = Script {
        name: name.clone(),
        compatibility_date: CompatibilityDate::parse(one(DATE)?).ok()?,
        module: Module::live(Digest::parse(one(MODULE)?)?),
        routes: listed(ROUTES)?,
        bypass: listed(BYPASS)?,
    };
    let stamp = Stamp {
        revision: Revision(revision),
        retiring,
    };
    Some((stamp, script))
}

/// the tags a fleet script is written with: norn's mark, then `others`
pub(super) fn write(
    revision: Revision,
    retiring: bool,
    script: &Script,
    others: &[String],
) -> Vec<String> {
    let retiring = if retiring { RETIRING } else { "" };
    let mut tags = vec![
        format!("{FLEET}{revision}{retiring}"),
        format!("{MODULE}{}", script.module.digest),
        format!("{DATE}{}", script.compatibility_date),
    ];
    for (prefix, patterns) in [(ROUTES, &script.routes), (BYPASS, &script.bypass)] {
        for pattern in patterns.iter().map(Pattern::as_str) {
            match tags.last_mut() {
                Some(tag)
                    if tag.starts_with(prefix) && tag.len() + 1 + pattern.len() <= LONGEST =>
                {
                    tag.push(' ');
                    tag.push_str(pattern);
                }
                _ => tags.push(format!("{prefix}{pattern}")),
            }
        }
    }
    tags.extend(others.iter().cloned());
    tags
}

/// whether a declared script's mark fits in the tags cloudflare keeps
pub(crate) fn fits(script: &Script) -> Result<(), String> {
    let tags = write(Revision(u64::MAX), true, script, &[]);
    if let Some(long) = tags.iter().find(|tag| tag.len() > LONGEST) {
        return Err(format!(
            "its tag {long:?} is longer than the {LONGEST} bytes cloudflare is known to take"
        ));
    }
    if tags.len() > MOST {
        return Err(format!(
            "its routes and bypasses take {} tags, and cloudflare keeps {MOST} on a script; declare fewer",
            tags.len()
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn patterns(listed: &[&str]) -> BTreeSet<Pattern> {
        listed
            .iter()
            .map(|pattern| Pattern::live(pattern))
            .collect()
    }

    fn script(routes: &[&str], bypass: &[&str]) -> Script {
        Script {
            name: ScriptName::parse("coeus-offline").unwrap(),
            compatibility_date: CompatibilityDate::parse("2026-11-01").unwrap(),
            module: Module::live(Digest::of(b"export default {}")),
            routes: patterns(routes),
            bypass: patterns(bypass),
        }
    }

    #[test]
    fn a_written_mark_reads_back_as_its_stamp_and_script() {
        let others = vec!["team-web".to_owned()];
        let written = script(
            &["coeus.example.com/*"],
            &["coeus.example.com/api/*", "coeus.example.com/live/*"],
        );
        for retiring in [false, true] {
            let tags = write(Revision(u64::MAX), retiring, &written, &others);
            assert_eq!(
                tags[3..],
                [
                    "norn v1 routes coeus.example.com/*",
                    "norn v1 bypass coeus.example.com/api/* coeus.example.com/live/*",
                    "team-web"
                ]
            );
            let read = read(&written.name, &tags);
            assert_eq!(
                read.ownership,
                Ownership::Fleet(Stamp {
                    revision: Revision(u64::MAX),
                    retiring
                })
            );
            assert_eq!(read.value, Some(written.clone()));
            assert_eq!(read.others, others);
        }
    }

    #[test]
    fn long_lists_spill_into_more_tags_up_to_cloudflares_limit() {
        let many: Vec<String> = (0..60)
            .map(|n| format!("site-{n:02}.example.com/*"))
            .collect();
        let listed = |count: usize| many[..count].iter().map(String::as_str).collect::<Vec<_>>();
        let fitting = script(&listed(56), &[]);
        let tags = write(Revision(u64::MAX), true, &fitting, &[]);
        assert_eq!(tags.len(), MOST);
        assert!(tags.iter().all(|tag| tag.len() <= LONGEST), "{tags:#?}");
        assert_eq!(read(&fitting.name, &tags).value, Some(fitting.clone()));
        assert!(fits(&fitting).is_ok());
        assert!(fits(&script(&listed(57), &[])).is_err());
        let long = format!("coeus.example.com/{}*", "x".repeat(200));
        assert!(fits(&script(&[], &[&long])).is_err());
    }

    #[test]
    fn a_script_without_norns_tags_is_unmarked() {
        let read = read(&script(&[], &[]).name, &["team-web".to_owned()]);
        assert_eq!(read.ownership, Ownership::Unmarked { annotated: false });
        assert_eq!(read.value, None);
    }

    #[test]
    fn a_mark_norn_cannot_parse_is_unreadable() {
        let whole = write(Revision(12), false, &script(&["a.example.com/*"], &[]), &[]);
        let cases = [
            whole[..2].to_vec(),
            [whole.clone(), vec!["norn v2 other".to_owned()]].concat(),
            [whole.clone(), vec![whole[0].clone()]].concat(),
            [vec!["norn v1 fleet t=012".to_owned()], whole[1..].to_vec()].concat(),
            [
                vec!["norn v1 fleet t=12 remove".to_owned()],
                whole[1..].to_vec(),
            ]
            .concat(),
            [
                whole.clone(),
                vec!["norn v1 routes a.example.com/*  b.example.com/*".to_owned()],
            ]
            .concat(),
        ];
        for tags in cases {
            let read = read(&script(&[], &[]).name, &tags);
            assert!(
                matches!(read.ownership, Ownership::Unreadable(_)),
                "{tags:?}"
            );
            assert_eq!(read.value, None);
        }
    }
}
