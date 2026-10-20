//! Read HWiNFO's user-enabled Gadget export; never infer liveness from a value.
//!
//! Public format: https://docs.rainmeter.net/tips/hwinfo/
//! These registry values have no transaction, sequence number or sample timestamp.
//! Two matching reads reduce mixed-write risk but cannot prove sample freshness.

use serde::{Deserialize, Serialize};

// A deliberate memory/work budget for this two-temperature consumer. HWiNFO users
// should export only needed sensors. Exceeding this budget is an explicit error.
const MAX_REGISTRY_VALUES: usize = 4096;
const MAX_VALUE_BYTES: usize = 2048;
const MAX_VALUE_NAME_CHARS: usize = 64;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RegistryHive {
    CurrentUser,
    LocalMachine,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum HwinfoProduct {
    Hwinfo64,
    Hwinfo32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RegistrySource {
    pub hive: RegistryHive,
    pub product: HwinfoProduct,
}

impl RegistrySource {
    pub fn path(&self) -> &'static str {
        match self.product {
            HwinfoProduct::Hwinfo64 => r"SOFTWARE\HWiNFO64\VSB",
            HwinfoProduct::Hwinfo32 => r"SOFTWARE\HWiNFO32\VSB",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SensorSelector {
    pub sensor: String,
    pub label: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SensorReading {
    pub sensor: String,
    pub label: String,
    pub index: String,
    pub value: String,
    pub value_raw: String,
    pub celsius: Option<f64>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Tuple {
    sensor: Option<String>,
    label: Option<String>,
    value: Option<String>,
    value_raw: Option<String>,
}

impl Tuple {
    fn temperature(&self) -> Result<f64, String> {
        required(&self.sensor, "Sensor")?;
        required(&self.label, "Label")?;
        parse_temperature(
            required(&self.value, "Value")?,
            required(&self.value_raw, "ValueRaw")?,
        )
    }
}

fn required<'a>(value: &'a Option<String>, name: &str) -> Result<&'a str, String> {
    value
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| format!("missing or empty {name} field"))
}

trait Export {
    fn indices(&self) -> Result<Vec<String>, String>;
    fn tuple(&self, index: &str) -> Result<Tuple, String>;
}

/// Select only an exact, unique Sensor + Label from a validated snapshot.
/// Call list_sensors again for every poll; indices are not stable identities.
pub fn select_reading(
    readings: &[SensorReading],
    selector: &SensorSelector,
) -> Result<f64, String> {
    if selector.sensor.trim().is_empty() || selector.label.trim().is_empty() {
        return Err("sensor selector must contain a nonempty Sensor and Label".into());
    }
    let mut found = None;
    for reading in readings {
        if reading.sensor == selector.sensor && reading.label == selector.label {
            if found.is_some() {
                return Err(
                    "ambiguous export: more than one entry has this Sensor and Label".into(),
                );
            }
            found = Some(reading);
        }
    }
    let reading = found.ok_or_else(|| {
        "selected Sensor and Label were not found in the pinned registry source".to_string()
    })?;
    if let Some(error) = &reading.error {
        return Err(error.clone());
    }
    reading
        .celsius
        .filter(|value| value.is_finite())
        .ok_or_else(|| "selected sensor has no valid temperature".into())
}

#[cfg(test)]
fn read_selected_from(export: &impl Export, selector: &SensorSelector) -> Result<f64, String> {
    select_reading(&list_from(export)?, selector)
}

fn list_from(export: &impl Export) -> Result<Vec<SensorReading>, String> {
    let indices = export.indices()?;
    let mut first = Vec::with_capacity(indices.len());
    for index in indices {
        let tuple = export.tuple(&index)?;
        first.push((index, tuple));
    }
    let second_indices = export.indices()?;
    if !first.iter().map(|row| &row.0).eq(second_indices.iter()) {
        return Err("HWiNFO export indices changed during reading; wait for the next poll".into());
    }
    drop(second_indices);
    let mut readings = Vec::with_capacity(first.len());
    for (index, first) in first {
        let second = export.tuple(&index)?;
        if first != second {
            return Err("HWiNFO export changed during reading; wait for the next poll".into());
        }
        let (celsius, error) = match second.temperature() {
            Ok(value) => (Some(value), None),
            Err(error) => (None, Some(error)),
        };
        readings.push(SensorReading {
            sensor: second.sensor.unwrap_or_default(),
            label: second.label.unwrap_or_default(),
            index,
            value: second.value.unwrap_or_default(),
            value_raw: second.value_raw.unwrap_or_default(),
            celsius,
            error,
        });
    }
    // Discovery also exposes ambiguity. Sorting in place avoids a second map
    // allocation and keeps the work bounded to O(n log n) for large exports.
    readings.sort_unstable_by(|left, right| {
        left.sensor
            .cmp(&right.sensor)
            .then_with(|| left.label.cmp(&right.label))
            .then_with(|| left.index.cmp(&right.index))
    });
    for i in 1..readings.len() {
        if readings[i - 1].sensor == readings[i].sensor
            && readings[i - 1].label == readings[i].label
        {
            for index in [i - 1, i] {
                readings[index].celsius = None;
                readings[index].error = Some("ambiguous duplicate Sensor and Label".into());
            }
        }
    }
    Ok(readings)
}

/// Read one explicitly selected registry source. Never merge or switch sources.
pub fn list_sensors(source: &RegistrySource) -> Result<Vec<SensorReading>, String> {
    list_from(&RegistryExport::open(source)?)
}

fn parse_temperature(formatted: &str, raw: &str) -> Result<f64, String> {
    let formatted = formatted.trim();
    let (number, fahrenheit) = if let Some(number) = formatted.strip_suffix("°C") {
        (number, false)
    } else if let Some(number) = formatted.strip_suffix('℃') {
        (number, false)
    } else if let Some(number) = formatted.strip_suffix("°F") {
        (number, true)
    } else if let Some(number) = formatted.strip_suffix('℉') {
        (number, true)
    } else {
        return Err("Value must explicitly have a supported °C/℃ or °F/℉ unit".into());
    };
    let (displayed, decimals) = decimal(number.trim())?;
    // Three-digit groups are ambiguous with the documented thousands formatting.
    // Reject that uncommon temperature format instead of silently reinterpreting it.
    if decimals > 2 {
        return Err(
            "Value temperature format supports at most two decimal places and no grouping".into(),
        );
    }
    let (raw, _) = decimal(raw.trim())?;
    let tolerance = 0.5 * 10_f64.powi(-(decimals as i32));
    if (displayed - raw).abs() > tolerance + 1e-9 {
        return Err("Value and ValueRaw disagree beyond the displayed rounding precision".into());
    }
    let celsius = if fahrenheit { (raw - 32.0) / 1.8 } else { raw };
    if !celsius.is_finite() {
        return Err("temperature is not finite".into());
    }
    Ok(celsius)
}

// ValueRaw is documented as unformatted, but decimal commas occur on localized
// installations. Accept only an unambiguous decimal grammar, without allocation.
fn decimal(text: &str) -> Result<(f64, usize), String> {
    let input = text.as_bytes();
    if input.is_empty() || input.len() > 64 {
        return Err("temperature number is empty or exceeds 64 bytes".into());
    }
    let mut normalized = [0_u8; 64];
    let mut seen_separator = false;
    let mut digits = 0;
    let mut decimals = 0;
    for (position, byte) in input.iter().copied().enumerate() {
        normalized[position] = match byte {
            b'0'..=b'9' => {
                digits += 1;
                if seen_separator {
                    decimals += 1;
                }
                byte
            }
            b'+' | b'-' if position == 0 => byte,
            b'.' | b',' if !seen_separator => {
                seen_separator = true;
                b'.'
            }
            _ => return Err("unsupported temperature number: use an ungrouped decimal".into()),
        };
    }
    if digits == 0 || (seen_separator && decimals == 0) {
        return Err("temperature number is incomplete".into());
    }
    let normalized =
        std::str::from_utf8(&normalized[..input.len()]).map_err(|error| error.to_string())?;
    let number = normalized
        .parse::<f64>()
        .map_err(|error| error.to_string())?;
    if !number.is_finite() {
        return Err("temperature number is not finite".into());
    }
    Ok((number, decimals))
}

#[cfg(windows)]
struct RegistryExport(winreg::RegKey);

#[cfg(windows)]
impl RegistryExport {
    fn open(source: &RegistrySource) -> Result<Self, String> {
        use winreg::enums::{
            HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, KEY_QUERY_VALUE, KEY_WOW64_64KEY,
        };
        let hive = match source.hive {
            RegistryHive::CurrentUser => HKEY_CURRENT_USER,
            RegistryHive::LocalMachine => HKEY_LOCAL_MACHINE,
        };
        // Releases target x64 Windows. Pin its native registry view explicitly.
        let key = winreg::RegKey::predef(hive)
            .open_subkey_with_flags(source.path(), KEY_QUERY_VALUE | KEY_WOW64_64KEY)
            .map_err(|error| {
                format!("cannot open {:?}\\{}: {error}", source.hive, source.path())
            })?;
        let export = Self(key);
        export.check_budget()?;
        Ok(export)
    }

    fn check_budget(&self) -> Result<(), String> {
        let metadata = self.0.query_info().map_err(|error| error.to_string())?;
        if metadata.values as usize > MAX_REGISTRY_VALUES
            || metadata.max_value_len as usize > MAX_VALUE_BYTES
            || metadata.max_value_name_len as usize > MAX_VALUE_NAME_CHARS
        {
            return Err("Gadget export exceeds the reader budget (4096 values, 2048 bytes/value, 64 name characters)".into());
        }
        Ok(())
    }

    fn field(&self, prefix: &str, index: &str) -> Result<Option<String>, String> {
        let name = format!("{prefix}{index}");
        let value = match self.0.get_raw_value(&name) {
            Ok(value) => value,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(format!("cannot read {name}: {error}")),
        };
        if value.vtype != winreg::enums::REG_SZ || value.bytes.len() > MAX_VALUE_BYTES {
            return Err(format!(
                "{name} must be a REG_SZ string of at most {MAX_VALUE_BYTES} bytes"
            ));
        }
        if value.bytes.len() % 2 != 0 {
            return Err(format!("{name} contains malformed UTF-16"));
        }
        let mut text = String::with_capacity(value.bytes.len() / 2);
        let mut terminated = false;
        for character in char::decode_utf16(
            value
                .bytes
                .chunks_exact(2)
                .map(|pair| u16::from_le_bytes([pair[0], pair[1]])),
        ) {
            let character = character.map_err(|_| format!("{name} contains malformed UTF-16"))?;
            if character == '\0' {
                terminated = true;
            } else if terminated {
                return Err(format!("{name} contains embedded NUL characters"));
            } else {
                text.push(character);
            }
        }
        Ok(Some(text))
    }
}

#[cfg(windows)]
impl Export for RegistryExport {
    fn indices(&self) -> Result<Vec<String>, String> {
        self.check_budget()?;
        let mut indices = Vec::new();
        for (count, entry) in self.0.enum_values().enumerate() {
            if count >= MAX_REGISTRY_VALUES {
                return Err("Gadget export grew beyond the reader value budget".into());
            }
            let (name, value) = entry.map_err(|error| error.to_string())?;
            if value.bytes.len() > MAX_VALUE_BYTES || name.len() > MAX_VALUE_NAME_CHARS {
                return Err("Gadget export grew beyond the reader string budget".into());
            }
            if let Some(index) = name.strip_prefix("Sensor")
                && !index.is_empty()
                && index.bytes().all(|byte| byte.is_ascii_digit())
            {
                indices.push(index.to_owned());
            }
        }
        indices.sort_unstable();
        Ok(indices)
    }

    fn tuple(&self, index: &str) -> Result<Tuple, String> {
        Ok(Tuple {
            sensor: self.field("Sensor", index)?,
            label: self.field("Label", index)?,
            value: self.field("Value", index)?,
            value_raw: self.field("ValueRaw", index)?,
        })
    }
}

#[cfg(not(windows))]
struct RegistryExport;

#[cfg(not(windows))]
impl RegistryExport {
    fn open(_source: &RegistrySource) -> Result<Self, String> {
        Err("HWiNFO Gadget registry access requires Windows".into())
    }
}

#[cfg(not(windows))]
impl Export for RegistryExport {
    fn indices(&self) -> Result<Vec<String>, String> {
        Err("HWiNFO Gadget registry access requires Windows".into())
    }

    fn tuple(&self, _index: &str) -> Result<Tuple, String> {
        Err("HWiNFO Gadget registry access requires Windows".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    fn tuple(sensor: &str, label: &str, value: &str, raw: &str) -> Tuple {
        Tuple {
            sensor: Some(sensor.into()),
            label: Some(label.into()),
            value: Some(value.into()),
            value_raw: Some(raw.into()),
        }
    }

    struct Fixture {
        snapshots: Vec<Vec<(String, Tuple)>>,
        scans: Cell<usize>,
    }

    impl Export for Fixture {
        fn indices(&self) -> Result<Vec<String>, String> {
            let index = self.scans.get().min(self.snapshots.len() - 1);
            self.scans.set(self.scans.get() + 1);
            Ok(self.snapshots[index]
                .iter()
                .map(|row| row.0.clone())
                .collect())
        }

        fn tuple(&self, index: &str) -> Result<Tuple, String> {
            let snapshot = self
                .scans
                .get()
                .saturating_sub(1)
                .min(self.snapshots.len() - 1);
            self.snapshots[snapshot]
                .iter()
                .find(|row| row.0 == index)
                .map(|row| row.1.clone())
                .ok_or_else(|| "fixture entry missing".into())
        }
    }

    fn selector() -> SensorSelector {
        SensorSelector {
            sensor: "CPU [#0]".into(),
            label: "CPU (Tctl/Tdie)".into(),
        }
    }

    #[test]
    fn accepts_localized_and_fahrenheit_temperatures_with_rounding() {
        assert_eq!(parse_temperature(" 52.6 °C ", "52.625").unwrap(), 52.625);
        assert_eq!(parse_temperature("52,6 ℃", "52,625").unwrap(), 52.625);
        assert!((parse_temperature("122.0 °F", "122").unwrap() - 50.0).abs() < 1e-9);
        assert!((parse_temperature("32 ℉", "32").unwrap()).abs() < 1e-9);
        assert_eq!(parse_temperature("-2.0 °C", "-2").unwrap(), -2.0);
    }

    #[test]
    fn rejects_untyped_nonfinite_grouped_and_disagreeing_values() {
        for (formatted, raw) in [
            ("55", "55"),
            ("55 RPM", "55"),
            ("NaN °C", "NaN"),
            ("inf °C", "inf"),
            ("50.0 °C", "70"),
            ("1,234 °C", "1234"),
            ("1,234 °C", "1,234"),
            ("50.0 °C", "50,0.0"),
            ("122.0 °F", "50"),
            ("50.0 °C", ""),
        ] {
            assert!(
                parse_temperature(formatted, raw).is_err(),
                "{formatted}: {raw}"
            );
        }
    }

    #[test]
    fn stable_reordering_is_followed_by_identity_not_previous_index() {
        let selected = selector();
        let cpu = tuple(&selected.sensor, &selected.label, "52.6 °C", "52.625");
        let gpu = tuple("GPU [#0]", "GPU Temperature", "40.0 °C", "40");
        let original = vec![("0".into(), cpu.clone()), ("1".into(), gpu.clone())];
        let reordered = vec![("0".into(), gpu), ("1".into(), cpu)];
        let fixture = Fixture {
            snapshots: vec![original.clone(), original, reordered],
            scans: Cell::new(0),
        };
        assert_eq!(read_selected_from(&fixture, &selected).unwrap(), 52.625);
        assert_eq!(read_selected_from(&fixture, &selected).unwrap(), 52.625);
    }

    #[test]
    fn rejects_reorder_or_value_change_between_reads() {
        let selected = selector();
        let cpu = tuple(&selected.sensor, &selected.label, "52.6 °C", "52.625");
        let moved = Fixture {
            snapshots: vec![
                vec![("0".into(), cpu.clone())],
                vec![("1".into(), cpu.clone())],
            ],
            scans: Cell::new(0),
        };
        assert!(
            read_selected_from(&moved, &selected)
                .unwrap_err()
                .contains("changed")
        );
        let changed = Fixture {
            snapshots: vec![
                vec![("0".into(), cpu)],
                vec![(
                    "0".into(),
                    tuple(&selected.sensor, &selected.label, "60.0 °C", "60"),
                )],
            ],
            scans: Cell::new(0),
        };
        assert!(
            read_selected_from(&changed, &selected)
                .unwrap_err()
                .contains("changed")
        );
    }

    #[test]
    fn rejects_duplicate_identity_and_missing_raw_value() {
        let selected = selector();
        let cpu = tuple(&selected.sensor, &selected.label, "52.6 °C", "52.625");
        let duplicates = Fixture {
            snapshots: vec![vec![("0".into(), cpu.clone()), ("1".into(), cpu.clone())]],
            scans: Cell::new(0),
        };
        assert!(
            read_selected_from(&duplicates, &selected)
                .unwrap_err()
                .contains("ambiguous")
        );
        assert!(
            list_from(&duplicates)
                .unwrap()
                .iter()
                .all(|row| row.celsius.is_none())
        );
        let mut missing = cpu;
        missing.value_raw = None;
        let fixture = Fixture {
            snapshots: vec![vec![("0".into(), missing)]],
            scans: Cell::new(0),
        };
        assert!(
            read_selected_from(&fixture, &selected)
                .unwrap_err()
                .contains("ValueRaw")
        );
        let row = list_from(&fixture).unwrap().remove(0);
        assert!(row.celsius.is_none());
        assert!(row.error.unwrap().contains("ValueRaw"));
    }
}
