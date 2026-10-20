//! HID transport using the safe `hidapi` interface.
//!
//! Validates the report layout reconstructed by Windows HIDAPI.

use hidapi::{BusType, DeviceInfo, HidApi, HidDevice};
use serde::Serialize;

const VENDOR_ID: u16 = 0x2022;
const PRODUCT_ID: u16 = 0x0522;
const USAGE_PAGE: u16 = 0xff00;
const USAGE: u16 = 0x0001;
const PAYLOAD_BITS: u32 = 64 * 8;
const REPORT_BYTES: usize = 65;

#[derive(Debug, Serialize)]
pub struct DeviceSummary {
    pub path: String,
    pub vendor_id: u16,
    pub product_id: u16,
    pub usage_page: u16,
    pub usage: u16,
    pub interface_number: i32,
    pub manufacturer: Option<String>,
    pub product: Option<String>,
    pub bus_type: String,
    pub release_number: u16,
    pub supported: bool,
    pub reason: String,
    pub descriptor_bytes: Option<usize>,
    pub input_report_bytes: Option<usize>,
    pub output_report_bytes: Option<usize>,
    pub feature_report_bytes: Option<usize>,
}

impl DeviceSummary {
    fn from_info(info: &DeviceInfo) -> Self {
        Self {
            path: info.path().to_string_lossy().into_owned(),
            vendor_id: info.vendor_id(),
            product_id: info.product_id(),
            usage_page: info.usage_page(),
            usage: info.usage(),
            interface_number: info.interface_number(),
            manufacturer: info.manufacturer_string().map(str::to_owned),
            product: info.product_string().map(str::to_owned),
            bus_type: format!("{:?}", info.bus_type()),
            release_number: info.release_number(),
            supported: false,
            reason: "not validated".into(),
            descriptor_bytes: None,
            input_report_bytes: None,
            output_report_bytes: None,
            feature_report_bytes: None,
        }
    }
}

/// Enumerate only the display's VID/PID and validate each candidate. This does
/// not send output/feature reports or read input reports. HIDAPI opens a handle
/// with read/write access and configures the handle's Windows input queue.
pub fn list_devices() -> Result<Vec<DeviceSummary>, String> {
    let api = HidApi::new().map_err(|e| format!("HID enumeration failed: {e}"))?;
    Ok(api
        .device_list()
        .filter(|info| is_candidate(info))
        .map(|info| {
            let mut summary = DeviceSummary::from_info(info);
            match open_validated(&api, info, &mut summary) {
                Ok(device) => drop(device),
                Err(error) => summary.reason = error,
            }
            summary
        })
        .collect())
}

/// Validate and close a selected device without sending a display report.
pub fn probe(path: Option<&str>) -> Result<DeviceSummary, String> {
    let (_, summary) = open_selected(path)?;
    Ok(summary)
}

pub struct DisplayDevice {
    device: HidDevice,
}

impl DisplayDevice {
    pub fn open(path: Option<&str>) -> Result<Self, String> {
        let (device, _) = open_selected(path)?;
        Ok(Self { device })
    }

    /// Send exactly one complete, unnumbered report. The caller must discard
    /// this connection after an error and reopen/revalidate before retrying.
    pub fn write_report(&self, report: &[u8; REPORT_BYTES]) -> Result<(), String> {
        if report[0] != 0 {
            return Err("FLUX PRO requires the zero report-ID prefix".into());
        }
        let written = self
            .device
            .write(report)
            .map_err(|e| format!("FLUX PRO HID write failed: {e}"))?;
        validate_write_count(written)
    }
}

fn is_candidate(info: &DeviceInfo) -> bool {
    info.vendor_id() == VENDOR_ID && info.product_id() == PRODUCT_ID
}

fn open_selected(path: Option<&str>) -> Result<(HidDevice, DeviceSummary), String> {
    let api = HidApi::new().map_err(|e| format!("HID enumeration failed: {e}"))?;
    let mut candidates = api.device_list().filter(|info| {
        is_candidate(info) && path.is_none_or(|path| info.path().to_bytes() == path.as_bytes())
    });
    let selected = candidates.next().ok_or_else(|| match path {
        Some(path) => format!("No FLUX PRO HID candidate at the exact path {path:?}"),
        None => "No FLUX PRO HID candidate (2022:0522) is present".into(),
    })?;
    if candidates.next().is_some() {
        return Err("Multiple FLUX PRO HID candidates match; select one exact device path".into());
    }
    let mut summary = DeviceSummary::from_info(selected);
    let device = open_validated(&api, selected, &mut summary)?;
    Ok((device, summary))
}

