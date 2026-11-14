use anyhow::{Context, Result, bail};
use serde::Serialize;
use serde_json::{Value, json};
use std::path::Path;

use crate::simctl::{self, SLOW, SimDevice, simctl};
use crate::state::{Device, Lease, State, Store, Template, now};

pub const DEFAULT_DEVICE: &str = "iPhone 17 Pro";
const POOL_PREFIX: &str = "simpool-";

pub struct Owner {
    pub name: String,
    pub path: String,
}

pub struct Report {
    pub text: String,
    pub json: Value,
}

pub fn max_booted() -> usize {
    std::env::var("SIMPOOL_MAX_BOOTED").ok().and_then(|value| value.parse().ok()).unwrap_or(4)
}

pub fn parse_duration(text: &str) -> Result<u64> {
    let text = text.trim();
    let (digits, unit) = match text.char_indices().find(|(_, letter)| !letter.is_ascii_digit()) {
        Some((index, _)) => text.split_at(index),
        None => (text, "s"),
    };
    let amount: u64 = digits.parse().with_context(|| format!("`{text}` is not a duration; use 45m, 8h or 2d"))?;
    let scale = match unit {
        "s" => 1,
        "m" => 60,
        "h" => 3600,
        "d" => 86400,
        _ => bail!("`{text}` is not a duration; use 45m, 8h or 2d"),
    };
    Ok(amount * scale)
}

pub fn human_span(secs: u64) -> String {
    match secs {
        0..=59 => format!("{secs}s"),
        60..=3599 => format!("{}m", secs / 60),
        3600..=172_799 => format!("{}h", secs / 3600),
        _ => format!("{}d", secs / 86400),
    }
}

pub fn human_size(bytes: u64) -> String {
    const GB: f64 = 1_073_741_824.0;
    let bytes = bytes as f64;
    if bytes >= GB { format!("{:.1} GB", bytes / GB) } else { format!("{:.0} MB", bytes / 1_048_576.0) }
}

fn short(identifier: &str) -> &str {
    identifier.rsplit('.').next().unwrap_or(identifier)
}

fn check_cap(sims: &[SimDevice]) -> Result<()> {
    let booted: Vec<&str> = sims.iter().filter(|sim| sim.booted()).map(|sim| sim.name.as_str()).collect();
    let cap = max_booted();
    if booted.len() >= cap {
        bail!(
            "{} simulators are already booted (the cap is {cap}): {}. Free one with `simpool release` or `simpool gc --apply`, or raise SIMPOOL_MAX_BOOTED.",
            booted.len(),
            booted.join(", ")
        );
    }
    let room = simctl::process_room()?;
    let needed = simctl::SIMULATOR_PROCESSES + simctl::process_margin();
    if room.free() < needed {
        bail!(
            "This account runs {} of the {} processes it is allowed, and a simulator needs about {} more plus a margin of {}. Going over the limit stops every shell on this machine from starting commands. Shut a simulator down ({}), or raise the limit: {}.",
            room.used,
            room.limit,
            simctl::SIMULATOR_PROCESSES,
            simctl::process_margin(),
            if booted.is_empty() { "none are booted".to_string() } else { format!("booted now: {}", booted.join(", ")) },
            simctl::RAISE_LIMIT
        );
    }
    Ok(())
}

fn ensure_template(store: &Store, kind: &simctl::DeviceType, runtime: &simctl::Runtime) -> Result<String> {
    let sims = simctl::list_devices()?;
    let known = store.with(|state| {
        state.sync(&sims);
        Ok(state.templates.iter().find(|template| template.device_type == kind.identifier && template.runtime == runtime.identifier).map(|template| template.udid.clone()))
    })?;
    if let Some(udid) = known {
        if sims.iter().any(|sim| sim.udid == udid && sim.booted()) {
            simctl::shutdown(&udid)?;
        }
        return Ok(udid);
    }
    let name = format!("{POOL_PREFIX}template {} {}", kind.name, runtime.version);
    eprintln!("simpool: first use of {} on iOS {}: building a template (about a minute, once)", kind.name, runtime.version);
    let udid = simctl(&["create", &name, &kind.identifier, &runtime.identifier], SLOW)?.trim().to_string();
    store.with(|state| {
        state.templates.push(Template { udid: udid.clone(), name, device_type: kind.identifier.clone(), runtime: runtime.identifier.clone() });
        Ok(())
    })?;
    simctl::boot_and_wait(&udid)?;
    simctl::shutdown(&udid)?;
    Ok(udid)
}

