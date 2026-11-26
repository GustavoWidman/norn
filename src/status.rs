use std::{
    fmt,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    time::Duration,
};

use chrono::{DateTime, Utc};
use eyre::WrapErr;
use serde::{Deserialize, Serialize};

use crate::{
    desired::Mode,
    run::{Line, RACED, What},
};

const LINES: usize = 20;

const HEALTHY: &str = "norn_healthy_timestamp_seconds";

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
                What::Held(verb, why) => {
                    counts.held += 1;
                    shown.push(format!("held: {verb} {subject}: {why}"));
                }
                What::Raced(verb) => {
                    counts.held += 1;
                    shown.push(format!("raced: {verb} {subject}: {RACED}"));
                }
                What::Failed(verb, why) => {
                    counts.failed += 1;
                    shown.push(format!("failed: {verb} {subject}: {why}"));
                }
                What::Withheld(why) => {
                    counts.withheld += 1;
                    shown.push(format!("withheld: delete {subject}: {why}"));
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

/// where a run's outcome goes: `status.json` in the state directory and,
/// with `metrics`, a prometheus textfile of when this machine was last
/// known healthy
#[derive(Clone)]
pub(crate) struct Store {
    pub state_dir: PathBuf,
    pub metrics: Option<PathBuf>,
}

impl Store {
    pub fn record(&self, summary: &Summary) {
        let previous = read(&self.state_dir).ok();
        let status = Status::following(previous.as_ref(), summary, Utc::now());
        let written = serde_json::to_vec_pretty(&status)
            .map_err(eyre::Report::from)
            .and_then(|json| replace(&self.state_dir.join("status.json"), &json));
        if let Err(error) = written {
            log::error!(
                "cannot record the run's status in {}: {error:#}",
                self.state_dir.display()
            );
        }
        if let Some(path) = &self.metrics
            && let Err(error) = replace(path, status.metrics().as_bytes())
        {
            log::error!("cannot write {}: {error:#}", path.display());
        }
    }
}

/// writes `path` whole or not at all, readable by everyone: a reader sees
/// the old file or the new one
fn replace(path: &Path, bytes: &[u8]) -> eyre::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut partial = path.as_os_str().to_owned();
    partial.push(".partial");
    let partial = PathBuf::from(partial);
    // one left by a run under another uid cannot be opened, only removed
    match std::fs::remove_file(&partial) {
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => return Err(error.into()),
        _ => {}
    }
    std::fs::write(&partial, bytes)?;
    std::fs::set_permissions(&partial, std::fs::Permissions::from_mode(0o644))?;
    std::fs::rename(&partial, path)?;
    Ok(())
}

pub(crate) fn read(state_dir: &Path) -> eyre::Result<Status> {
    let path = state_dir.join("status.json");
    let bytes = std::fs::read(&path).wrap_err_with(|| format!("cannot read {}", path.display()))?;
    serde_json::from_slice(&bytes).wrap_err_with(|| format!("cannot parse {}", path.display()))
}

impl Status {
    /// the status of a run that ended `now`. a troubled run keeps the
    /// streak's start from a troubled run before it
    pub fn following(previous: Option<&Status>, summary: &Summary, now: DateTime<Utc>) -> Self {
        let troubled_since = summary.health.troubled().then(|| {
            previous
                .filter(|previous| previous.health.troubled())
                .and_then(|previous| previous.troubled_since)
                .unwrap_or(now)
        });
        Status {
            finished: now,
            desired: summary.desired.clone(),
            mode: summary.mode.map(str::to_owned),
            health: summary.health,
            counts: summary.counts,
            troubled_since,
            lines: summary.lines.clone(),
        }
    }

    /// the last moment this machine's norn was known healthy: the start of
    /// its troubled streak, or else the end of its last run
    pub fn healthy_at(&self) -> DateTime<Utc> {
        self.troubled_since.unwrap_or(self.finished)
    }

    /// whether the machine was known healthy within `stale` of `now`
    pub fn fresh(&self, now: DateTime<Utc>, stale: Duration) -> bool {
        !(now - self.healthy_at())
            .to_std()
            .is_ok_and(|age| age > stale)
    }

    /// the prometheus textfile node exporter's textfile collector serves
    pub fn metrics(&self) -> String {
        format!(
            "# HELP {HEALTHY} When norn was last known healthy on this machine: the start of its troubled streak, or else the end of its last run.\n\
             # TYPE {HEALTHY} gauge\n\
             {HEALTHY} {}\n",
            self.healthy_at().timestamp()
        )
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
