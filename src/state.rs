use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::simctl::SimDevice;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Lease {
    pub owner: String,
    pub owner_path: String,
    pub leased_at: u64,
    pub expires_at: u64,
    pub ttl_secs: u64,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Device {
    pub udid: String,
    pub name: String,
    pub device_type: String,
    pub runtime: String,
    pub created_at: u64,
    pub last_used_at: u64,
    pub lease: Option<Lease>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Template {
    pub udid: String,
    pub name: String,
    pub device_type: String,
    pub runtime: String,
}

#[derive(Serialize, Deserialize, Default, Debug)]
pub struct State {
    #[serde(default)]
    pub devices: Vec<Device>,
    #[serde(default)]
    pub templates: Vec<Template>,
}

impl State {
    /// Forgets devices that no longer exist in CoreSimulator.
    pub fn sync(&mut self, sims: &[SimDevice]) {
        let exists = |udid: &str| sims.iter().any(|sim| sim.udid == udid);
        self.devices.retain(|device| exists(&device.udid));
        self.templates.retain(|template| exists(&template.udid));
    }

    pub fn leased_by(&mut self, owner: &str) -> Option<&mut Device> {
        self.devices.iter_mut().find(|device| device.lease.as_ref().is_some_and(|lease| lease.owner == owner))
    }
}

pub fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|elapsed| elapsed.as_secs()).unwrap_or(0)
}

pub struct Store {
    dir: PathBuf,
}

impl Store {
    pub fn open() -> Result<Self> {
        let dir = match std::env::var_os("SIMPOOL_HOME") {
            Some(home) => PathBuf::from(home),
            None => PathBuf::from(std::env::var_os("HOME").context("HOME is not set")?).join("Library/Application Support/simpool"),
        };
        fs::create_dir_all(&dir).with_context(|| format!("could not create {}", dir.display()))?;
        Ok(Self { dir })
    }

    /// Takes an exclusive, machine-wide lock that is released when the file drops.
    pub fn lock(&self, name: &str) -> Result<File> {
        let path = self.dir.join(format!("{name}.lock"));
        let file = OpenOptions::new().create(true).truncate(false).write(true).open(&path)?;
        file.lock().with_context(|| format!("could not lock {}", path.display()))?;
        Ok(file)
    }

    pub fn read(&self) -> Result<State> {
        let path = self.dir.join("state.json");
        match fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str(&text).with_context(|| format!("{} is corrupt; delete it and run `simpool gc`", path.display())),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(State::default()),
            Err(error) => Err(error.into()),
        }
    }

    /// Reads, changes and writes the state under the state lock.
    pub fn with<T>(&self, change: impl FnOnce(&mut State) -> Result<T>) -> Result<T> {
        let _guard = self.lock("state")?;
        let mut state = self.read()?;
        let result = change(&mut state)?;
        let temporary = self.dir.join(format!("state.json.{}", std::process::id()));
        fs::write(&temporary, serde_json::to_vec_pretty(&state)?)?;
        fs::rename(&temporary, self.dir.join("state.json"))?;
        Ok(result)
    }
}
