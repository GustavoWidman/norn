use std::{fmt::Write, time::Duration};

use crate::{
    desired::Desired,
    run::{Line, What},
};

const VALUE: usize = 72;

pub(crate) fn render(
    desired: &Desired,
    lines: &[Line],
    elapsed: Duration,
    verbose: bool,
) -> String {
    let read = lines
        .iter()
        .filter(|line| match &line.what {
            What::Planned(verb) => *verb != "create",
            What::Kept | What::Untouched { .. } | What::Foreign(_) | What::Withheld => true,
            _ => false,
        })
        .count();
    let scopes: Vec<(&str, &str)> = {
        let mut scopes: Vec<_> = lines
            .iter()
            .map(|line| (line.kind, line.scope.as_str()))
            .collect();
        scopes.dedup();
        scopes
    };

    let mut text = String::new();
    let _ = writeln!(
        text,
        "norn report · {} · configured mode {} · {} scopes, {read} objects read in {:.1?}",
        desired.host,
        desired.mode.as_str(),
        scopes.len(),
        elapsed,
    );

    let shown = |line: &&Line| match &line.what {
        What::Kept => verbose,
        What::Foreign(whose) => verbose || whose.starts_with("unreadable"),
        _ => true,
    };
    let owner_width = lines
        .iter()
        .filter_map(|line| line.owner.as_ref())
        .map(|o| o.to_string().len())
        .max()
        .unwrap_or(0);
    let key_width = lines
        .iter()
        .filter(shown)
        .map(|line| line.key.len())
        .max()
        .unwrap_or(0);

    for (kind, scope) in &scopes {
        let _ = writeln!(text, "\n{kind} · {scope}");
        let mut block: Vec<&Line> = lines
            .iter()
            .filter(|line| line.kind == *kind && line.scope == *scope)
            .filter(shown)
            .collect();
        block.sort_by_key(|line| rank(&line.what));
        for line in block {
            let (verb, note) = match &line.what {
                What::Planned(verb) => (*verb, None),
                What::Applied(verb) => (*verb, Some("applied".to_owned())),
                What::Failed(verb, why) => (*verb, Some(format!("failed: {why}"))),
                What::Withheld => ("withheld", Some("past this run's delete cap".to_owned())),
                What::Blocked(why) => ("blocked", Some(why.clone())),
                What::Unread(why) => {
                    let _ = writeln!(text, "  unread: {why}");
                    continue;
                }
                What::Kept => ("kept", None),
                What::Untouched { annotated } => {
                    ("untouched", annotated.then(|| "# has a comment".to_owned()))
                }
                What::Foreign(whose) => ("other", Some(format!("# {whose}"))),
            };
            let owner = line
                .owner
                .as_ref()
                .map(ToString::to_string)
                .unwrap_or_default();
            let row = format!(
                "  {verb:<10} {owner:<owner_width$}  {:<key_width$}  {}",
                line.key,
                cut(&line.value)
            );
            let _ = match note {
                Some(note) if note.starts_with('#') => writeln!(text, "{}  {note}", row.trim_end()),
                Some(note) => writeln!(
                    text,
                    "{}\n  {:<10} {:<owner_width$}  {note}",
                    row.trim_end(),
                    "",
                    ""
                ),
                None => writeln!(text, "{}", row.trim_end()),
            };
        }
    }

    let count = |test: &dyn Fn(&What) -> bool| lines.iter().filter(|line| test(&line.what)).count();
    let planned = |verb: &str| count(&|what| matches!(what, What::Planned(v) if *v == verb));
    let _ = writeln!(
        text,
        "\n{} to adopt · {} to update · {} to delete · {} to create · {} withheld · {} blocked · {} untouched · {} others' · {} kept",
        planned("adopt"),
        planned("update"),
        planned("delete"),
        planned("create"),
        count(&|what| *what == What::Withheld),
        count(&|what| matches!(what, What::Blocked(_))),
        count(&|what| matches!(what, What::Untouched { .. })),
        count(&|what| matches!(what, What::Foreign(_))),
        count(&|what| *what == What::Kept),
    );
    let _ = writeln!(
        text,
        "nothing was written: report only reads. adopt writes the mark into a record's comment and changes nothing else"
    );
    text
}

fn rank(what: &What) -> u8 {
    match what {
        What::Unread(_) => 0,
        What::Planned(_) | What::Applied(_) | What::Failed(..) => 1,
        What::Withheld => 2,
        What::Blocked(_) => 3,
        What::Kept => 4,
        What::Untouched { .. } => 5,
        What::Foreign(_) => 6,
    }
}

fn cut(value: &str) -> String {
    if value.chars().count() <= VALUE {
        return value.to_owned();
    }
    let mut cut: String = value.chars().take(VALUE - 1).collect();
    cut.push('…');
    cut
}
