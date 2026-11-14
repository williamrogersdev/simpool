mod doctor;
mod pool;
mod simctl;
mod state;

use anyhow::Result;
use clap::{Parser, Subcommand};
use serde_json::json;
use std::process::ExitCode;

use pool::{Owner, Report};

#[derive(Parser)]
#[command(name = "simpool", version, about = "A pool of iOS simulators: one per worktree, booted fast, cleaned up after.")]
struct Cli {
    /// Print machine-readable JSON
    #[arg(long, global = true)]
    json: bool,
    /// Who holds the simulator. Defaults to the name of the current git worktree
    #[arg(long, global = true, env = "SIMPOOL_OWNER")]
    owner: Option<String>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Get a booted simulator for this worktree (the same one each time)
    Lease {
        #[arg(long, default_value = pool::DEFAULT_DEVICE)]
        device: String,
        /// iOS version such as 26.5; the newest installed when omitted
        #[arg(long)]
        runtime: Option<String>,
        /// How long the lease lasts without use, such as 45m, 8h or 2d
        #[arg(long, default_value = "8h")]
        ttl: String,
    },
    /// Shut the simulator down and return it to the pool
    Release {
        /// Delete the device instead of keeping it for the next lease
        #[arg(long)]
        delete: bool,
    },
    /// Erase the leased simulator and boot it clean
    Reset,
    /// Print the leased simulator's id, for use with xcrun simctl or Xcode
    Udid,
    /// Install a simulator .app build
    Install { app: String },
    /// Launch an installed app, restarting it if it is running
    Launch {
        bundle_id: String,
        #[arg(trailing_var_arg = true)]
        args: Vec<String>,
    },
    /// Open a URL or deep link
    OpenUrl { url: String },
    /// Save a screenshot and print its path
    Shot { path: Option<String> },
    /// Show the pool, who holds what, and what is booted
    Status,
    /// Plan the cleanup of ended leases and idle devices; --apply makes the changes
    Gc {
        #[arg(long)]
        apply: bool,
        /// Free devices to keep ready for the next lease
        #[arg(long, default_value_t = 2)]
        keep_free: usize,
        /// Delete extra free devices unused for this long
        #[arg(long, default_value = "1d")]
        idle: String,
    },
    /// Check Xcode, the simulator service, runtimes and disk
    Doctor,
}

fn owner(named: Option<String>) -> Result<Owner> {
    let root = simctl::run("git", &["rev-parse", "--show-toplevel"], 5).map(|path| path.trim().to_string()).ok().filter(|path| !path.is_empty());
    let path = match root {
        Some(path) => path,
        None => std::env::current_dir()?.to_string_lossy().into_owned(),
    };
    let name = named.unwrap_or_else(|| std::path::Path::new(&path).file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_else(|| path.clone()));
    Ok(Owner { name, path })
}

fn execute(cli: Cli) -> Result<(Report, bool)> {
    if let Command::Doctor = cli.command {
        let checks = doctor::checks();
        let healthy = checks.iter().all(|check| check.ok);
        return Ok((Report { text: doctor::render(&checks), json: json!({ "healthy": healthy, "checks": checks }) }, healthy));
    }
    let store = state::Store::open()?;
    let owner = owner(cli.owner)?;
    let report = match cli.command {
        Command::Lease { device, runtime, ttl } => pool::lease(&store, &owner, &device, runtime.as_deref(), pool::parse_duration(&ttl)?)?,
        Command::Release { delete } => pool::release(&store, &owner, delete)?,
        Command::Reset => pool::reset(&store, &owner)?,
        Command::Udid => {
            let udid = pool::held(&store, &owner)?;
            Report { text: udid.clone(), json: json!({ "udid": udid }) }
        }
        Command::Install { app } => pool::install(&store, &owner, &app)?,
        Command::Launch { bundle_id, args } => pool::launch(&store, &owner, &bundle_id, &args)?,
        Command::OpenUrl { url } => pool::open_url(&store, &owner, &url)?,
        Command::Shot { path } => pool::shot(&store, &owner, path)?,
        Command::Status => pool::status(&store)?,
        Command::Gc { apply, keep_free, idle } => pool::gc(&store, apply, keep_free, pool::parse_duration(&idle)?)?,
        Command::Doctor => unreachable!("handled above"),
    };
    Ok((report, true))
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let as_json = cli.json;
    match execute(cli) {
        Ok((report, success)) => {
            if as_json {
                println!("{}", serde_json::to_string_pretty(&report.json).unwrap_or_default());
            } else {
                println!("{}", report.text);
            }
            if success { ExitCode::SUCCESS } else { ExitCode::FAILURE }
        }
        Err(error) => {
            if as_json {
                println!("{}", json!({ "error": format!("{error:#}") }));
            } else {
                eprintln!("simpool: {error:#}");
            }
            ExitCode::FAILURE
        }
    }
}
