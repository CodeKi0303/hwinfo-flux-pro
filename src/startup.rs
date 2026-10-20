//! Current-user logon registration for the selected EXE and configuration.
//! Reads never register or repair anything. A moved EXE's old registration is
//! considered different and is preserved for explicit user cleanup.

use serde::Serialize;
use std::{
    borrow::Cow,
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};
use winreg::{
    RegKey, RegValue,
    enums::{HKEY_CURRENT_USER, KEY_QUERY_VALUE, KEY_SET_VALUE, KEY_WOW64_64KEY, REG_SZ},
};

const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const VALUE_NAME: &str = "hwinfo-flux-pro";
// Microsoft documents a maximum 260-character Run command line. Count UTF-16
// code units, conservatively including both halves of supplementary characters.
// https://learn.microsoft.com/en-us/windows/win32/setupapi/run-and-runonce-registry-keys
const MAX_COMMAND_UNITS: usize = 260;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Disabled,
    Enabled,
    OtherRegistration,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct RawValue {
    value_type: u32,
    bytes: Vec<u8>,
}

impl RawValue {
    fn command(command: &str) -> Self {
        Self {
            value_type: REG_SZ as u32,
            bytes: command
                .encode_utf16()
                .chain(std::iter::once(0))
                .flat_map(u16::to_le_bytes)
                .collect(),
        }
    }

