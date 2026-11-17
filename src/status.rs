use std::{fmt, path::Path, time::Duration};

use chrono::{DateTime, Utc};
use eyre::WrapErr;
use serde::{Deserialize, Serialize};

use crate::{
    desired::Mode,
    run::{Line, What},
};

const LINES: usize = 20;

#[derive(Serialize, Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Status {
    pub finished: DateTime<Utc>,
    pub desired: Option<String>,
    pub mode: Option<String>,
    pub health: Health,
    pub counts: Counts,
    pub troubled_since: Option<DateTime<Utc>>,
    pub lines: Vec<String>,
}

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
#[serde(rename_all = "lowercase")]
pub enum Health {
    Failing,
    Blocked,
    Skipped,
    Held,
    Converged,
}

impl Health {
    pub fn as_str(self) -> &'static str {
        match self {
            Health::Failing => "failing",
            Health::Blocked => "blocked",
            Health::Skipped => "skipped",
            Health::Held => "held",
            Health::Converged => "converged",
        }
    }

    fn troubled(self) -> bool {
        matches!(self, Health::Failing | Health::Blocked | Health::Skipped)
    }
}

#[derive(Serialize, Deserialize, Default, Clone, Copy, Debug)]
pub(crate) struct Counts {
    pub kept: usize,
    pub applied: usize,
    pub held: usize,
    pub blocked: usize,
    pub withheld: usize,
    pub failed: usize,
    pub unread: usize,
    pub untouched: usize,
    pub foreign: usize,
}

impl fmt::Display for Counts {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} kept, {} applied, {} held, {} blocked, {} withheld, {} failed, {} unread, {} untouched, {} others'",
            self.kept,
            self.applied,
            self.held,
            self.blocked,
            self.withheld,
            self.failed,
            self.unread,
            self.untouched,
            self.foreign
        )
    }
}

pub(crate) struct Summary {
    pub health: Health,
    pub counts: Counts,
    lines: Vec<String>,
    desired: Option<String>,
    mode: Option<&'static str>,
}

impl Summary {
    pub fn problem(health: Health, message: String) -> Self {
        Self {
            health,
            counts: Counts::default(),
            lines: vec![message],
            desired: None,
            mode: None,
        }
    }

    pub fn of(lines: &[Line]) -> Self {
        let mut counts = Counts::default();
        let mut shown = Vec::new();
        for line in lines {
            let subject = line.subject();
            match &line.what {
                What::Kept => counts.kept += 1,
                What::Untouched { .. } => counts.untouched += 1,
                What::Foreign(_) => counts.foreign += 1,
                What::Applied(_) => counts.applied += 1,
                What::Planned(verb) => {
                    counts.held += 1;
                    shown.push(format!("held: {verb} {subject}"));
                }
                What::Failed(verb, why) => {
                    counts.failed += 1;
                    shown.push(format!("failed: {verb} {subject}: {why}"));
                }
                What::Withheld => {
                    counts.withheld += 1;
                    shown.push(format!("withheld: delete {subject}"));
                }
                What::Blocked(why) => {
                    counts.blocked += 1;
                    shown.push(format!("blocked: {subject}: {why}"));
                }
                What::Unread(why) => {
                    counts.unread += 1;
                    shown.push(format!("unread: {} {}: {why}", line.kind, line.scope));
                }
            }
        }
        let health = if counts.failed + counts.unread > 0 {
            Health::Failing
        } else if counts.blocked + counts.withheld > 0 {
            Health::Blocked
        } else if counts.held > 0 {
            Health::Held
        } else {
            Health::Converged
        };
        shown.sort_by_key(|line| !line.starts_with("failed") && !line.starts_with("unread"));
        shown.truncate(LINES);
        Self {
            health,
            counts,
            lines: shown,
            desired: None,
            mode: None,
        }
    }

    pub fn desired(mut self, path: &Path, mode: Mode) -> Self {
        self.desired = Some(path.display().to_string());
        self.mode = Some(mode.as_str());
        self
    }
}

pub(crate) fn record(state_dir: &Path, summary: &Summary) {
    let previous = read(state_dir).ok();
    let now = Utc::now();
    let troubled_since = summary.health.troubled().then(|| {
        previous
            .as_ref()
            .filter(|previous| previous.health.troubled())
            .and_then(|previous| previous.troubled_since)
            .unwrap_or(now)
    });
    let status = Status {
        finished: now,
        desired: summary.desired.clone(),
        mode: summary.mode.map(str::to_owned),
        health: summary.health,
        counts: summary.counts,
        troubled_since,
        lines: summary.lines.clone(),
    };
    if let Err(error) = write(state_dir, &status) {
        log::error!(
            "cannot record the run's status in {}: {error:#}",
            state_dir.display()
        );
    }
}

fn write(state_dir: &Path, status: &Status) -> eyre::Result<()> {
    std::fs::create_dir_all(state_dir)?;
    let partial = state_dir.join("status.json.partial");
    std::fs::write(&partial, serde_json::to_vec_pretty(status)?)?;
    std::fs::rename(&partial, state_dir.join("status.json"))?;
    Ok(())
}

pub(crate) fn read(state_dir: &Path) -> eyre::Result<Status> {
    let path = state_dir.join("status.json");
    let bytes = std::fs::read(&path).wrap_err_with(|| format!("cannot read {}", path.display()))?;
    serde_json::from_slice(&bytes).wrap_err_with(|| format!("cannot parse {}", path.display()))
}

impl Status {
    pub fn fresh(&self, now: DateTime<Utc>, stale: Duration) -> bool {
        let older = |at: DateTime<Utc>| (now - at).to_std().is_ok_and(|age| age > stale);
        !older(self.finished) && !self.troubled_since.is_some_and(older)
    }

    pub fn describe(&self) -> String {
        let mut text = format!(
            "{} at {} ({})\n",
            self.health.as_str(),
            self.finished.to_rfc3339(),
            self.mode.as_deref().unwrap_or("no mode read")
        );
        if let Some(since) = self.troubled_since {
            text.push_str(&format!("troubled since {}\n", since.to_rfc3339()));
        }
        text.push_str(&format!("{}\n", self.counts));
        for line in &self.lines {
            text.push_str(&format!("{line}\n"));
        }
        text
    }
}
