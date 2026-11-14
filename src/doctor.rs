use serde::Serialize;

use crate::pool::{human_size, max_booted};
use crate::simctl::{self, run};

#[derive(Serialize)]
pub struct Check {
    pub name: &'static str,
    pub ok: bool,
    pub detail: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fix: Option<String>,
}

fn pass(name: &'static str, detail: impl Into<String>) -> Check {
    Check { name, ok: true, detail: detail.into(), fix: None }
}

fn warn(name: &'static str, detail: impl Into<String>, fix: impl Into<String>) -> Check {
    Check { name, ok: false, detail: detail.into(), fix: Some(fix.into()) }
}

fn xcode() -> Check {
    const SELECT: &str = "sudo xcode-select -s /Applications/Xcode.app/Contents/Developer";
    match run("xcode-select", &["-p"], 5) {
        Ok(path) if path.trim().ends_with(".app/Contents/Developer") => pass("xcode", path.trim()),
        Ok(path) => warn("xcode", format!("xcode-select points at {}, which is not an Xcode.app, so simctl is missing", path.trim()), SELECT),
        Err(error) => warn("xcode", error.to_string(), format!("install Xcode, then {SELECT}")),
    }
}

fn license() -> Check {
    match run("xcodebuild", &["-license", "check"], 10) {
        Ok(_) => pass("xcode license", "accepted"),
        Err(error) => warn("xcode license", error.to_string(), "sudo xcodebuild -license accept"),
    }
}

fn locale() -> Check {
    let setting = ["LC_ALL", "LC_CTYPE", "LANG"].iter().find_map(|key| std::env::var(key).ok().filter(|value| !value.trim().is_empty()).map(|value| format!("{key}={value}")));
    match setting {
        Some(setting) if setting.to_lowercase().replace('-', "").contains("utf8") => pass("locale", setting),
        Some(setting) => warn("locale", format!("{setting} is not UTF-8, so CocoaPods crashes in `pod install`"), "export LANG=en_US.UTF-8 LC_ALL=en_US.UTF-8"),
        None => warn("locale", "LANG, LC_CTYPE and LC_ALL are unset, so CocoaPods crashes in `pod install`", "export LANG=en_US.UTF-8 LC_ALL=en_US.UTF-8"),
    }
}

fn runtimes() -> Check {
    match simctl::list_runtimes() {
        Ok(found) if found.is_empty() => warn("ios runtime", "no iOS simulator runtime is installed", "Xcode > Settings > Components, or xcodebuild -downloadPlatform iOS"),
        Ok(found) => pass("ios runtime", found.iter().map(|runtime| runtime.version.as_str()).collect::<Vec<_>>().join(", ")),
        Err(error) => warn("ios runtime", error.to_string(), "fix the checks above, then run simpool doctor again"),
    }
}

fn service_and_devices() -> Vec<Check> {
    let started = std::time::Instant::now();
    let devices = match simctl::list_devices() {
        Ok(devices) => devices,
        Err(error) => {
            return vec![warn("simulator service", error.to_string(), "killall -9 com.apple.CoreSimulator.CoreSimulatorService (it restarts itself; booted simulators shut down)")];
        }
    };
    let mut checks = vec![pass("simulator service", format!("answered in {} ms", started.elapsed().as_millis()))];
    let booted = devices.iter().filter(|device| device.booted()).count();
    let cap = max_booted();
    checks.push(if booted > cap {
        warn("booted simulators", format!("{booted} booted, over the cap of {cap}; each holds 1 to 3 GB of memory"), "simpool gc --apply, then shut down the ones nobody is using")
    } else {
        pass("booted simulators", format!("{booted} of {cap}"))
    });
    let unavailable = devices.iter().filter(|device| !device.available).count();
    checks.push(if unavailable > 0 {
        warn("unavailable devices", format!("{unavailable} device(s) belong to a runtime that is no longer installed"), "xcrun simctl delete unavailable")
    } else {
        pass("unavailable devices", "none")
    });
    let total: u64 = devices.iter().map(|device| device.size_bytes).sum();
    let detail = format!("{} devices, {}", devices.len(), human_size(total));
    checks.push(if total > 40 * 1_073_741_824 {
        warn("simulator disk", detail, "simpool status shows what the pool holds; delete simulators you no longer need in Xcode > Devices and Simulators")
    } else {
        pass("simulator disk", detail)
    });
    checks
}

fn processes() -> Check {
    let needed = simctl::SIMULATOR_PROCESSES + simctl::process_margin();
    match simctl::process_room() {
        Ok(room) if room.free() < needed => warn(
            "process limit",
            format!("{} of {} processes in use; a simulator needs about {} more, and going over the limit stops every shell from starting commands", room.used, room.limit, simctl::SIMULATOR_PROCESSES),
            format!("shut down simulators nobody is using, or {}", simctl::RAISE_LIMIT),
        ),
        Ok(room) => pass("process limit", format!("{} of {} in use, room for {} more simulator(s)", room.used, room.limit, room.free().saturating_sub(simctl::process_margin()) / simctl::SIMULATOR_PROCESSES)),
        Err(error) => warn("process limit", error.to_string(), simctl::RAISE_LIMIT),
    }
}

pub fn checks() -> Vec<Check> {
    let xcode = xcode();
    let usable = xcode.ok;
    let mut all = vec![xcode, locale(), processes()];
    if usable {
        all.push(license());
        all.push(runtimes());
        all.extend(service_and_devices());
    }
    all
}

pub fn render(checks: &[Check]) -> String {
    let mut lines: Vec<String> = Vec::new();
    for check in checks {
        lines.push(format!("{} {}: {}", if check.ok { "ok  " } else { "WARN" }, check.name, check.detail));
        if let Some(fix) = &check.fix {
            lines.push(format!("     fix: {fix}"));
        }
    }
    let problems = checks.iter().filter(|check| !check.ok).count();
    lines.push(if problems == 0 { "Everything simpool needs is working.".to_string() } else { format!("{problems} problem(s) found.") });
    lines.join("\n")
}
