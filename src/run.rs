use std::{
    collections::{BTreeMap, BTreeSet},
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
    gate::{self, Deletes, Permit, Rollout, RolloutFile},
    kind::{Kind, Ladder, Live, Mark, Ownership, Plan, Reading, Sent, Step, Want, Withhold},
    model::{Owner, Revision},
    plan::plan,
    workers::{self, Owners, Pattern, Routes, ScriptName, Scripts},
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
    rollout: Option<&RolloutFile>,
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
        rollout,
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
    rollout: Option<&RolloutFile>,
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
        let rollout = Rollout::read(rollout, chrono::Utc::now());
        let (lines, gate) = match desired.mode {
            Mode::Report => {
                let api = cloudflare.reader(token, timing.clock());
                let mut show = Show::default();
                let gate = visit(&api, &desired, &rollout, max_deletes, &mut show);
                (show.lines, gate)
            }
            Mode::Write(ladder) => {
                let api = cloudflare.writer(token, timing.clock());
                let mut apply = Apply {
                    api: &api,
                    ladder,
                    lines: Vec::new(),
                };
                let gate = visit(&api, &desired, &rollout, max_deletes, &mut apply);
                (apply.lines, gate)
            }
        };
        if let Some(gate) = &gate {
            log::info!("fleet scope: {gate}");
        }
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
    rollout: Option<&RolloutFile>,
    verbose: bool,
) -> eyre::Result<()> {
    let (text, complete) = report_with(
        &Cloudflare::Https,
        desired,
        token_file,
        timing,
        max_deletes,
        rollout,
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
    rollout: Option<&RolloutFile>,
    verbose: bool,
) -> eyre::Result<(String, bool)> {
    let desired = Desired::read(desired_path).map_err(|problems| eyre!("{problems}"))?;
    let token_file = token_file
        .ok_or_else(|| eyre!("no token file: pass --token-file or set NORN_TOKEN_FILE"))?;
    let token =
        Token::read(token_file).map_err(|error| eyre!("{}: {error}", token_file.display()))?;
    let started = Instant::now();
    let rollout = Rollout::read(rollout, chrono::Utc::now());
    let api = cloudflare.reader(token, timing.clock());
    let mut show = Show::default();
    let gate = visit(&api, &desired, &rollout, max_deletes, &mut show);
    let complete = !show
        .lines
        .iter()
        .any(|line| matches!(line.what, What::Unread(_)));
    Ok((
        crate::report::render(
            &desired,
            gate.as_ref(),
            &show.lines,
            started.elapsed(),
            verbose,
        ),
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
    pub owner: Option<String>,
    pub key: String,
    pub value: String,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum What {
    Planned(&'static str),
    /// withheld in the normal course of a fleet rollout
    Held(&'static str, String),
    Applied(&'static str),
    /// another machine wrote the same shared object first
    Raced(&'static str),
    Failed(&'static str, String),
    Withheld(String),
    Blocked(String),
    Unread(String),
    Kept,
    Untouched {
        annotated: bool,
    },
    Foreign(String),
}

pub(crate) const RACED: &str = "another machine wrote it first; the next run reads what is there";

impl Line {
    fn of<K: Kind>(
        scope: &K::Scope,
        what: What,
        owner: Option<String>,
        key: &K::Key,
        value: String,
    ) -> Self {
        Self {
            kind: K::NAME,
            scope: scope.to_string(),
            what,
            owner,
            key: key.to_string(),
            value,
        }
    }

    fn step<K: Kind>(scope: &K::Scope, step: &Step<K>, what: What) -> Self {
        Self::of::<K>(
            scope,
            what,
            Some(step.owner().to_string()),
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
            What::Held(verb, why) => log::info!("held ({verb}) {subject}: {why}"),
            What::Raced(verb) => log::info!("raced ({verb}) {subject}: {RACED}"),
            What::Failed(verb, why) => log::error!("failed to {verb} {subject}: {why}"),
            What::Withheld(why) => log::warn!("withheld delete {subject}: {why}"),
            What::Blocked(why) => log::warn!("blocked {subject}: {why}"),
            What::Unread(why) => log::error!("{} {}: unread: {why}", self.kind, self.scope),
            What::Kept | What::Untouched { .. } | What::Foreign(_) => {}
        }
    }

    /// one scope's lines, from the plans of every owner over it. an object
    /// no plan claims is listed once, as the first plan sees it.
    fn plans<K: Kind>(
        scope: &K::Scope,
        plans: &[Plan<K>],
        steps: &mut dyn FnMut(&Plan<K>, &Step<K>) -> What,
    ) -> Vec<Line> {
        let mut lines = Vec::new();
        for plan in plans {
            lines.extend(
                plan.steps
                    .iter()
                    .map(|step| Line::step(scope, step, steps(plan, step))),
            );
            lines.extend(plan.withheld.iter().map(|withheld| {
                let what = if withheld.why.holds() {
                    What::Held(withheld.step.verb(), withheld.why.explain())
                } else {
                    What::Withheld(withheld.why.explain())
                };
                Line::step(scope, &withheld.step, what)
            }));
            lines.extend(plan.blocked.iter().map(|blocked| {
                let why = format!(
                    "{}; there now: {} {}",
                    blocked.why.explain(),
                    blocked.by.key,
                    shown(&blocked.by)
                );
                Line::of::<K>(
                    scope,
                    if blocked.why.holds() {
                        What::Held("create", why)
                    } else {
                        What::Blocked(why)
                    },
                    Some(blocked.owner.to_string()),
                    &K::key(&blocked.want),
                    blocked.want.to_string(),
                )
            }));
            lines.extend(plan.kept.iter().map(|live| {
                let mut line = Line::live(scope, live, What::Kept);
                line.owner = match &live.ownership {
                    Ownership::Host(owner) => Some(owner.to_string()),
                    Ownership::Fleet(stamp) => Some(format!("fleet t={}", stamp.revision)),
                    Ownership::Unmarked { .. } | Ownership::Unreadable(_) => None,
                };
                line
            }));
        }
        let Some((first, others)) = plans.split_first() else {
            return lines;
        };
        let unclaimed = |live: &&Live<K>| !others.iter().any(|plan| plan.claims(&live.handle));
        lines.extend(first.untouched.iter().filter(unclaimed).map(|live| {
            let annotated = live.ownership == (Ownership::Unmarked { annotated: true });
            Line::live(scope, live, What::Untouched { annotated })
        }));
        lines.extend(first.foreign.iter().filter(unclaimed).map(|live| {
            let whose = match (&live.ownership, &live.value) {
                (Ownership::Unreadable(mark), _) => format!("unreadable mark {mark:?}"),
                (_, None) => "a shape norn does not model".to_owned(),
                (Ownership::Host(owner), _) => format!("{owner}'s"),
                (Ownership::Fleet(_), _) => "the fleet's".to_owned(),
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
    /// acts on one kind's plans, and returns the keys of the steps it sent
    /// that failed
    fn on<K: Kind>(&mut self, planned: Planned<K>) -> BTreeSet<K::Key>;
}

struct Planned<K: Kind> {
    scopes: Vec<Scoped<K>>,
    /// the mark the fleet's plans write, when the gate permits them to
    fleet: Option<Mark>,
}

enum Scoped<K: Kind> {
    Read {
        scope: K::Scope,
        place: K::Place,
        plans: Vec<Plan<K>>,
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
    fn on<K: Kind>(&mut self, planned: Planned<K>) -> BTreeSet<K::Key> {
        for scoped in planned.scopes {
            match scoped {
                Scoped::Read { scope, plans, .. } => {
                    self.lines
                        .extend(Line::plans(&scope, &plans, &mut |_, step| {
                            What::Planned(step.verb())
                        }));
                }
                Scoped::Unread { scope, error } => self.lines.push(unread::<K>(&scope, &error)),
            }
        }
        BTreeSet::new()
    }
}

struct Apply<'a> {
    api: &'a Api<Write>,
    ladder: Ladder,
    lines: Vec<Line>,
}

impl Act for Apply<'_> {
    fn on<K: Kind>(&mut self, planned: Planned<K>) -> BTreeSet<K::Key> {
        let mut failed = BTreeSet::new();
        for scoped in planned.scopes {
            match scoped {
                Scoped::Read {
                    scope,
                    place,
                    plans,
                } => {
                    let (api, ladder) = (self.api, self.ladder);
                    self.lines
                        .extend(Line::plans(&scope, &plans, &mut |plan, step| {
                            let mark = match &plan.owner {
                                Owner::Host(host) => Some(Mark::Host(host.clone())),
                                Owner::Fleet => planned.fleet.clone(),
                            };
                            let Some(mark) = mark.filter(|_| ladder.permits(step.rung())) else {
                                return What::Planned(step.verb());
                            };
                            match K::write(api, &place, step, &mark) {
                                Ok(Sent::Applied) => What::Applied(step.verb()),
                                Ok(Sent::Raced) => What::Raced(step.verb()),
                                Err(error) => {
                                    failed.insert(step.key());
                                    What::Failed(step.verb(), error.to_string())
                                }
                            }
                        }));
                }
                Scoped::Unread { scope, error } => self.lines.push(unread::<K>(&scope, &error)),
            }
        }
        failed
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

/// returns what the fleet gate decided, when shared objects matter to this
/// run. every kind is read before anything is planned, so one gate and one
/// delete cap cover the whole run.
fn visit<C: Capability>(
    api: &Api<C>,
    desired: &Desired,
    rollout: &Rollout,
    max_deletes: usize,
    act: &mut impl Act,
) -> Option<Permit> {
    let Wants {
        dns,
        mut scripts,
        routes,
    } = desired.wants();
    let own = desired.fleet.revision;
    let zones: Vec<_> = desired.zones.iter().cloned().collect();
    let dns_read = Scopes::<Dns>::read(api, zones.clone(), &());
    let (script_read, route_read) = match &desired.account {
        None => (Scopes::default(), Scopes::default()),
        Some(account) => {
            let script_read = Scopes::<Scripts>::read(api, vec![account.clone()], &());
            // whose a route is depends on whose its script is
            let route_read = match script_read.failure() {
                Some(error) => Scopes::failed(zones, error),
                None => {
                    let owners = Owners::new(script_read.live(), &routes, own);
                    Scopes::<Routes>::read(api, zones, &owners)
                }
            };
            workers::listings(&mut scripts, script_read.live(), route_read.live());
            (script_read, route_read)
        }
    };

    let newest = [dns_read.newest(), script_read.newest(), route_read.newest()]
        .into_iter()
        .flatten()
        .max();
    let unread = dns_read.unread() || script_read.unread() || route_read.unread();
    let permit = gate::permit(own, newest, rollout, unread);
    let shared = own.is_some()
        || newest.is_some()
        || dns.iter().any(|want| want.owner == Owner::Fleet)
        || !scripts.is_empty()
        || !routes.is_empty();
    if let (true, Rollout::Unknown(why)) = (shared, rollout) {
        log::warn!("the fleet's rollout is unknown: {why}");
    }
    let fleet = Fleet::new(&permit);
    let host = Owner::Host(desired.host.clone());

    // the cap counts the deletes a run would send, so it counts them once
    // the steps one kind holds for another are held
    let plan_all = |breaker: Option<&Withhold>| {
        let mut script_plans = script_read.plan(&host, &scripts, breaker, &fleet);
        let mut route_plans = route_read.plan(&host, &routes, breaker, &fleet);
        settle(
            &mut script_plans,
            &mut route_plans,
            &script_read,
            &route_read,
        );
        (
            dns_read.plan(&host, &dns, breaker, &fleet),
            script_plans,
            route_plans,
        )
    };
    let (mut dns_plans, mut script_plans, mut route_plans) = plan_all(None);
    let planned = deletes(&dns_plans) + deletes(&script_plans) + deletes(&route_plans);
    if planned > max_deletes {
        let breaker = Withhold::Breaker {
            planned,
            max: max_deletes,
        };
        (dns_plans, script_plans, route_plans) = plan_all(Some(&breaker));
    }

    act.on(Planned {
        scopes: dns_plans,
        fleet: fleet.mark.clone(),
    });
    let listings: Vec<(ScriptName, Pattern)> = steps(&script_plans)
        .filter_map(|step| match step {
            Step::Create { want, .. } | Step::Update { want, .. } => Some(want),
            _ => None,
        })
        .flat_map(|script| {
            let listed = script.routes.iter().chain(&script.bypass);
            listed.map(|pattern| (script.name.clone(), pattern.clone()))
        })
        .collect();
    let failed = act.on(Planned {
        scopes: script_plans,
        fleet: fleet.mark.clone(),
    });
    // a route written while the script that lists it is not would be no
    // one's, so it waits for that script
    let unlisted: BTreeMap<Pattern, ScriptName> = listings
        .into_iter()
        .filter(|(script, _)| failed.contains(script))
        .map(|(script, pattern)| (pattern, script))
        .collect();
    for plan in plans(&mut route_plans) {
        plan.withhold(|step| match step {
            Step::Create { .. } | Step::Update { .. } => unlisted
                .get(&step.key())
                .map(|script| Withhold::Unlisted(script.to_string())),
            _ => None,
        });
    }
    act.on(Planned {
        scopes: route_plans,
        fleet: fleet.mark.clone(),
    });
    shared.then_some(permit)
}

/// orders a worker's script and its routes across the two kinds: a route
/// runs a script only once the script is the fleet's, or is written in this
/// same run before it, and a script is deleted only once the bypasses it
/// lists are gone. deleting a script deletes the routes that run it, so those
/// need not go first.
fn settle(
    scripts: &mut [Scoped<Scripts>],
    routes: &mut [Scoped<Routes>],
    script_read: &Scopes<Scripts>,
    route_read: &Scopes<Routes>,
) {
    let written = steps(scripts)
        .filter(|step| matches!(step, Step::Create { .. } | Step::Update { .. }))
        .map(Step::key);
    let ready: BTreeSet<ScriptName> = script_read
        .live()
        .filter(|live| matches!(live.ownership, Ownership::Fleet(_)))
        .map(|live| live.key.clone())
        .chain(written)
        .collect();
    let bypasses: BTreeSet<&Pattern> = route_read
        .live()
        .filter(|live| {
            live.value
                .as_ref()
                .is_some_and(|route| route.script.is_none())
        })
        .map(|live| &live.key)
        .collect();

    for plan in plans(scripts) {
        plan.withhold(|step| match step {
            Step::Delete { live, .. }
                if live.value.as_ref().is_some_and(|script| {
                    script
                        .bypass
                        .iter()
                        .any(|pattern| bypasses.contains(pattern))
                }) =>
            {
                Some(Withhold::Bypassed)
            }
            _ => None,
        });
    }
    for plan in plans(routes) {
        plan.withhold(|step| match step {
            Step::Create { want, .. } | Step::Update { want, .. } => want
                .script
                .as_ref()
                .filter(|script| !ready.contains(*script))
                .map(|script| Withhold::Unscripted(script.to_string())),
            _ => None,
        });
    }
}

/// what the gate lets the fleet's plans do this run
struct Fleet {
    deletes: Deletes,
    hold: Option<Withhold>,
    /// the mark the fleet's plans write, when the gate permits them to
    mark: Option<Mark>,
}

impl Fleet {
    fn new(permit: &Permit) -> Self {
        match permit {
            Permit::Write { revision, deletes } => Self {
                deletes: *deletes,
                hold: None,
                mark: Some(Mark::Fleet(*revision)),
            },
            Permit::Hold(hold) => Self {
                deletes: Deletes::Never,
                hold: Some(Withhold::Fleet(hold.clone())),
                mark: None,
            },
        }
    }
}

/// one kind's scopes as this run read them
struct Scopes<K: Kind> {
    scopes: Vec<(K::Scope, Reading<K>)>,
}

impl<K: Kind> Default for Scopes<K> {
    fn default() -> Self {
        Self { scopes: Vec::new() }
    }
}

impl<K: Kind> Scopes<K> {
    fn read<C: Capability>(api: &Api<C>, scopes: Vec<K::Scope>, context: &K::Context) -> Self {
        let scopes = scopes
            .into_iter()
            .map(|scope| {
                let read = K::read(api, &scope, context);
                (scope, read)
            })
            .collect();
        Self { scopes }
    }

    /// scopes left unread because what reading them needs could not be read
    fn failed(scopes: Vec<K::Scope>, error: ApiError) -> Self {
        let scopes = scopes
            .into_iter()
            .map(|scope| (scope, Err(error.clone())))
            .collect();
        Self { scopes }
    }

    fn failure(&self) -> Option<ApiError> {
        self.scopes
            .iter()
            .find_map(|(_, read)| read.as_ref().err().cloned())
    }

    fn live(&self) -> impl Iterator<Item = &Live<K>> {
        self.scopes
            .iter()
            .filter_map(|(_, read)| read.as_ref().ok())
            .flat_map(|(_, live)| live)
    }

    fn newest(&self) -> Option<Revision> {
        self.live()
            .filter_map(|live| match live.ownership {
                Ownership::Fleet(stamp) => Some(stamp.revision),
                _ => None,
            })
            .max()
    }

    fn unread(&self) -> bool {
        self.scopes.iter().any(|(_, read)| read.is_err())
    }

    fn plan(
        &self,
        host: &Owner,
        wants: &[Want<K>],
        breaker: Option<&Withhold>,
        fleet: &Fleet,
    ) -> Vec<Scoped<K>> {
        let owners = [host.clone(), Owner::Fleet];
        self.scopes
            .iter()
            .map(|(scope, read)| match read {
                Ok((place, live)) => Scoped::Read {
                    scope: scope.clone(),
                    place: place.clone(),
                    plans: owners
                        .iter()
                        .map(|owner| {
                            let mine = wants
                                .iter()
                                .filter(|want| want.scope == *scope && want.owner == *owner)
                                .cloned()
                                .collect();
                            let mut plan = plan(owner, mine, live.clone(), breaker, fleet.deletes);
                            if let (Owner::Fleet, Some(hold)) = (owner, &fleet.hold) {
                                plan.hold(hold.clone());
                            }
                            plan
                        })
                        .collect(),
                },
                Err(error) => Scoped::Unread {
                    scope: scope.clone(),
                    error: error.clone(),
                },
            })
            .collect()
    }
}

fn plans<K: Kind>(scopes: &mut [Scoped<K>]) -> impl Iterator<Item = &mut Plan<K>> {
    scopes.iter_mut().flat_map(|scoped| match scoped {
        Scoped::Read { plans, .. } => plans.as_mut_slice(),
        Scoped::Unread { .. } => &mut [],
    })
}

fn steps<K: Kind>(scopes: &[Scoped<K>]) -> impl Iterator<Item = &Step<K>> {
    scopes
        .iter()
        .flat_map(|scoped| match scoped {
            Scoped::Read { plans, .. } => plans.as_slice(),
            Scoped::Unread { .. } => &[],
        })
        .flat_map(|plan| &plan.steps)
}

fn deletes<K: Kind>(scopes: &[Scoped<K>]) -> usize {
    steps(scopes)
        .filter(|step| matches!(step, Step::Delete { .. }))
        .count()
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