fn check_identity(info: &DeviceInfo) -> Result<(), String> {
    if !is_candidate(info) || !matches!(info.bus_type(), BusType::Usb) {
        return Err("The device must be a USB controller with VID/PID 2022:0522".into());
    }
    if info.usage_page() != USAGE_PAGE || info.usage() != USAGE {
        return Err(format!(
            "Unsupported HID usage {:04x}:{:04x}; validated profile is ff00:0001",
            info.usage_page(),
            info.usage()
        ));
    }
    // HIDAPI uses -1 when a single-interface USB device has no MI_xx path.
    if !matches!(info.interface_number(), -1 | 0) {
        return Err(
            "Unsupported HID interface; expected interface 0 or a single-interface device".into(),
        );
    }
    info.path().to_str().map_err(|_| {
        "The device path is not valid UTF-8 and cannot be selected exactly".to_owned()
    })?;
    Ok(())
}

fn open_validated(
    api: &HidApi,
    selected: &DeviceInfo,
    summary: &mut DeviceSummary,
) -> Result<HidDevice, String> {
    check_identity(selected)?;
    let device = api
        .open_path(selected.path())
        .map_err(|e| format!("Opening FLUX PRO failed: {e}"))?;
    // Windows HIDAPI collects this information when opening this handle.
    let opened = device
        .get_device_info()
        .map_err(|e| format!("Cannot verify opened HID device: {e}"))?;
    check_identity(&opened)?;
    if opened.path() != selected.path()
        || opened.interface_number() != selected.interface_number()
        || opened.release_number() != selected.release_number()
        || opened.serial_number_raw() != selected.serial_number_raw()
        || opened.serial_number() != selected.serial_number()
    {
        return Err("HID identity changed between enumeration and opening; retry detection".into());
    }
    let mut descriptor = [0_u8; hidapi::MAX_REPORT_DESCRIPTOR_SIZE];
    let length = device
        .get_report_descriptor(&mut descriptor)
        .map_err(|e| format!("Cannot validate HID report layout: {e}"))?;
    summary.descriptor_bytes = Some(length);
    if length == 0 || length >= descriptor.len() {
        return Err("HID descriptor is empty or may have been truncated".into());
    }
    validate_descriptor(&descriptor[..length])?;
    summary.input_report_bytes = Some(REPORT_BYTES);
    summary.output_report_bytes = Some(REPORT_BYTES);
    summary.feature_report_bytes = Some(0);
    summary.supported = true;
    summary.reason = "Matches the observed ff00:0001 profile: unnumbered 64-byte input/output, no feature report; display behavior still requires a hardware output test".into();
    Ok(device)
}

fn validate_write_count(written: usize) -> Result<(), String> {
    if written != REPORT_BYTES {
        return Err(format!(
            "Incomplete HID output: wrote {written} of {REPORT_BYTES} bytes; no remainder was sent"
        ));
    }
    Ok(())
}