fn fresh_lease(owner: &Owner, ttl: u64) -> Lease {
    let stamp = now();
    Lease { owner: owner.name.clone(), owner_path: owner.path.clone(), leased_at: stamp, expires_at: stamp + ttl, ttl_secs: ttl }
}

pub fn lease(store: &Store, owner: &Owner, device: &str, runtime: Option<&str>, ttl: u64) -> Result<Report> {
    let kind = simctl::resolve_device_type(device)?;
    let runtime = simctl::resolve_runtime(runtime)?;
    let sims = simctl::list_devices()?;
    let already_running = store.read()?.leased_by(&owner.name).is_some_and(|held| sims.iter().any(|sim| sim.udid == held.udid && sim.booted()));
    if !already_running {
        check_cap(&sims)?;
    }
    let picked = store.with(|state| {
        state.sync(&sims);
        if let Some(held) = state.leased_by(&owner.name) {
            if held.device_type != kind.identifier || held.runtime != runtime.identifier {
                bail!("`{}` already holds {} ({}, {}). Run `simpool release` before leasing a different device.", owner.name, held.name, short(&held.device_type), short(&held.runtime));
            }
            held.lease = Some(fresh_lease(owner, ttl));
            held.last_used_at = now();
            return Ok(Some((held.udid.clone(), held.name.clone())));
        }
        let free = state.devices.iter_mut().find(|candidate| candidate.lease.is_none() && candidate.device_type == kind.identifier && candidate.runtime == runtime.identifier);
        Ok(free.map(|candidate| {
            candidate.lease = Some(fresh_lease(owner, ttl));
            candidate.last_used_at = now();
            (candidate.udid.clone(), candidate.name.clone())
        }))
    })?;
    let (udid, name, created) = match picked {
        Some((udid, name)) => (udid, name, false),
        None => {
            let guard = store.lock("template")?;
            let template = ensure_template(store, &kind, &runtime)?;
            let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|elapsed| elapsed.subsec_nanos()).unwrap_or(0);
            let name = format!("{POOL_PREFIX}{:06x}", (stamp ^ std::process::id()) & 0x00ff_ffff);
            let cloned = simctl(&["clone", &template, &name], SLOW)?;
            drop(guard);
            let udid = cloned.lines().last().unwrap_or("").trim().to_string();
            if udid.is_empty() {
                bail!("`simctl clone` returned no device id");
            }
            store.with(|state| {
                state.devices.push(Device {
                    udid: udid.clone(),
                    name: name.clone(),
                    device_type: kind.identifier.clone(),
                    runtime: runtime.identifier.clone(),
                    created_at: now(),
                    last_used_at: now(),
                    lease: Some(fresh_lease(owner, ttl)),
                });
                Ok(())
            })?;
            (udid, name, true)
        }
    };
    simctl::boot_and_wait(&udid).with_context(|| format!("{name} is leased to `{}` but did not boot. Try `simpool reset`.", owner.name))?;
    Ok(Report {
        text: format!("{udid}\n{name} ({} on iOS {}) is booted for `{}`, held for {}", kind.name, runtime.version, owner.name, human_span(ttl)),
        json: json!({ "udid": udid, "name": name, "device": kind.name, "runtime": runtime.version, "owner": owner.name, "created": created, "expires_in_secs": ttl }),
    })
}

/// The caller's leased device, booted, with its lease renewed.
pub fn held(store: &Store, owner: &Owner) -> Result<String> {
    let sims = simctl::list_devices()?;
    let udid = store.with(|state| {
        state.sync(&sims);
        let device = state.leased_by(&owner.name).with_context(|| format!("`{}` holds no simulator. Run `simpool lease` first.", owner.name))?;
        let stamp = now();
        device.last_used_at = stamp;
        if let Some(lease) = device.lease.as_mut() {
            lease.expires_at = stamp + lease.ttl_secs;
        }
        Ok(device.udid.clone())
    })?;
    if !sims.iter().any(|sim| sim.udid == udid && sim.booted()) {
        simctl::boot_and_wait(&udid)?;
    }
    Ok(udid)
}

