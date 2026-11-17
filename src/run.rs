use std::{
    fs::File,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    thread,
    time::{Duration, Instant},
};

use eyre::{WrapErr, eyre};
use rustix::fs::{FlockOperation, flock};

use crate::{
    cloudflare::{Api, ApiError, Capability, Clock, Read, Token, TokenError, Write},
    desired::{Desired, Mode, Problems, Wants},
    dns::Dns,
    kind::{Kind, Ladder, Live, Ownership, Plan, Step, Want, Withhold},
    model::HostName,
    plan::plan,
};

#[derive(Clone, Copy, Debug)]
pub struct Timing {
    /// the whole run's budget against cloudflare
    pub deadline: Duration,
    pub per_request: Duration,
    /// how long a run waits for another run of this machine to finish;
    /// past it, the run leaves the work to the next one
    pub lock_wait: Duration,
}

impl Timing {
    fn watchdog(&self) -> Duration {
        self.lock_wait + self.deadline + Duration::from_secs(5)
    }

    fn clock(&self) -> Clock {
        Clock::start(self.deadline, self.per_request)
    }
}

#[derive(Clone)]
pub(crate) enum Cloudflare {
    Https,
    #[cfg(test)]
    Fake(std::sync::Arc<crate::test::fake::Cloudflare>),
}

impl Cloudflare {
    fn reader(&self, token: Token, clock: Clock) -> Api<Read> {
        match self {
            Cloudflare::Https => Api::read_only(token, clock),
            #[cfg(test)]
            Cloudflare::Fake(fake) => Api::over_fetch(fake.clone(), token, clock),
        }
    }

    fn writer(&self, token: Token, clock: Clock) -> Api<Write> {
        match self {
            Cloudflare::Https => Api::read_write(token, clock),
            #[cfg(test)]
            Cloudflare::Fake(fake) => Api::over(fake.clone(), token, clock),
        }
    }
}

/// the service's entry. brings this machine's objects in line in the
/// configured mode and records the outcome in `state_dir/status.json`.
///
/// it returns within the run's watchdog, and the process exits 0 whatever
/// happened: an unreadable desired file, a missing token, a cloudflare
/// outage, a timeout, even a panic (the hook logs it and exits 0). an
/// activation never fails because of cloudflare; the status file says what
/// did not apply.
///
/// the one error is a caller that does not own `state_dir`, which is
/// refused before the run starts.
pub fn converge(
    desired: &Path,
    token_file: Option<&Path>,
    state_dir: &Path,
    timing: Timing,
    max_deletes: usize,
) -> eyre::Result<()> {
    owner_only(state_dir, rustix::process::geteuid().as_raw())?;
    let crash_dir = state_dir.to_owned();
    std::panic::set_hook(Box::new(move |panic| {
        log::error!("norn crashed: {panic}");
        crate::status::record(
            &crash_dir,
            &crate::status::Summary::problem(
                crate::status::Health::Failing,
                format!("crashed: {panic}"),
            ),
        );
        std::process::exit(0);
    }));
    let watchdog_dir = state_dir.to_owned();
    let after = timing.watchdog();
    thread::spawn(move || {
        thread::sleep(after);
        let message =
            format!("gave up after {after:?}; what did not apply is retried by the next run");
        log::error!("{message}");
        crate::status::record(
            &watchdog_dir,
            &crate::status::Summary::problem(crate::status::Health::Failing, message),
        );
        std::process::exit(0);
    });
    converge_with(
        &Cloudflare::Https,
        desired,
        token_file,
        state_dir,
        timing,
        max_deletes,
    );
    Ok(())
}

pub(crate) fn owner_only(state_dir: &Path, caller: u32) -> eyre::Result<()> {
    let owner = match std::fs::metadata(state_dir) {
        Ok(metadata) => metadata.uid(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(error).wrap_err_with(|| {
                format!(
                    "cannot inspect {}; run `systemctl start norn` instead",
                    state_dir.display()
                )
            });
        }
    };
    if owner == caller {
        return Ok(());
    }
    Err(eyre!(
        "{} belongs to uid {owner}, and a run as uid {caller} would leave files there the service cannot open; run `systemctl start norn` instead",
        state_dir.display()
    ))
}