/// Interpret only the HID short-item subset needed by this controller profile.
/// Unsupported constructs are rejected, never guessed. The parser has no heap
/// allocation, and every report must fit the known 64-byte controller payload.
fn validate_descriptor(bytes: &[u8]) -> Result<(), String> {
    let mut offset = 0;
    let mut usage_page = 0_u32;
    let mut usage = None;
    let mut report_size = 0_u32;
    let mut report_count = 0_u32;
    let mut input_bits = 0_u32;
    let mut output_bits = 0_u32;
    let mut in_collection = false;
    let mut collection_count = 0;

    while let Some(&prefix) = bytes.get(offset) {
        offset += 1;
        if prefix == 0xfe {
            return Err("Unsupported HID long item in descriptor".into());
        }
        let length = match prefix & 3 {
            3 => 4,
            size => usize::from(size),
        };
        let data = bytes
            .get(offset..offset + length)
            .ok_or_else(|| "Truncated HID descriptor item".to_owned())?;
        offset += length;
        let mut value = 0_u32;
        for (byte, shift) in data.iter().zip([0, 8, 16, 24]) {
            value |= u32::from(*byte) << shift;
        }
        let item_type = (prefix >> 2) & 3;
        let tag = prefix >> 4;
        match (item_type, tag) {
            (1, 0) => usage_page = value,
            (1, 7) => report_size = value,
            (1, 8) => {
                return Err(
                    "Numbered HID reports are not supported by the FLUX PRO profile".into(),
                );
            }
            (1, 9) => report_count = value,
            (1, 10 | 11) => {
                return Err("HID global push/pop needs a separately validated profile".into());
            }
            (1, 1..=6) => {} // Logical/physical ranges and units do not set report length.
            (2, 0) => usage = Some(value),
            (2, 1..=5 | 7..=9) => {} // Local usage ranges and designator/string indices.
            (0, 10) => {
                if in_collection || collection_count != 0 || value != 1 {
                    return Err("Expected one HID application collection".into());
                }
                if usage_page != u32::from(USAGE_PAGE) || usage != Some(u32::from(USAGE)) {
                    return Err("HID descriptor application usage does not match ff00:0001".into());
                }
                in_collection = true;
                collection_count += 1;
            }
            (0, 12) => {
                if !in_collection || length != 0 {
                    return Err("Unbalanced HID end-collection item".into());
                }
                in_collection = false;
            }
            (0, 8 | 9) => {
                if !in_collection || report_size != 8 || report_count == 0 {
                    return Err("Expected byte-sized HID input/output fields inside the application collection".into());
                }
                let bits = report_size
                    .checked_mul(report_count)
                    .ok_or_else(|| "HID report length overflow".to_owned())?;
                let total = if tag == 8 {
                    &mut input_bits
                } else {
                    &mut output_bits
                };
                *total = total
                    .checked_add(bits)
                    .filter(|sum| *sum <= PAYLOAD_BITS)
                    .ok_or_else(|| "HID report exceeds the validated 64-byte payload".to_owned())?;
            }
            (0, 11) => {
                return Err(
                    "Unexpected HID feature report; controller profile is not supported".into(),
                );
            }
            _ => return Err(format!("Unsupported HID descriptor item 0x{prefix:02x}")),
        }
        // HID local items apply only to the next main item.
        if item_type == 0 {
            usage = None;
        }
    }
    if in_collection
        || collection_count != 1
        || input_bits != PAYLOAD_BITS
        || output_bits != PAYLOAD_BITS
    {
        return Err("HID descriptor must contain one complete application with exactly 64-byte input and output reports".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{validate_descriptor, validate_write_count};

    const PROFILE: &[u8] = &[
        0x06, 0x00, 0xff, 0x09, 0x01, 0xa1, 0x01, 0x15, 0x00, 0x26, 0xff, 0x00, 0x75, 0x08, 0x95,
        0x40, 0x09, 0x01, 0x81, 0x02, 0x09, 0x01, 0x91, 0x02, 0xc0,
    ];

    // Windows HIDAPI reconstruction for the controller profile with release number 6.
    const WINDOWS_PROFILE: &[u8] = &[
        0x06, 0x00, 0xff, 0x09, 0x01, 0xa1, 0x01, 0x09, 0x02, 0x15, 0x00, 0x26, 0x00, 0xff, 0x75,
        0x08, 0x95, 0x40, 0x81, 0x06, 0x09, 0x02, 0x15, 0x00, 0x26, 0x00, 0xff, 0x75, 0x08, 0x95,
        0x40, 0x91, 0x06, 0xc0,
    ];

    #[test]
    fn validates_known_layout_and_rejects_other_usage_or_lengths() {
        assert!(validate_descriptor(PROFILE).is_ok());
        assert!(validate_descriptor(WINDOWS_PROFILE).is_ok());
        for (index, replacement) in [(2, 1), (4, 0), (13, 1), (15, 63), (15, 65)] {
            let mut changed = PROFILE.to_vec();
            changed[index] = replacement;
            assert!(validate_descriptor(&changed).is_err(), "index {index}");
        }
    }

    #[test]
    fn rejects_numbered_feature_and_unsupported_descriptors() {
        for extra in [
            &[0x85, 1][..],
            &[0xb1, 2],
            &[0xa4],
            &[0xfe, 0, 0],
            &[0xa1, 1],
        ] {
            let mut changed = PROFILE[..PROFILE.len() - 1].to_vec();
            changed.extend_from_slice(extra);
            changed.push(0xc0);
            assert!(validate_descriptor(&changed).is_err());
        }
    }

    #[test]
    fn rejects_every_truncated_prefix_and_trailing_collection() {
        for length in 0..PROFILE.len() {
            assert!(validate_descriptor(&PROFILE[..length]).is_err());
        }
        let mut twice = PROFILE.to_vec();
        twice.extend_from_slice(PROFILE);
        assert!(validate_descriptor(&twice).is_err());
    }

    #[test]
    fn report_length_arithmetic_cannot_wrap() {
        let mut oversized = PROFILE[..14].to_vec();
        oversized.extend_from_slice(&[0x97, 0xff, 0xff, 0xff, 0xff]);
        oversized.extend_from_slice(&PROFILE[16..]);
        assert!(validate_descriptor(&oversized).is_err());
    }

    #[test]
    fn short_or_unexpected_write_counts_are_errors() {
        for count in [0, 1, 64, 66, usize::MAX] {
            assert!(validate_write_count(count).is_err());
        }
        assert!(validate_write_count(65).is_ok());
    }
}