pub fn release(store: &Store, owner: &Owner, delete: bool) -> Result<Report> {
    let sims = simctl::list_devices()?;
    let (udid, name) = store.with(|state| {
        state.sync(&sims);
        let device = state.leased_by(&owner.name).with_context(|| format!("`{}` holds no simulator.", owner.name))?;
        device.lease = None;
        device.last_used_at = now();
        Ok((device.udid.clone(), device.name.clone()))
    })?;
    simctl::shutdown(&udid)?;
    if delete {
        simctl(&["delete", &udid], SLOW)?;
        store.with(|state| {
            state.devices.retain(|device| device.udid != udid);
            Ok(())
        })?;
    }
    let outcome = if delete { "deleted" } else { "shut down and returned to the pool" };
    Ok(Report { text: format!("{name} {outcome}"), json: json!({ "udid": udid, "name": name, "deleted": delete }) })
}

pub fn reset(store: &Store, owner: &Owner) -> Result<Report> {
    let udid = held(store, owner)?;
    simctl::shutdown(&udid)?;
    simctl(&["erase", &udid], SLOW)?;
    simctl::boot_and_wait(&udid)?;
    Ok(Report { text: format!("{udid} erased and booted clean"), json: json!({ "udid": udid, "erased": true }) })
}

fn plist_value(plist: &Path, key: &str) -> Option<String> {
    simctl::run("plutil", &["-extract", key, "raw", "-o", "-", &plist.to_string_lossy()], 10).ok().map(|value| value.trim().to_string()).filter(|value| !value.is_empty())
}

pub fn install(store: &Store, owner: &Owner, app: &str) -> Result<Report> {
    let path = Path::new(app);
    if !path.is_dir() || path.extension().is_none_or(|extension| extension != "app") {
        bail!("`{app}` is not an .app bundle. Pass the folder a simulator build produced, such as .../Build/Products/Debug-iphonesimulator/MyApp.app.");
    }
    let plist = path.join("Info.plist");
    if !plist.is_file() {
        bail!("`{app}` has no Info.plist, so the build is incomplete. Build again.");
    }
    let bundle_id = plist_value(&plist, "CFBundleIdentifier").with_context(|| format!("`{}` names no CFBundleIdentifier", plist.display()))?;
    if plist_value(&plist, "DTPlatformName").is_some_and(|platform| platform == "iphoneos") {
        bail!("`{app}` is built for a real iPhone, which the simulator cannot run. Build for the simulator (`expo run:ios`, or `xcodebuild -sdk iphonesimulator`).");
    }
    let udid = held(store, owner)?;
    simctl(&["install", &udid, app], SLOW)?;
    Ok(Report { text: format!("{bundle_id} installed on {udid}"), json: json!({ "udid": udid, "bundle_id": bundle_id }) })
}

pub fn launch(store: &Store, owner: &Owner, bundle_id: &str, args: &[String]) -> Result<Report> {
    let udid = held(store, owner)?;
    let mut command = vec!["launch", "--terminate-running-process", udid.as_str(), bundle_id];
    command.extend(args.iter().map(String::as_str));
    let output = simctl(&command, 60).with_context(|| format!("is `{bundle_id}` installed? Run `simpool install <path to .app>`"))?;
    let pid = output.trim().rsplit(": ").next().and_then(|pid| pid.parse::<u32>().ok());
    Ok(Report { text: format!("{bundle_id} launched on {udid}{}", pid.map(|pid| format!(" (pid {pid})")).unwrap_or_default()), json: json!({ "udid": udid, "bundle_id": bundle_id, "pid": pid }) })
}

pub fn open_url(store: &Store, owner: &Owner, url: &str) -> Result<Report> {
    let udid = held(store, owner)?;
    simctl(&["openurl", &udid, url], 60)?;
    Ok(Report { text: format!("opened {url} on {udid}"), json: json!({ "udid": udid, "url": url }) })
}