    fn description(&self) -> String {
        if matches!(self.value_type, 1 | 2) && self.bytes.len().is_multiple_of(2) {
            let mut units: Vec<_> = self
                .bytes
                .chunks_exact(2)
                .map(|bytes| u16::from_le_bytes([bytes[0], bytes[1]]))
                .collect();
            if units.last() == Some(&0) {
                units.pop();
            }
            if !units.contains(&0)
                && let Ok(command) = String::from_utf16(&units)
            {
                return command;
            }
        }
        format!(
            "Existing registry value has type {} and {} bytes; it is not this exact registration.",
            self.value_type,
            self.bytes.len()
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    pub enabled: bool,
    pub status: Status,
    pub other_registration: Option<String>,
    raw: Option<RawValue>,
}

impl Snapshot {
    fn from_raw(raw: Option<RawValue>, owned: &RawValue) -> Self {
        let status = match raw.as_ref() {
            None => Status::Disabled,
            Some(current) if current == owned => Status::Enabled,
            Some(_) => Status::OtherRegistration,
        };
        let other_registration = if status == Status::OtherRegistration {
            raw.as_ref().map(RawValue::description)
        } else {
            None
        };
        Self {
            enabled: status == Status::Enabled,
            status,
            other_registration,
            raw,
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Change {
    None,
    Enable,
    Disable,
}

fn planned_change(
    expected: Option<&RawValue>,
    current: Option<&RawValue>,
    owned: &RawValue,
    enabled: bool,
) -> Result<Change, String> {
    if current != expected {
        return Err("Startup registration changed since it was read. Refresh before changing it; nothing was overwritten.".into());
    }
    match current {
        None => Ok(if enabled { Change::Enable } else { Change::None }),
        Some(current) if current == owned => {
            Ok(if enabled { Change::None } else { Change::Disable })
        }
        Some(_) => Err("A different startup command already uses the hwinfo-flux-pro value. It was preserved. If the EXE or config was moved, remove the old registration explicitly through Windows/Registry Editor before enabling this location.".into()),
    }
}

pub struct Controller {
    command: String,
    owned: RawValue,
    state_dir: PathBuf,
}

impl Controller {
    pub fn new(config_path: &Path) -> Result<Self, String> {
        let executable = std::env::current_exe().map_err(|error| error.to_string())?;
        let config_path = std::path::absolute(config_path).map_err(|error| error.to_string())?;
        let command = startup_command(&executable, &config_path)?;
        Ok(Self {
            owned: RawValue::command(&command),
            command,
            state_dir: crate::config::state_dir()?,
        })
    }

    pub fn read(&self) -> Result<Snapshot, String> {
        let key = open_run(KEY_QUERY_VALUE)?;
        Ok(Snapshot::from_raw(read_raw(key.as_ref())?, &self.owned))
    }

    /// The exact observed type/bytes are checked before mutation, including a
    /// recheck after backup. The lock serializes this app's writers. An external
    /// writer ignoring it can still race the final check; this is not a registry
    /// transaction or global compare-and-swap.
    pub fn set_enabled(&self, expected: &Snapshot, enabled: bool) -> Result<Snapshot, String> {
        let observed = self.read()?;
        if planned_change(
            expected.raw.as_ref(),
            observed.raw.as_ref(),
            &self.owned,
            enabled,
        )? == Change::None
        {
            return Ok(observed);
        }
        fs::create_dir_all(&self.state_dir).map_err(|error| error.to_string())?;
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(self.state_dir.join("startup.write.lock"))
            .map_err(|error| format!("Cannot open startup-settings lock: {error}"))?;
        lock.try_lock()
            .map_err(|error| format!("Another startup-settings change may be active: {error}"))?;
        let key = match open_run(KEY_QUERY_VALUE | KEY_SET_VALUE)? {
            Some(key) => key,
            None => {
                RegKey::predef(HKEY_CURRENT_USER)
                    .create_subkey_with_flags(
                        RUN_KEY,
                        KEY_QUERY_VALUE | KEY_SET_VALUE | KEY_WOW64_64KEY,
                    )
                    .map_err(|error| format!("Cannot open current-user startup key: {error}"))?
                    .0
            }
        };
        let current = read_raw(Some(&key))?;
        let change = planned_change(
            expected.raw.as_ref(),
            current.as_ref(),
            &self.owned,
            enabled,
        )?;
        if change == Change::None {
            return Ok(Snapshot::from_raw(current, &self.owned));
        }
        if let Some(current) = &current {
            self.backup(current)?;
        }
        let latest = read_raw(Some(&key))?;
        planned_change(current.as_ref(), latest.as_ref(), &self.owned, enabled)?;
        match change {
            Change::Enable => key.set_raw_value(
                VALUE_NAME,
                &RegValue {
                    bytes: Cow::Borrowed(&self.owned.bytes),
                    vtype: REG_SZ,
                },
            ),
            Change::Disable => key.delete_value(VALUE_NAME),
            Change::None => unreachable!(),
        }
        .map_err(|error| format!("Cannot update the current-user startup registration: {error}"))?;
        let saved = self.read()?;
        if saved.status == Status::OtherRegistration || saved.enabled != enabled {
            return Err("Startup registration changed immediately after saving. Refresh to inspect its current value.".into());
        }
        Ok(saved)
    }

    fn backup(&self, value: &RawValue) -> Result<(), String> {
        static SEQUENCE: AtomicU64 = AtomicU64::new(0);
        let directory = self.state_dir.join("startup-backups");
        fs::create_dir_all(&directory).map_err(|error| error.to_string())?;
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        let path = directory.join(format!(
            "startup-{}-{}-{}.json",
            timestamp.as_nanos(),
            std::process::id(),
            SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        let backup = serde_json::json!({
            "schema_version": 1, "created_unix_ms": timestamp.as_millis(),
            "hive": "HKEY_CURRENT_USER", "key": RUN_KEY, "value_name": VALUE_NAME,
            "command": self.command, "raw_value": value
        });
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(|error| format!("Cannot create startup backup: {error}"))?;
        let result = (|| {
            serde_json::to_writer_pretty(&mut file, &backup).map_err(|error| error.to_string())?;
            file.write_all(b"\n")
                .and_then(|()| file.sync_all())
                .map_err(|error| error.to_string())
        })();
        drop(file);
        if let Err(error) = result {
            let cleanup = fs::remove_file(&path).err();
            return Err(format!(
                "Startup backup failed; registration was preserved: {error}; partial-backup cleanup error: {cleanup:?}"
            ));
        }
        Ok(())
    }
}

fn open_run(access: u32) -> Result<Option<RegKey>, String> {
    match RegKey::predef(HKEY_CURRENT_USER)
        .open_subkey_with_flags(RUN_KEY, access | KEY_WOW64_64KEY)
    {
        Ok(key) => Ok(Some(key)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!(
            "Cannot read current-user startup settings: {error}"
        )),
    }
}

fn read_raw(key: Option<&RegKey>) -> Result<Option<RawValue>, String> {
    let Some(key) = key else { return Ok(None) };
    match key.get_raw_value(VALUE_NAME) {
        Ok(value) if value.bytes.len() <= 65_536 => Ok(Some(RawValue {
            value_type: value.vtype as u32,
            bytes: value.bytes.into_owned(),
        })),
        Ok(_) => Err(
            "The existing startup value exceeds the 64 KiB read budget; it was preserved.".into(),
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!("Cannot read the startup registration: {error}")),
    }
}

fn startup_command(executable: &Path, config: &Path) -> Result<String, String> {
    if !executable.is_absolute() || !config.is_absolute() {
        return Err("Startup EXE and config paths must be absolute.".into());
    }
    let executable = executable
        .to_str()
        .ok_or("Startup EXE path is not valid Unicode; no lossy conversion is allowed.")?;
    let config = config
        .to_str()
        .ok_or("Startup config path is not valid Unicode; no lossy conversion is allowed.")?;
    if executable.contains('\0') || config.contains('\0') {
        return Err("Startup paths must not contain NUL characters.".into());
    }
    let command = format!(
        "{} tray --background --config {}",
        quote_argument(executable),
        quote_argument(config)
    );
    if command.encode_utf16().count() > MAX_COMMAND_UNITS {
        return Err("Windows Run commands must fit within 260 UTF-16 code units. Use shorter EXE/config paths before enabling startup.".into());
    }
    Ok(command)
}

fn quote_argument(argument: &str) -> String {
    let mut quoted = String::with_capacity(argument.len() + 2);
    quoted.push('"');
    let mut slashes = 0;
    for character in argument.chars() {
        if character == '\\' {
            slashes += 1;
            continue;
        }
        let escaped_slashes = if character == '"' {
            slashes * 2 + 1
        } else {
            slashes
        };
        quoted.extend(std::iter::repeat_n('\\', escaped_slashes));
        quoted.push(character);
        slashes = 0;
    }
    quoted.extend(std::iter::repeat_n('\\', slashes * 2));
    quoted.push('"');
    quoted
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quotes_unicode_paths_and_starts_background_tray_without_settings() {
        let command = startup_command(
            Path::new(r"C:\앱 폴더\flux.exe"),
            Path::new(r"C:\내 설정\config.json"),
        )
        .unwrap();
        assert_eq!(
            command,
            "\"C:\\앱 폴더\\flux.exe\" tray --background --config \"C:\\내 설정\\config.json\""
        );
        assert!(!command.contains("--settings"));
        assert_eq!(quote_argument("C:\\folder\\"), "\"C:\\folder\\\\\"");
        assert_eq!(quote_argument("a\\\"b"), "\"a\\\\\\\"b\"");
    }

    #[test]
    fn command_length_counts_utf16_and_rejects_relative_or_invalid_unicode_paths() {
        let exe = Path::new(r"C:\a.exe");
        let base = startup_command(exe, Path::new(r"C:\c")).unwrap();
        let extra = MAX_COMMAND_UNITS - base.encode_utf16().count();
        let exact = format!("C:\\c{}", "x".repeat(extra));
        assert_eq!(
            startup_command(exe, Path::new(&exact))
                .unwrap()
                .encode_utf16()
                .count(),
            MAX_COMMAND_UNITS
        );
        assert!(startup_command(exe, Path::new(&(exact.clone() + "x"))).is_err());
        let supplementary = format!("C:\\c{}😀", "x".repeat(extra - 1));
        assert!(startup_command(exe, Path::new(&supplementary)).is_err());
        assert!(startup_command(exe, Path::new("config.json")).is_err());
        use std::os::windows::ffi::OsStringExt;
        let invalid =
            std::ffi::OsString::from_wide(&[b'C' as u16, b':' as u16, b'\\' as u16, 0xd800]);
        assert!(startup_command(exe, Path::new(&invalid)).is_err());
    }

    #[test]
    fn exact_owned_transitions_are_idempotent_and_foreign_values_stay_untouched() {
        let owned =
            RawValue::command("\"C:\\current.exe\" tray --background --config \"C:\\config.json\"");
        assert_eq!(
            planned_change(None, None, &owned, false).unwrap(),
            Change::None
        );
        assert_eq!(
            planned_change(None, None, &owned, true).unwrap(),
            Change::Enable
        );
        assert_eq!(
            planned_change(Some(&owned), Some(&owned), &owned, true).unwrap(),
            Change::None
        );
        assert_eq!(
            planned_change(Some(&owned), Some(&owned), &owned, false).unwrap(),
            Change::Disable
        );
        let old_path =
            RawValue::command("\"C:\\old.exe\" tray --background --config \"C:\\config.json\"");
        let mut different_type = owned.clone();
        different_type.value_type = 2;
        let mut different_bytes = owned.clone();
        different_bytes.bytes.extend_from_slice(&[0, 0]);
        for foreign in [old_path, different_type, different_bytes] {
            let snapshot = Snapshot::from_raw(Some(foreign.clone()), &owned);
            assert_eq!(snapshot.status, Status::OtherRegistration);
            assert!(!snapshot.enabled);
            assert!(snapshot.other_registration.is_some());
            for enabled in [true, false] {
                assert!(planned_change(Some(&foreign), Some(&foreign), &owned, enabled).is_err());
            }
        }
    }

    #[test]
    fn stale_snapshot_cannot_authorize_creation_or_deletion() {
        let owned = RawValue::command("current command");
        let changed = RawValue::command("changed command");
        for enabled in [false, true] {
            assert!(
                planned_change(None, Some(&owned), &owned, enabled)
                    .unwrap_err()
                    .contains("changed since")
            );
            assert!(
                planned_change(Some(&owned), None, &owned, enabled)
                    .unwrap_err()
                    .contains("changed since")
            );
            assert!(
                planned_change(Some(&owned), Some(&changed), &owned, enabled)
                    .unwrap_err()
                    .contains("changed since")
            );
        }
    }
}
