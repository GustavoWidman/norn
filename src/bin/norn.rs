use std::{path::PathBuf, process::ExitCode, time::Duration};

use clap::{Parser, Subcommand};

const DESIRED: &str = "/etc/norn/desired.json";
const STATE: &str = "/var/lib/norn";

#[derive(Parser)]
#[command(
    version,
    about = "bring cloudflare in line with the fleet's nix configuration"
)]
struct Args {
    #[command(subcommand)]
    command: Command,
}

#[derive(clap::Args)]
struct Run {
    #[arg(long, env = "NORN_DESIRED", default_value = DESIRED)]
    desired: PathBuf,
    #[arg(long, env = "NORN_TOKEN_FILE")]
    token_file: Option<PathBuf>,
    /// the whole run's budget against cloudflare
    #[arg(long, default_value = "30s")]
    deadline: humantime::Duration,
    /// one request's budget
    #[arg(long, default_value = "5s")]
    request_timeout: humantime::Duration,
    /// when a run plans more deletes than this, it sends none of them
    #[arg(long, env = "NORN_MAX_DELETES", default_value_t = 10)]
    max_deletes: usize,
    /// the fleet's rollout file: the revision the whole fleet runs, and when
    /// that was last checked. without one, retiring shared records are never
    /// deleted
    #[arg(long, env = "NORN_ROLLOUT")]
    rollout: Option<PathBuf>,
    /// past this age, the rollout file counts as unknown and shared records
    /// are held
    #[arg(long, env = "NORN_ROLLOUT_MAX_AGE", default_value = "30m")]
    rollout_max_age: humantime::Duration,
}

impl Run {
    fn rollout(&self) -> Option<norn::RolloutFile> {
        self.rollout.as_ref().map(|path| norn::RolloutFile {
            path: path.clone(),
            max_age: self.rollout_max_age.into(),
        })
    }
}

/// Web Push notices to the owner's devices: one when runs have stayed
/// troubled for --push-after, and one when that ends
#[derive(clap::Args)]
struct Push {
    /// the VAPID private key: the base64url of a P-256 scalar
    #[arg(
        long,
        env = "NORN_PUSH_VAPID_KEY_FILE",
        requires_all = ["push_subscriptions_file", "push_subject", "push_navigate"]
    )]
    push_vapid_key_file: Option<PathBuf>,
    /// a JSON array of `PushSubscription.toJSON()` objects, each with a name
    #[arg(
        long,
        env = "NORN_PUSH_SUBSCRIPTIONS_FILE",
        requires = "push_vapid_key_file"
    )]
    push_subscriptions_file: Option<PathBuf>,
    /// the VAPID subject: an https URL or a mailto:
    #[arg(long, env = "NORN_PUSH_SUBJECT", requires = "push_vapid_key_file")]
    push_subject: Option<String>,
    /// the https URL a tap on a notice opens; {host} stands for this machine
    #[arg(long, env = "NORN_PUSH_NAVIGATE", requires = "push_vapid_key_file")]
    push_navigate: Option<String>,
    /// how long runs stay troubled before the devices hear of it
    #[arg(long, env = "NORN_PUSH_AFTER", default_value = "30m")]
    push_after: humantime::Duration,
}

impl Push {
    fn settings(self) -> Option<norn::PushSettings> {
        Some(norn::PushSettings {
            vapid_key_file: self.push_vapid_key_file?,
            subscriptions_file: self.push_subscriptions_file?,
            subject: self.push_subject?,
            navigate: self.push_navigate?,
            after: self.push_after.into(),
        })
    }
}

#[derive(Subcommand)]
enum Command {
    /// act in the desired file's mode; the service runs this, and every run exits 0.
    /// a caller that does not own the state directory is refused and exits 1
    Converge {
        #[command(flatten)]
        run: Run,
        #[arg(long, env = "NORN_STATE_DIR", default_value = STATE)]
        state_dir: PathBuf,
        /// how long to wait for another run of this machine
        #[arg(long, default_value = "10s")]
        lock_wait: humantime::Duration,
        #[command(flatten)]
        push: Push,
    },
    /// print what a run would do; it holds no client that can write
    Report {
        #[command(flatten)]
        run: Run,
        /// also list records already in line and other machines' records
        #[arg(long, short)]
        verbose: bool,
    },
    /// judge a rendered desired file and write its canonical form
    Check {
        #[arg(long = "in")]
        input: PathBuf,
        #[arg(long = "out")]
        output: Option<PathBuf>,
    },
    /// the last run's outcome; exits 1 when troubled, or silent, for longer than --stale
    Status {
        #[arg(long, env = "NORN_STATE_DIR", default_value = STATE)]
        state_dir: PathBuf,
        #[arg(long, default_value = "30m")]
        stale: humantime::Duration,
    },
}

fn timing(run: &Run, lock_wait: Duration) -> norn::Timing {
    norn::Timing {
        deadline: run.deadline.into(),
        per_request: run.request_timeout.into(),
        lock_wait,
    }
}

fn main() -> ExitCode {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let args = match Args::try_parse() {
        Ok(args) => args,
        Err(error)
            if std::env::args().nth(1).as_deref() == Some("converge") && error.use_stderr() =>
        {
            log::error!("{error}");
            return ExitCode::SUCCESS;
        }
        Err(error) => error.exit(),
    };

    match args.command {
        Command::Converge {
            run,
            state_dir,
            lock_wait,
            push,
        } => {
            let timing = timing(&run, lock_wait.into());
            match norn::converge(
                &run.desired,
                run.token_file.as_deref(),
                &state_dir,
                timing,
                run.max_deletes,
                run.rollout().as_ref(),
                push.settings().as_ref(),
            ) {
                Ok(()) => ExitCode::SUCCESS,
                Err(error) => {
                    log::error!("{error:#}");
                    ExitCode::FAILURE
                }
            }
        }
        Command::Report { run, verbose } => {
            match norn::report(
                &run.desired,
                run.token_file.as_deref(),
                timing(&run, Duration::ZERO),
                run.max_deletes,
                run.rollout().as_ref(),
                verbose,
            ) {
                Ok(()) => ExitCode::SUCCESS,
                Err(error) => {
                    log::error!("{error:#}");
                    ExitCode::FAILURE
                }
            }
        }
        Command::Check { input, output } => match norn::check(&input, output.as_deref()) {
            Ok(()) => ExitCode::SUCCESS,
            Err(problems) => {
                eprintln!("{problems}");
                ExitCode::FAILURE
            }
        },
        Command::Status { state_dir, stale } => match norn::status(&state_dir, stale.into()) {
            Ok(true) => ExitCode::SUCCESS,
            Ok(false) => ExitCode::FAILURE,
            Err(error) => {
                log::error!("{error:#}");
                ExitCode::FAILURE
            }
        },
    }
}
