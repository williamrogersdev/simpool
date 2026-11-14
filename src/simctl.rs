use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::io::Read;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

pub const QUICK: u64 = 20;
pub const SLOW: u64 = 240;

/// Runs a command with a hard timeout. Apple's tools hang without one when
/// the CoreSimulator service is stuck.
pub fn run(program: &str, args: &[&str], secs: u64) -> Result<String> {
    let shown = format!("{program} {}", args.join(" "));
    let mut attempt = 0;
    let mut child = loop {
        match Command::new(program).args(args).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn() {
            Ok(child) => break child,
            Err(error) if error.raw_os_error() == Some(libc::EAGAIN) && attempt < 8 => {
                attempt += 1;
                thread::sleep(Duration::from_millis(400 * attempt));
            }
            Err(error) => return Err(error).with_context(|| format!("could not start `{shown}`")),
        }
    };
    let mut out = child.stdout.take().expect("piped stdout");
    let mut err = child.stderr.take().expect("piped stderr");
    let out_reader = thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = out.read_to_end(&mut bytes);
        String::from_utf8_lossy(&bytes).into_owned()
    });
    let err_reader = thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = err.read_to_end(&mut bytes);
        String::from_utf8_lossy(&bytes).into_owned()
    });
    let deadline = Instant::now() + Duration::from_secs(secs);
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            bail!(
                "`{shown}` did not finish within {secs}s. The simulator service may be stuck: run `simpool doctor`."
            );
        }
        thread::sleep(Duration::from_millis(40));
    };
    let stdout = out_reader.join().unwrap_or_default();
    let stderr = err_reader.join().unwrap_or_default();
    if !status.success() {
        let detail = if stderr.trim().is_empty() { stdout.trim() } else { stderr.trim() };
        bail!("`{shown}` failed ({status}): {detail}");
    }
    Ok(stdout)
}

/// A booted simulator runs roughly this many processes.
pub const SIMULATOR_PROCESSES: u64 = 200;

pub struct ProcessRoom {
    pub used: u64,
    pub limit: u64,
}

impl ProcessRoom {
    pub fn free(&self) -> u64 {
        self.limit.saturating_sub(self.used)
    }
}

pub fn process_margin() -> u64 {
    std::env::var("SIMPOOL_PROCESS_MARGIN").ok().and_then(|value| value.parse().ok()).unwrap_or(150)
}

/// How close this account is to its process limit. Going over it stops every
/// shell on the machine from starting commands, so a boot checks this first.
pub fn process_room() -> Result<ProcessRoom> {
    let uid = unsafe { libc::getuid() }.to_string();
    let used = run("ps", &["-u", &uid, "-o", "pid="], 10)?.lines().count() as u64;
    let mut limits = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
    if unsafe { libc::getrlimit(libc::RLIMIT_NPROC, &mut limits) } != 0 {
        bail!("could not read the process limit");
    }
    Ok(ProcessRoom { used, limit: limits.rlim_cur })
}

pub const RAISE_LIMIT: &str = "check `ulimit -Su` (the limit this shell allows) against `launchctl limit maxproc` (what macOS allows), and raise the lower one only if the machine has memory to spare";

pub fn simctl(args: &[&str], secs: u64) -> Result<String> {
    let mut full = vec!["simctl"];
    full.extend_from_slice(args);
    run("xcrun", &full, secs)
}

#[derive(Debug, Clone)]
pub struct SimDevice {
    pub udid: String,
    pub name: String,
    pub state: String,
    pub available: bool,
    pub size_bytes: u64,
}

impl SimDevice {
    pub fn booted(&self) -> bool {
        self.state == "Booted"
    }
}

#[derive(Deserialize)]
struct RawDevice {
    udid: String,
    name: String,
    state: String,
    #[serde(rename = "isAvailable", default)]
    is_available: bool,
    #[serde(rename = "dataPathSize", default)]
    data_path_size: u64,
}

#[derive(Deserialize)]
struct RawDevices {
    devices: BTreeMap<String, Vec<RawDevice>>,
}

pub fn parse_devices(json: &str) -> Result<Vec<SimDevice>> {
    let raw: RawDevices = serde_json::from_str(json).context("unreadable `simctl list devices -j` output")?;
    Ok(raw
        .devices
        .into_values()
        .flatten()
        .map(|device| SimDevice { udid: device.udid, name: device.name, state: device.state, available: device.is_available, size_bytes: device.data_path_size })
        .collect())
}