pub(crate) fn converge_with(
    cloudflare: &Cloudflare,
    desired_path: &Path,
    token_file: Option<&Path>,
    state_dir: &Path,
    timing: Timing,
    max_deletes: usize,
) -> crate::status::Health {
    let summary = 'run: {
        let desired = match Desired::read(desired_path) {
            Ok(desired) => desired,
            Err(problems) => {
                log::error!("{problems}");
                break 'run crate::status::Summary::problem(
                    crate::status::Health::Failing,
                    problems.to_string(),
                );
            }
        };
        let _lock = match Lock::acquire(state_dir, timing.lock_wait) {
            Ok(Some(lock)) => lock,
            Ok(None) => {
                log::warn!(
                    "another norn run held the lock for {:?}; the next run does this one's work",
                    timing.lock_wait
                );
                return crate::status::Health::Skipped;
            }
            Err(error) => {
                log::error!("{error:#}");
                break 'run crate::status::Summary::problem(
                    crate::status::Health::Failing,
                    format!("{error:#}"),
                );
            }
        };
        let token = match token_file.map(Token::read) {
            Some(Ok(token)) => token,
            None | Some(Err(TokenError::Missing | TokenError::Empty)) => {
                let message = "no cloudflare token yet; skipping this run".to_owned();
                log::warn!("{message}");
                break 'run crate::status::Summary::problem(
                    crate::status::Health::Skipped,
                    message,
                )
                .desired(desired_path, desired.mode);
            }
            Some(Err(error)) => {
                log::error!("{error}");
                break 'run crate::status::Summary::problem(
                    crate::status::Health::Failing,
                    error.to_string(),
                )
                .desired(desired_path, desired.mode);
            }
        };

        let started = Instant::now();
        let lines = match desired.mode {
            Mode::Report => {
                let api = cloudflare.reader(token, timing.clock());
                let mut show = Show::default();
                visit(&api, &desired, max_deletes, &mut show);
                show.lines
            }
            Mode::Write(ladder) => {
                let api = cloudflare.writer(token, timing.clock());
                let mut apply = Apply {
                    api: &api,
                    ladder,
                    lines: Vec::new(),
                };
                visit(&api, &desired, max_deletes, &mut apply);
                apply.lines
            }
        };
        for line in &lines {
            line.log();
        }
        let summary = crate::status::Summary::of(&lines).desired(desired_path, desired.mode);
        log::info!(
            "{} in {:.1?} ({})",
            summary.health.as_str(),
            started.elapsed(),
            summary.counts
        );
        summary
    };
    let health = summary.health;
    crate::status::record(state_dir, &summary);
    health
}

/// a person's view of what a run would do. it builds only an `Api<Read>`
/// and hands every plan to [Show], which holds no client at all. errors are
/// returned, since a person reads the exit code.
pub fn report(
    desired: &Path,
    token_file: Option<&Path>,
    timing: Timing,
    max_deletes: usize,
    verbose: bool,
) -> eyre::Result<()> {
    let (text, complete) = report_with(
        &Cloudflare::Https,
        desired,
        token_file,
        timing,
        max_deletes,
        verbose,
    )?;
    print!("{text}");
    if complete {
        Ok(())
    } else {
        Err(eyre!(
            "some scopes could not be read; the report is incomplete"
        ))
    }
}

pub(crate) fn report_with(
    cloudflare: &Cloudflare,
    desired_path: &Path,
    token_file: Option<&Path>,
    timing: Timing,
    max_deletes: usize,
    verbose: bool,
) -> eyre::Result<(String, bool)> {
    let desired = Desired::read(desired_path).map_err(|problems| eyre!("{problems}"))?;
    let token_file = token_file
        .ok_or_else(|| eyre!("no token file: pass --token-file or set NORN_TOKEN_FILE"))?;
    let token =
        Token::read(token_file).map_err(|error| eyre!("{}: {error}", token_file.display()))?;
    let started = Instant::now();
    let api = cloudflare.reader(token, timing.clock());
    let mut show = Show::default();
    visit(&api, &desired, max_deletes, &mut show);
    let complete = !show
        .lines
        .iter()
        .any(|line| matches!(line.what, What::Unread(_)));
    Ok((
        crate::report::render(&desired, &show.lines, started.elapsed(), verbose),
        complete,
    ))
}

/// judges a rendered desired file and writes its canonical form
pub fn check(input: &Path, output: Option<&Path>) -> Result<(), Problems> {
    let desired = Desired::read(input)?;
    if let Some(output) = output {
        std::fs::write(output, desired.canonical()).map_err(|error| {
            Problems(vec![crate::desired::Problem {
                subject: output.display().to_string(),
                message: error.to_string(),
            }])
        })?;
    }
    Ok(())
}

/// prints the last run's outcome. false when the machine has been troubled
/// for longer than `stale`, or has not run for that long.
pub fn status(state_dir: &Path, stale: Duration) -> eyre::Result<bool> {
    let status = crate::status::read(state_dir)
        .wrap_err_with(|| format!("no status in {}", state_dir.display()))?;
    print!("{}", status.describe());
    Ok(status.fresh(chrono::Utc::now(), stale))
}

