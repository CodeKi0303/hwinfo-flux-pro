use crate::sensor::{RegistrySource, SensorSelector};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

pub const DEFAULT_DISPLAY_INTERVAL_MS: u64 = 1000;
pub const DEFAULT_SCAN_INTERVAL_MS: u64 = 2000;

fn default_display_interval_ms() -> u64 {
    DEFAULT_DISPLAY_INTERVAL_MS
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub schema_version: u32,
    pub registry: RegistrySource,
    pub cpu: SensorSelector,
    pub gpu: SensorSelector,
    pub interval_ms: u64,
    #[serde(default = "default_display_interval_ms")]
    pub display_interval_ms: u64,
    #[serde(default)]
    pub device_path: Option<String>,
}

impl Config {
    pub fn validate(&self) -> Result<(), String> {
        if self.schema_version != 1 {
            return Err("Unsupported configuration schema_version (expected 1).".into());
        }
        if !(1_000..=60_000).contains(&self.interval_ms) {
            return Err("interval_ms must be between 1000 and 60000; the default is 2000.".into());
        }
        if !(1_000..=60_000).contains(&self.display_interval_ms) {
            return Err(
                "display_interval_ms must be between 1000 and 60000; the default is 1000.".into(),
            );
        }
        for selector in [&self.cpu, &self.gpu] {
            if selector.sensor.trim().is_empty() || selector.label.trim().is_empty() {
                return Err("Select both an exact sensor group and label for CPU and GPU.".into());
            }
            if selector.sensor.len() > 2048 || selector.label.len() > 2048 {
                return Err("Sensor identity exceeds the 2 KiB field limit.".into());
            }
        }
        if self.cpu == self.gpu {
            return Err("CPU and GPU must select different sensor identities.".into());
        }
        if self
            .device_path
            .as_ref()
            .is_some_and(|p| p.is_empty() || p.len() > 32_768)
        {
            return Err("device_path must be a nonempty Windows device path.".into());
        }
        Ok(())
    }

    pub fn load(path: &Path) -> Result<Self, String> {
        load_document(path)?
            .map(|document| document.config)
            .ok_or_else(|| {
                format!(
                    "Configuration {} does not exist. Run `init` first.",
                    path.display()
                )
            })
    }

    pub fn save_new(&self, path: &Path) -> Result<(), String> {
        save_if_unchanged(path, None, self).map(|_| ())
    }
}

/// Exact file bytes, limited to 64 KiB; formatting changes alter the revision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigRevision {
    bytes: Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct ConfigDocument {
    pub config: Config,
    pub revision: ConfigRevision,
}

fn read_bytes(path: &Path) -> Result<Option<Vec<u8>>, String> {
    let file = match fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("Cannot read {}: {error}", path.display())),
    };
    if file.metadata().map_err(|error| error.to_string())?.len() > 65_536 {
        return Err("Configuration exceeds 64 KiB.".into());
    }
    let mut bytes = Vec::new();
    file.take(65_537)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("Cannot read {}: {error}", path.display()))?;
    if bytes.len() > 65_536 {
        return Err("Configuration exceeds 64 KiB.".into());
    }
    Ok(Some(bytes))
}

/// None means absent; invalid or unreadable files remain errors.
pub fn load_document(path: &Path) -> Result<Option<ConfigDocument>, String> {
    let Some(bytes) = read_bytes(path)? else {
        return Ok(None);
    };
    let config: Config = serde_json::from_slice(&bytes)
        .map_err(|error| format!("Invalid configuration {}: {error}", path.display()))?;
    config.validate()?;
    Ok(Some(ConfigDocument {
        config,
        revision: ConfigRevision { bytes },
    }))
}

fn sibling_path(path: &Path, suffix: &str) -> Result<PathBuf, String> {
    let mut name = path
        .file_name()
        .ok_or("Configuration path must name a file")?
        .to_os_string();
    name.push(suffix);
    Ok(path.with_file_name(name))
}

fn unique_sibling(path: &Path, extension: &str) -> Result<PathBuf, String> {
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);
    let time = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    sibling_path(
        path,
        &format!(
            ".{time}-{}-{}.{extension}",
            std::process::id(),
            SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ),
    )
}

fn write_new_bytes(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|error| format!("Cannot create {}: {error}", path.display()))?;
    let result = file.write_all(bytes).and_then(|()| file.sync_all());
    drop(file);
    if let Err(error) = result {
        let cleanup = fs::remove_file(path).err();
        return Err(format!(
            "Cannot write {}: {error}; partial-file cleanup error: {cleanup:?}",
            path.display()
        ));
    }
    Ok(())
}