pub fn shot(store: &Store, owner: &Owner, path: Option<String>) -> Result<Report> {
    let udid = held(store, owner)?;
    let path = path.unwrap_or_else(|| format!("simpool-{}.png", now()));
    simctl(&["io", &udid, "screenshot", &path], 60)?;
    let absolute = std::fs::canonicalize(&path).map(|full| full.to_string_lossy().into_owned()).unwrap_or(path);
    Ok(Report { text: absolute.clone(), json: json!({ "udid": udid, "path": absolute }) })
}

pub fn status(store: &Store) -> Result<Report> {
    let sims = simctl::list_devices()?;
    let state = store.with(|state| {
        state.sync(&sims);
        Ok(State { devices: state.devices.clone(), templates: state.templates.clone() })
    })?;
    let stamp = now();
    let owner_width = state.devices.iter().filter_map(|device| device.lease.as_ref()).map(|lease| lease.owner.chars().count()).max().unwrap_or(0).max(5);
    let mut lines = vec![format!("{:<16} {:<9} {:<owner_width$} {:<9} {:<8} DEVICE", "NAME", "STATE", "OWNER", "EXPIRES", "SIZE")];
    let mut rows = Vec::new();
    for device in &state.devices {
        let sim = sims.iter().find(|sim| sim.udid == device.udid);
        let live = sim.map(|sim| sim.state.as_str()).unwrap_or("Missing");
        let size = sim.map(|sim| sim.size_bytes).unwrap_or(0);
        let (owner, expires) = match &device.lease {
            Some(lease) if lease.expires_at > stamp => (lease.owner.as_str(), format!("in {}", human_span(lease.expires_at - stamp))),
            Some(lease) => (lease.owner.as_str(), "expired".to_string()),
            None => ("free", "-".to_string()),
        };
        lines.push(format!("{:<16} {:<9} {:<owner_width$} {:<9} {:<8} {} · {}", device.name, live, owner, expires, human_size(size), short(&device.device_type), short(&device.runtime)));
        rows.push(json!({ "udid": device.udid, "name": device.name, "state": live, "owner": device.lease.as_ref().map(|lease| &lease.owner), "expires_at": device.lease.as_ref().map(|lease| lease.expires_at), "size_bytes": size, "device_type": device.device_type, "runtime": device.runtime }));
    }
    if state.devices.is_empty() {
        lines.push("(no pool devices yet: run `simpool lease`)".to_string());
    }
    let ours = |udid: &str| state.devices.iter().any(|device| device.udid == udid) || state.templates.iter().any(|template| template.udid == udid);
    let booted: Vec<&SimDevice> = sims.iter().filter(|sim| sim.booted()).collect();
    let foreign: Vec<&str> = booted.iter().filter(|sim| !ours(&sim.udid)).map(|sim| sim.name.as_str()).collect();
    let total: u64 = sims.iter().map(|sim| sim.size_bytes).sum();
    lines.push(String::new());
    lines.push(format!("Booted on this machine: {} of {}{}", booted.len(), max_booted(), if foreign.is_empty() { String::new() } else { format!(" ({} not from simpool: {})", foreign.len(), foreign.join(", ")) }));
    lines.push(format!("All simulators on disk: {} devices, {}", sims.len(), human_size(total)));
    Ok(Report { text: lines.join("\n"), json: json!({ "devices": rows, "templates": state.templates, "booted": booted.len(), "max_booted": max_booted(), "booted_outside_pool": foreign, "all_simulators": { "count": sims.len(), "size_bytes": total } }) })
}

#[derive(Debug, PartialEq, Serialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum Action {
    Expire { udid: String, name: String, owner: String, reason: String },
    Delete { udid: String, name: String, reason: String },
}

