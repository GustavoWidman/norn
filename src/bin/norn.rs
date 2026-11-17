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
        } => {
            let timing = timing(&run, lock_wait.into());
            match norn::converge(
                &run.desired,
                run.token_file.as_deref(),
                &state_dir,
                timing,
                run.max_deletes,
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