pub fn list_devices() -> Result<Vec<SimDevice>> {
    parse_devices(&simctl(&["list", "devices", "-j"], QUICK)?)
}

#[derive(Deserialize, Debug, Clone)]
pub struct Runtime {
    pub identifier: String,
    pub version: String,
    pub name: String,
    #[serde(rename = "isAvailable", default)]
    pub available: bool,
}

#[derive(Deserialize)]
struct RawRuntimes {
    runtimes: Vec<Runtime>,
}

pub fn list_runtimes() -> Result<Vec<Runtime>> {
    let raw: RawRuntimes = serde_json::from_str(&simctl(&["list", "runtimes", "-j"], QUICK)?)
        .context("unreadable `simctl list runtimes -j` output")?;
    Ok(raw.runtimes.into_iter().filter(|runtime| runtime.available && runtime.name.starts_with("iOS")).collect())
}

#[derive(Deserialize, Debug, Clone)]
pub struct DeviceType {
    pub name: String,
    pub identifier: String,
}

#[derive(Deserialize)]
struct RawDeviceTypes {
    devicetypes: Vec<DeviceType>,
}

pub fn list_device_types() -> Result<Vec<DeviceType>> {
    let raw: RawDeviceTypes = serde_json::from_str(&simctl(&["list", "devicetypes", "-j"], QUICK)?)
        .context("unreadable `simctl list devicetypes -j` output")?;
    Ok(raw.devicetypes)
}

pub fn version_key(version: &str) -> Vec<u32> {
    version.split('.').map(|part| part.parse().unwrap_or(0)).collect()
}

pub fn resolve_runtime(wanted: Option<&str>) -> Result<Runtime> {
    let mut runtimes = list_runtimes()?;
    runtimes.sort_by_key(|runtime| version_key(&runtime.version));
    let Some(wanted) = wanted else {
        return runtimes.pop().context("no iOS simulator runtime is installed: Xcode > Settings > Components");
    };
    let installed = runtimes.iter().map(|runtime| runtime.version.as_str()).collect::<Vec<_>>().join(", ");
    runtimes
        .iter()
        .rev()
        .find(|runtime| runtime.identifier == wanted || runtime.version == wanted || runtime.version.starts_with(&format!("{wanted}.")))
        .cloned()
        .with_context(|| format!("no installed iOS runtime matches `{wanted}` (installed: {installed})"))
}

pub fn resolve_device_type(wanted: &str) -> Result<DeviceType> {
    let types = list_device_types()?;
    if let Some(found) = types.iter().find(|kind| kind.identifier == wanted || kind.name.eq_ignore_ascii_case(wanted)) {
        return Ok(found.clone());
    }
    let phones = types.iter().filter(|kind| kind.name.starts_with("iPhone")).map(|kind| kind.name.as_str()).rev().take(8).collect::<Vec<_>>().join(", ");
    bail!("no device type is called `{wanted}`. Recent iPhones: {phones}")
}

pub fn shutdown(udid: &str) -> Result<()> {
    match simctl(&["shutdown", udid], 60) {
        Ok(_) => Ok(()),
        Err(error) if error.to_string().contains("current state: Shutdown") => Ok(()),
        Err(error) => Err(error),
    }
}

pub fn boot_and_wait(udid: &str) -> Result<()> {
    simctl(&["bootstatus", udid, "-b"], SLOW).map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_device_list() {
        let json = r#"{"devices":{"com.apple.CoreSimulator.SimRuntime.iOS-26-5":[
            {"udid":"A","name":"simpool-1","state":"Booted","isAvailable":true,"dataPathSize":1024},
            {"udid":"B","name":"iPhone 17","state":"Shutdown","isAvailable":false}]}}"#;
        let devices = parse_devices(json).unwrap();
        assert_eq!(devices.len(), 2);
        assert!(devices[0].booted());
        assert_eq!(devices[0].size_bytes, 1024);
        assert!(!devices[1].available);
    }

    #[test]
    fn orders_versions_numerically() {
        assert!(version_key("26.10") > version_key("26.5"));
        assert!(version_key("27.0") > version_key("26.10.1"));
    }

    #[test]
    fn run_times_out() {
        let error = run("sleep", &["5"], 1).unwrap_err().to_string();
        assert!(error.contains("did not finish within 1s"), "{error}");
    }

    #[test]
    fn run_reports_stderr() {
        let error = run("ls", &["/definitely/not/here"], 5).unwrap_err().to_string();
        assert!(error.contains("No such file"), "{error}");
    }
}