/// None creates only; updates require the exact observed revision.
/// App writers share a lock. Existing bytes are backed up before publishing a
/// synced replacement atomically; external writers can still race the comparison.
pub fn save_if_unchanged(
    path: &Path,
    expected_revision: Option<&ConfigRevision>,
    config: &Config,
) -> Result<ConfigDocument, String> {
    config.validate()?;
    let mut bytes = serde_json::to_vec_pretty(config).map_err(|error| error.to_string())?;
    bytes.push(b'\n');
    if bytes.len() > 65_536 {
        return Err("Configuration exceeds 64 KiB.".into());
    }
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    let lock_path = sibling_path(path, ".write.lock")?;
    let lock = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
        .map_err(|error| format!("Cannot open configuration lock: {error}"))?;
    lock.try_lock()
        .map_err(|error| format!("Another configuration save may be active: {error}"))?;
    let previous = read_bytes(path)?;
    if previous.as_deref() != expected_revision.map(|revision| revision.bytes.as_slice()) {
        return Err("Configuration changed since it was loaded. Reload it before applying your selection; nothing was overwritten.".into());
    }
    if let Some(previous) = &previous {
        // Preserve existing bytes for an unchanged configuration.
        if serde_json::from_slice::<Config>(previous).is_ok_and(|current| current == *config) {
            return Ok(ConfigDocument {
                config: config.clone(),
                revision: ConfigRevision {
                    bytes: previous.clone(),
                },
            });
        }
        write_new_bytes(&unique_sibling(path, "bak")?, previous)?;
    }
    let temporary = unique_sibling(path, "tmp")?;
    write_new_bytes(&temporary, &bytes)?;
    let publish = (|| {
        if read_bytes(path)? != previous {
            return Err("Configuration changed during save. Reload it before applying your selection; nothing was overwritten.".into());
        }
        if previous.is_some() {
            fs::rename(&temporary, path)
                .map_err(|error| format!("Cannot replace configuration: {error}"))
        } else {
            // Publish only if the destination is still absent.
            fs::hard_link(&temporary, path).map_err(|error| {
                format!("Cannot create configuration without overwriting an existing file: {error}")
            })
        }
    })();
    if let Err(error) = fs::remove_file(&temporary)
        && error.kind() != std::io::ErrorKind::NotFound
    {
        return Err(format!(
            "Configuration publish result: {publish:?}; cannot remove temporary {}: {error}",
            temporary.display()
        ));
    }
    publish?;
    Ok(ConfigDocument {
        config: config.clone(),
        revision: ConfigRevision { bytes },
    })
}

pub fn state_dir() -> Result<PathBuf, String> {
    std::env::var_os("LOCALAPPDATA")
        .filter(|v| !v.is_empty())
        .map(|p| PathBuf::from(p).join("hwinfo-flux-pro"))
        .ok_or_else(|| "LOCALAPPDATA is not available; run in a Windows user session.".into())
}