pub fn plan_gc(state: &State, sims: &[SimDevice], stamp: u64, keep_free: usize, idle_secs: u64, path_exists: &dyn Fn(&str) -> bool) -> Vec<Action> {
    let mut actions = Vec::new();
    for device in &state.devices {
        let Some(lease) = &device.lease else { continue };
        let reason = if lease.expires_at <= stamp {
            Some(format!("its lease ended {} ago", human_span(stamp - lease.expires_at)))
        } else if !path_exists(&lease.owner_path) {
            Some(format!("its folder {} is gone", lease.owner_path))
        } else {
            None
        };
        if let Some(reason) = reason {
            actions.push(Action::Expire { udid: device.udid.clone(), name: device.name.clone(), owner: lease.owner.clone(), reason });
        }
    }
    let mut free: Vec<&Device> = state.devices.iter().filter(|device| device.lease.is_none()).collect();
    free.sort_by_key(|device| std::cmp::Reverse(device.last_used_at));
    for device in free.into_iter().skip(keep_free) {
        let idle = stamp.saturating_sub(device.last_used_at);
        if idle >= idle_secs {
            actions.push(Action::Delete { udid: device.udid.clone(), name: device.name.clone(), reason: format!("free and unused for {}", human_span(idle)) });
        }
    }
    for sim in sims {
        let recorded = state.devices.iter().any(|device| device.udid == sim.udid) || state.templates.iter().any(|template| template.udid == sim.udid);
        if sim.name.starts_with(POOL_PREFIX) && !recorded {
            actions.push(Action::Delete { udid: sim.udid.clone(), name: sim.name.clone(), reason: "named like a pool device but missing from simpool's records".to_string() });
        }
    }
    actions
}

/// Simulators someone created by hand: not simpool's, and not one of the
/// stock devices Xcode makes, which are named after their device type.
pub fn hand_made<'a>(sims: &'a [SimDevice], stock_names: &[String]) -> Vec<&'a SimDevice> {
    sims.iter().filter(|sim| !sim.name.starts_with(POOL_PREFIX) && !stock_names.iter().any(|name| name == &sim.name)).collect()
}