#[derive(Clone, Debug)]
pub(crate) struct Line {
    pub kind: &'static str,
    pub scope: String,
    pub what: What,
    pub owner: Option<HostName>,
    pub key: String,
    pub value: String,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum What {
    Planned(&'static str),
    Applied(&'static str),
    Failed(&'static str, String),
    Withheld(String),
    Blocked(String),
    Unread(String),
    Kept,
    Untouched { annotated: bool },
    Foreign(String),
}

impl Line {
    fn of<K: Kind>(
        scope: &K::Scope,
        what: What,
        owner: Option<&HostName>,
        key: &K::Key,
        value: String,
    ) -> Self {
        Self {
            kind: K::NAME,
            scope: scope.to_string(),
            what,
            owner: owner.cloned(),
            key: key.to_string(),
            value,
        }
    }

    fn step<K: Kind>(scope: &K::Scope, step: &Step<K>, what: What) -> Self {
        Self::of::<K>(
            scope,
            what,
            Some(step.owner()),
            &step.key(),
            step.describe(),
        )
    }

    fn live<K: Kind>(scope: &K::Scope, live: &Live<K>, what: What) -> Self {
        let value = live
            .value
            .as_ref()
            .map(ToString::to_string)
            .unwrap_or_else(|| "(not modeled)".into());
        Self::of::<K>(scope, what, None, &live.key, value)
    }

    pub fn subject(&self) -> String {
        let owner = self
            .owner
            .as_ref()
            .map(|owner| format!("{owner} "))
            .unwrap_or_default();
        format!(
            "{} {}: {owner}{} {}",
            self.kind, self.scope, self.key, self.value
        )
    }

    fn log(&self) {
        let subject = self.subject();
        match &self.what {
            What::Applied(verb) => log::info!("{verb} {subject}"),
            What::Planned(verb) => log::info!("held ({verb}) {subject}"),
            What::Failed(verb, why) => log::error!("failed to {verb} {subject}: {why}"),
            What::Withheld(why) => log::warn!("withheld delete {subject}: {why}"),
            What::Blocked(why) => log::warn!("blocked {subject}: {why}"),
            What::Unread(why) => log::error!("{} {}: unread: {why}", self.kind, self.scope),
            What::Kept | What::Untouched { .. } | What::Foreign(_) => {}
        }
    }

    fn plan<K: Kind>(
        scope: &K::Scope,
        plan: &Plan<K>,
        steps: &mut dyn FnMut(&Step<K>) -> What,
    ) -> Vec<Line> {
        let mut lines: Vec<Line> = plan
            .steps
            .iter()
            .map(|step| Line::step(scope, step, steps(step)))
            .collect();
        lines.extend(plan.withheld.iter().map(|withheld| {
            Line::step(
                scope,
                &withheld.step,
                What::Withheld(withheld.why.explain()),
            )
        }));
        lines.extend(plan.blocked.iter().map(|blocked| {
            Line::of::<K>(
                scope,
                What::Blocked(format!(
                    "{}; there now: {} {}",
                    blocked.why.explain(),
                    blocked.by.key,
                    shown(&blocked.by)
                )),
                Some(&blocked.owner),
                &K::key(&blocked.want),
                blocked.want.to_string(),
            )
        }));
        lines.extend(plan.kept.iter().map(|live| {
            let mut line = Line::live(scope, live, What::Kept);
            if let Ownership::Marked(owner) = &live.ownership {
                line.owner = Some(owner.clone());
            }
            line
        }));
        lines.extend(plan.untouched.iter().map(|live| {
            let annotated = live.ownership == (Ownership::Unmarked { annotated: true });
            Line::live(scope, live, What::Untouched { annotated })
        }));
        lines.extend(plan.foreign.iter().map(|live| {
            let whose = match (&live.ownership, &live.value) {
                (Ownership::Unreadable(mark), _) => format!("unreadable mark {mark:?}"),
                (_, None) => "a shape norn does not model".to_owned(),
                (Ownership::Marked(owner), _) => format!("{owner}'s"),
                (Ownership::Unmarked { .. }, _) => "unmarked".to_owned(),
            };
            Line::live(scope, live, What::Foreign(whose))
        }));
        lines
    }
}

fn shown<K: Kind>(live: &Live<K>) -> String {
    live.value
        .as_ref()
        .map(ToString::to_string)
        .unwrap_or_else(|| "(a shape norn does not model)".into())
}

trait Act {
    fn on<K: Kind>(&mut self, planned: Planned<K>);
}

struct Planned<K: Kind> {
    scopes: Vec<Scoped<K>>,
}

enum Scoped<K: Kind> {
    Read {
        scope: K::Scope,
        place: K::Place,
        plan: Plan<K>,
    },
    Unread {
        scope: K::Scope,
        error: ApiError,
    },
}

#[derive(Default)]
struct Show {
    lines: Vec<Line>,
}

impl Act for Show {
    fn on<K: Kind>(&mut self, planned: Planned<K>) {
        for scoped in planned.scopes {
            match scoped {
                Scoped::Read { scope, plan, .. } => {
                    self.lines.extend(Line::plan(&scope, &plan, &mut |step| {
                        What::Planned(step.verb())
                    }));
                }
                Scoped::Unread { scope, error } => self.lines.push(unread::<K>(&scope, &error)),
            }
        }
    }
}

struct Apply<'a> {
    api: &'a Api<Write>,
    ladder: Ladder,
    lines: Vec<Line>,
}

impl Act for Apply<'_> {
    fn on<K: Kind>(&mut self, planned: Planned<K>) {
        for scoped in planned.scopes {
            match scoped {
                Scoped::Read { scope, place, plan } => {
                    let (api, ladder) = (self.api, self.ladder);
                    self.lines.extend(Line::plan(&scope, &plan, &mut |step| {
                        if !ladder.permits(step.rung()) {
                            return What::Planned(step.verb());
                        }
                        match K::write(api, &place, step) {
                            Ok(()) => What::Applied(step.verb()),
                            Err(error) => What::Failed(step.verb(), error.to_string()),
                        }
                    }));
                }
                Scoped::Unread { scope, error } => self.lines.push(unread::<K>(&scope, &error)),
            }
        }
    }
}

fn unread<K: Kind>(scope: &K::Scope, error: &ApiError) -> Line {
    Line {
        kind: K::NAME,
        scope: scope.to_string(),
        what: What::Unread(error.to_string()),
        owner: None,
        key: String::new(),
        value: String::new(),
    }
}

fn visit<C: Capability>(api: &Api<C>, desired: &Desired, max_deletes: usize, act: &mut impl Act) {
    let Wants { dns } = desired.wants();
    let zones = desired.zones.iter().cloned().collect();
    act.on(plan_kind::<Dns, C>(
        api,
        &desired.host,
        zones,
        dns,
        max_deletes,
    ));
}

fn plan_kind<K: Kind, C: Capability>(
    api: &Api<C>,
    owner: &HostName,
    scopes: Vec<K::Scope>,
    wants: Vec<Want<K>>,
    max_deletes: usize,
) -> Planned<K> {
    let read: Vec<_> = scopes
        .into_iter()
        .map(|scope| {
            let read = K::read(api, &scope);
            (scope, read)
        })
        .collect();
    let plan_all = |breaker: Option<&Withhold>| -> Vec<Scoped<K>> {
        read.iter()
            .map(|(scope, read)| match read {
                Ok((place, live)) => {
                    let mine = wants
                        .iter()
                        .filter(|want| want.scope == *scope)
                        .cloned()
                        .collect();
                    Scoped::Read {
                        scope: scope.clone(),
                        place: place.clone(),
                        plan: plan(owner, mine, live.clone(), breaker),
                    }
                }
                Err(error) => Scoped::Unread {
                    scope: scope.clone(),
                    error: error.clone(),
                },
            })
            .collect()
    };
    let scopes = plan_all(None);
    let planned = scopes
        .iter()
        .map(|scoped| match scoped {
            Scoped::Read { plan, .. } => plan
                .steps
                .iter()
                .filter(|step| matches!(step, Step::Delete { .. }))
                .count(),
            Scoped::Unread { .. } => 0,
        })
        .sum();
    let scopes = if planned > max_deletes {
        plan_all(Some(&Withhold::Breaker {
            planned,
            max: max_deletes,
        }))
    } else {
        scopes
    };
    Planned { scopes }
}

pub(crate) struct Lock {
    _file: File,
}

impl Lock {
    pub fn acquire(state_dir: &Path, wait: Duration) -> eyre::Result<Option<Self>> {
        std::fs::create_dir_all(state_dir)
            .wrap_err_with(|| format!("cannot create {}", state_dir.display()))?;
        let path: PathBuf = state_dir.join("lock");
        let file = File::options()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .wrap_err_with(|| format!("cannot open {}", path.display()))?;
        let until = Instant::now() + wait;
        loop {
            match flock(&file, FlockOperation::NonBlockingLockExclusive) {
                Ok(()) => return Ok(Some(Self { _file: file })),
                Err(rustix::io::Errno::WOULDBLOCK) if Instant::now() < until => {
                    thread::sleep(Duration::from_millis(100));
                }
                Err(rustix::io::Errno::WOULDBLOCK) => return Ok(None),
                Err(error) => return Err(eyre!("cannot lock {}: {error}", path.display())),
            }
        }
    }
}