pub fn default_path() -> Result<PathBuf, String> {
    Ok(state_dir()?.join("config.json"))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new() -> Self {
            let path = unique_sibling(&std::env::temp_dir().join("hwinfo-flux-config-test"), "dir")
                .unwrap();
            // Successful create_new ownership is required before Drop cleans up.
            fs::create_dir(&path).unwrap();
            Self(path)
        }

        fn config_path(&self) -> PathBuf {
            self.0.join("config.json")
        }

        fn backups(&self) -> Vec<PathBuf> {
            fs::read_dir(&self.0)
                .unwrap()
                .map(|entry| entry.unwrap().path())
                .filter(|path| path.extension().is_some_and(|extension| extension == "bak"))
                .collect()
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            if let (Ok(root), Ok(path)) =
                (std::env::temp_dir().canonicalize(), self.0.canonicalize())
                && path.parent() == Some(root.as_path())
                && path.file_name().is_some_and(|name| {
                    name.to_string_lossy()
                        .starts_with("hwinfo-flux-config-test.")
                })
            {
                let _ = fs::remove_dir_all(path);
            }
        }
    }

    fn example() -> Config {
        serde_json::from_str(include_str!("../config.example.json")).unwrap()
    }

    #[test]
    fn old_schema_one_preserves_scan_interval_and_defaults_display_to_one_second() {
        let mut old = serde_json::to_value(example()).unwrap();
        old.as_object_mut().unwrap().remove("display_interval_ms");
        for scan_ms in [1000, 2700, 60_000] {
            old["interval_ms"] = scan_ms.into();
            let loaded: Config = serde_json::from_value(old.clone()).unwrap();
            loaded.validate().unwrap();
            assert_eq!(loaded.schema_version, 1);
            assert_eq!(loaded.interval_ms, scan_ms);
            assert_eq!(loaded.display_interval_ms, DEFAULT_DISPLAY_INTERVAL_MS);
        }
        assert_eq!(example().interval_ms, DEFAULT_SCAN_INTERVAL_MS);
        assert_eq!(example().display_interval_ms, DEFAULT_DISPLAY_INTERVAL_MS);
    }

    #[test]
    fn validates_scan_and_display_interval_boundaries_independently() {
        for milliseconds in [1000, 60_000] {
            let mut config = example();
            config.interval_ms = milliseconds;
            config.validate().unwrap();
            config = example();
            config.display_interval_ms = milliseconds;
            config.validate().unwrap();
        }
        for milliseconds in [0, 999, 60_001, u64::MAX] {
            let mut config = example();
            config.interval_ms = milliseconds;
            assert!(config.validate().unwrap_err().contains("interval_ms"));
            config = example();
            config.display_interval_ms = milliseconds;
            assert!(
                config
                    .validate()
                    .unwrap_err()
                    .contains("display_interval_ms")
            );
        }
    }

    #[test]
    fn distinguishes_missing_invalid_and_oversized_documents() {
        let directory = TestDirectory::new();
        let path = directory.config_path();
        assert!(load_document(&path).unwrap().is_none());
        fs::write(&path, b"{").unwrap();
        assert!(
            load_document(&path)
                .unwrap_err()
                .contains("Invalid configuration")
        );
        fs::write(&path, vec![b' '; 65_537]).unwrap();
        assert!(load_document(&path).unwrap_err().contains("64 KiB"));
    }

    #[test]
    fn backs_up_exact_bytes_and_rejects_stale_gui_edits() {
        let directory = TestDirectory::new();
        let path = directory.config_path();
        let original = save_if_unchanged(&path, None, &example()).unwrap();
        let second_window = load_document(&path).unwrap().unwrap();
        let mut selected = original.config.clone();
        selected.registry.hive = crate::sensor::RegistryHive::LocalMachine;
        selected.cpu.sensor = "CPU [#0]: Exact / 이름 ".into();
        selected.cpu.label = "선택한 온도 ".into();
        let saved = save_if_unchanged(&path, Some(&original.revision), &selected).unwrap();
        assert_eq!(load_document(&path).unwrap().unwrap().config, selected);
        assert_eq!(directory.backups().len(), 1);
        assert_eq!(
            fs::read(&directory.backups()[0]).unwrap(),
            original.revision.bytes
        );
        assert!(
            save_if_unchanged(&path, Some(&second_window.revision), &second_window.config)
                .unwrap_err()
                .contains("changed")
        );
        assert_eq!(fs::read(&path).unwrap(), saved.revision.bytes);
        assert_eq!(directory.backups().len(), 1);
    }

    #[test]
    fn unchanged_apply_preserves_formatting_but_external_whitespace_is_a_conflict() {
        let directory = TestDirectory::new();
        let path = directory.config_path();
        let bytes = serde_json::to_vec(&example()).unwrap();
        fs::write(&path, &bytes).unwrap();
        let loaded = load_document(&path).unwrap().unwrap();
        let applied = save_if_unchanged(&path, Some(&loaded.revision), &loaded.config).unwrap();
        assert_eq!(applied.revision, loaded.revision);
        assert_eq!(fs::read(&path).unwrap(), bytes);
        assert!(directory.backups().is_empty());

        let mut externally_edited = bytes;
        externally_edited.push(b'\n');
        fs::write(&path, &externally_edited).unwrap();
        assert!(save_if_unchanged(&path, Some(&loaded.revision), &loaded.config).is_err());
        assert_eq!(fs::read(&path).unwrap(), externally_edited);
        assert!(directory.backups().is_empty());
    }

    #[test]
    fn first_time_and_locked_saves_do_not_clobber_another_writer() {
        let directory = TestDirectory::new();
        let path = directory.config_path();
        let original = save_if_unchanged(&path, None, &example()).unwrap();
        assert!(save_if_unchanged(&path, None, &example()).is_err());
        let lock = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(sibling_path(&path, ".write.lock").unwrap())
            .unwrap();
        lock.try_lock().unwrap();
        let mut changed = original.config.clone();
        changed.interval_ms = 3000;
        assert!(
            save_if_unchanged(&path, Some(&original.revision), &changed)
                .unwrap_err()
                .contains("save may be active")
        );
        assert_eq!(fs::read(&path).unwrap(), original.revision.bytes);
        assert!(directory.backups().is_empty());
    }

    #[test]
    fn rejects_unknown_fields_and_unsupported_schema() {
        let example = include_str!("../config.example.json");
        let mut value: Config = serde_json::from_str(example).unwrap();
        value.validate().unwrap();
        value.schema_version = 2;
        assert!(value.validate().is_err());
        value.schema_version = 1;
        value.gpu = value.cpu.clone();
        assert!(value.validate().is_err());
        let mut json: serde_json::Value = serde_json::from_str(example).unwrap();
        json["typo"] = true.into();
        assert!(serde_json::from_value::<Config>(json).is_err());
    }
}