pub fn gc(store: &Store, apply: bool, keep_free: usize, idle_secs: u64) -> Result<Report> {
    let sims = simctl::list_devices()?;
    let actions = store.with(|state| {
        state.sync(&sims);
        Ok(plan_gc(state, &sims, now(), keep_free, idle_secs, &|path| Path::new(path).exists()))
    })?;
    let mut lines = Vec::new();
    for action in &actions {
        match action {
            Action::Expire { udid, name, owner, reason } => {
                lines.push(format!("release {name} from `{owner}`: {reason}"));
                if apply {
                    simctl::shutdown(udid)?;
                    store.with(|state| {
                        if let Some(device) = state.devices.iter_mut().find(|device| &device.udid == udid) {
                            device.lease = None;
                            device.last_used_at = now();
                        }
                        Ok(())
                    })?;
                }
            }
            Action::Delete { udid, name, reason } => {
                lines.push(format!("delete {name}: {reason}"));
                if apply {
                    simctl::shutdown(udid)?;
                    simctl(&["delete", udid], SLOW)?;
                    store.with(|state| {
                        state.devices.retain(|device| &device.udid != udid);
                        Ok(())
                    })?;
                }
            }
        }
    }
    let stock: Vec<String> = simctl::list_device_types().map(|kinds| kinds.into_iter().map(|kind| kind.name).collect()).unwrap_or_default();
    let hand_made = hand_made(&sims, &stock);
    if !hand_made.is_empty() {
        let total: u64 = hand_made.iter().map(|sim| sim.size_bytes).sum();
        lines.push(format!("{} hand-made simulator(s) hold {}. simpool will not touch them; delete one with `xcrun simctl delete <name>` once nobody uses it:", hand_made.len(), human_size(total)));
        for sim in &hand_made {
            lines.push(format!("  {} ({}, {})", sim.name, sim.state, human_size(sim.size_bytes)));
        }
    }
    let untouched = sims.iter().filter(|sim| !sim.name.starts_with(POOL_PREFIX)).count();
    let summary = match (actions.is_empty(), apply) {
        (true, _) => "Nothing to clean up.".to_string(),
        (false, true) => format!("Done: {} change(s) made.", actions.len()),
        (false, false) => format!("{} change(s) planned. Nothing was changed: run `simpool gc --apply`.", actions.len()),
    };
    lines.push(summary);
    lines.push(format!("{untouched} simulator(s) that simpool did not create were left alone."));
    let hand_made_json: Vec<Value> = hand_made.iter().map(|sim| json!({ "udid": sim.udid, "name": sim.name, "state": sim.state, "size_bytes": sim.size_bytes })).collect();
    Ok(Report { text: lines.join("\n"), json: json!({ "applied": apply, "actions": actions, "left_alone": untouched, "hand_made": hand_made_json }) })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn device(udid: &str, last_used_at: u64, lease: Option<Lease>) -> Device {
        Device { udid: udid.into(), name: format!("simpool-{udid}"), device_type: "type".into(), runtime: "runtime".into(), created_at: 0, last_used_at, lease }
    }

    fn sim(udid: &str, name: &str) -> SimDevice {
        SimDevice { udid: udid.into(), name: name.into(), state: "Shutdown".into(), available: true, size_bytes: 0 }
    }

    fn lease(owner: &str, path: &str, expires_at: u64) -> Lease {
        Lease { owner: owner.into(), owner_path: path.into(), leased_at: 0, expires_at, ttl_secs: 100 }
    }

    #[test]
    fn hand_made_excludes_stock_and_pool_devices() {
        let sims = [sim("A", "iPhone 17 Pro"), sim("B", "simpool-abc123"), sim("C", "my-own-simulator"), sim("D", "simpool-template iPhone 17 Pro 26.5")];
        let found = hand_made(&sims, &["iPhone 17 Pro".to_string()]);
        assert_eq!(found.iter().map(|sim| sim.udid.as_str()).collect::<Vec<_>>(), ["C"]);
    }

    #[test]
    fn durations() {
        assert_eq!(parse_duration("45m").unwrap(), 2700);
        assert_eq!(parse_duration("8h").unwrap(), 28800);
        assert_eq!(parse_duration("2d").unwrap(), 172_800);
        assert_eq!(parse_duration("30").unwrap(), 30);
        assert!(parse_duration("soon").is_err());
        assert!(parse_duration("5w").is_err());
    }

    #[test]
    fn gc_expires_ended_leases_and_gone_folders_only() {
        let state = State {
            devices: vec![
                device("ended", 0, Some(lease("old", "/here", 50))),
                device("gone", 0, Some(lease("moved", "/missing", 500))),
                device("live", 0, Some(lease("active", "/here", 500))),
            ],
            templates: vec![],
        };
        let actions = plan_gc(&state, &[], 100, 2, 1000, &|path| path == "/here");
        let touched: Vec<&str> = actions.iter().map(|action| match action { Action::Expire { udid, .. } | Action::Delete { udid, .. } => udid.as_str() }).collect();
        assert_eq!(touched, ["ended", "gone"]);
        assert!(actions.iter().all(|action| matches!(action, Action::Expire { .. })));
    }

    #[test]
    fn gc_keeps_the_most_recent_free_devices() {
        let state = State { devices: vec![device("oldest", 10, None), device("newest", 900, None), device("middle", 500, None), device("recent-extra", 990, None)], templates: vec![] };
        let actions = plan_gc(&state, &[], 2000, 2, 1000, &|_| true);
        assert_eq!(actions.len(), 2);
        assert!(matches!(&actions[0], Action::Delete { udid, .. } if udid == "middle"));
        assert!(matches!(&actions[1], Action::Delete { udid, .. } if udid == "oldest"));
    }

    #[test]
    fn gc_never_plans_a_device_simpool_did_not_create() {
        let state = State { devices: vec![], templates: vec![Template { udid: "T".into(), name: "simpool-template".into(), device_type: "type".into(), runtime: "runtime".into() }] };
        let sims = [sim("T", "simpool-template iPhone"), sim("X", "my-own-simulator"), sim("Y", "iPhone 17 Pro"), sim("Z", "simpool-orphan")];
        let actions = plan_gc(&state, &sims, 100, 2, 1000, &|_| true);
        assert_eq!(actions.len(), 1);
        assert!(matches!(&actions[0], Action::Delete { udid, .. } if udid == "Z"));
    }
}
