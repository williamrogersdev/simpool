mod doctor;
mod hid;
mod mcp;
mod pool;
mod simctl;
mod state;
mod view;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use serde_json::json;
use std::process::ExitCode;
use std::time::Duration;

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
    /// Tap the screen. x and y are fractions of the screen from the top left, 0 to 1
    Tap {
        x: f64,
        y: f64,
        /// Hold for this many milliseconds, for a long press
        #[arg(long, default_value_t = 60)]
        hold: u64,
    },
    /// Swipe from one point to another, in fractions of the screen
    Swipe {
        from_x: f64,
        from_y: f64,
        to_x: f64,
        to_y: f64,
        /// How long the swipe takes, in milliseconds
        #[arg(long, default_value_t = 300)]
        duration: u64,
    },
    /// Type text into the focused field, as a hardware keyboard would
    Type { text: String },
    /// Press one key: return, delete, escape, tab, space, up, down, left, right
    Key { name: String },
    /// Press a hardware button: home, lock, side, siri
    Button { name: String },
    /// Watch and control the leased simulator in a browser, on this Mac only
    View {
        /// Port to listen on; a free one when omitted
        #[arg(long, default_value_t = 0)]
        port: u16,
    },
    /// Run as an MCP server on stdio, so a coding agent can use the pool as tools
    Mcp,
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

fn fraction(value: f64) -> Result<()> {
    if !(0.0..=1.0).contains(&value) {
        bail!("{value} is not a fraction of the screen. Use 0 to 1, such as 0.5 for the middle");
    }
    Ok(())
}

fn input(store: &state::Store, owner: &Owner) -> Result<hid::Hid> {
    hid::Hid::for_device(&pool::held(store, owner)?)
}

fn execute(cli: Cli) -> Result<(Report, bool)> {
    if let Command::Doctor = cli.command {
        let checks = doctor::checks();
        let healthy = checks.iter().all(|check| check.ok);
        return Ok((Report { text: doctor::render(&checks), json: json!({ "healthy": healthy, "checks": checks }) }, healthy));
    }
    let store = state::Store::open()?;
    let owner = owner(cli.owner)?;
    let as_json = cli.json;
    let report = match cli.command {
        Command::View { port } => {
            let udid = pool::held(&store, &owner)?;
            let input = hid::Hid::for_device(&udid)?;
            view::serve(&udid, &input, port, |address| {
                if as_json {
                    println!("{}", json!({ "udid": udid, "url": address }));
                } else {
                    println!("{address}\nOpen that address to watch and control {udid}. Ctrl-C stops it.");
                }
            })?;
            Report { text: "viewer stopped".into(), json: json!({ "stopped": true }) }
        }
        Command::Mcp => {
            mcp::serve(store, owner)?;
            return Ok((Report { text: String::new(), json: json!(null) }, true));
        }
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
        Command::Tap { x, y, hold } => {
            fraction(x)?;
            fraction(y)?;
            input(&store, &owner)?.tap(x, y, Duration::from_millis(hold))?;
            Report { text: format!("tapped {x} {y}"), json: json!({ "tapped": [x, y] }) }
        }
        Command::Swipe { from_x, from_y, to_x, to_y, duration } => {
            for value in [from_x, from_y, to_x, to_y] {
                fraction(value)?;
            }
            input(&store, &owner)?.swipe((from_x, from_y), (to_x, to_y), Duration::from_millis(duration))?;
            Report { text: format!("swiped {from_x} {from_y} to {to_x} {to_y}"), json: json!({ "swiped": [from_x, from_y, to_x, to_y] }) }
        }
        Command::Type { text } => {
            input(&store, &owner)?.type_text(&text)?;
            Report { text: format!("typed {} character(s)", text.chars().count()), json: json!({ "typed": text.chars().count() }) }
        }
        Command::Key { name } => {
            let usage = hid::named_key(&name).with_context(|| format!("no key named `{name}`. Use return, delete, escape, tab, space, up, down, left or right"))?;
            input(&store, &owner)?.key(usage, false)?;
            Report { text: format!("pressed {name}"), json: json!({ "pressed": name }) }
        }
        Command::Button { name } => {
            let code = hid::named_button(&name).with_context(|| format!("no button named `{name}`. Use home, lock, side or siri"))?;
            input(&store, &owner)?.button(code)?;
            Report { text: format!("pressed {name}"), json: json!({ "pressed": name }) }
        }
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
            if report.json.is_null() {
                return ExitCode::SUCCESS;
            }
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
