//! Independent encoder based on the published FLUX PRO display protocol.
//! https://github.com/renanmt/linux-flux-pro/blob/9c6d713607b5e6cfcab95c024c88568f7559f711/docs/PROTOCOL.md

pub const REPORT_LEN: usize = 65;

/// A prepared display value, separate from the original sensor measurement.
/// The private representation admits only tenths of a degree 10..=999 or unavailable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DisplayTemperature(Option<u16>);

impl DisplayTemperature {
    pub const UNAVAILABLE: Self = Self(None);

    pub fn from_celsius(value: f64) -> Self {
        if !value.is_finite() || value < 1.0 {
            return Self::UNAVAILABLE;
        }
        let tenths = if value >= 100.0 {
            990
        } else {
            // Rounding must not introduce a third whole-degree digit.
            (value * 10.0).round().min(999.0) as u16
        };
        Self(Some(tenths))
    }

    pub fn is_available(self) -> bool {
        self.0.is_some()
    }

    fn digits(self) -> [u8; 3] {
        match self.0 {
            Some(value) => [
                (value / 100) as u8,
                (value / 10 % 10) as u8,
                (value % 10) as u8,
            ],
            None => [0xee; 3],
        }
    }
}

impl serde::Serialize for DisplayTemperature {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match self.0 {
            Some(tenths) => serializer.serialize_some(&(f64::from(tenths) / 10.0)),
            None => serializer.serialize_none(),
        }
    }
}

pub fn encode(cpu: DisplayTemperature, gpu: DisplayTemperature) -> [u8; REPORT_LEN] {
    let mut report = [0_u8; REPORT_LEN];
    report[1..6].copy_from_slice(&[0x55, 0xaa, 0x01, 0x01, 0x06]);
    report[6..9].copy_from_slice(&cpu.digits());
    report[9..12].copy_from_slice(&gpu.digits());
    report[12] = report[1..12]
        .iter()
        .fold(0_u8, |sum, byte| sum.wrapping_add(*byte));
    report
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decimal_packet_checksum_and_windows_padding() {
        let report = encode(
            DisplayTemperature::from_celsius(42.5),
            DisplayTemperature::from_celsius(63.2),
        );
        assert_eq!(
            &report[..13],
            &[0, 0x55, 0xaa, 1, 1, 6, 4, 2, 5, 6, 3, 2, 0x1d]
        );
        assert!(report[13..].iter().all(|byte| *byte == 0));
    }

    #[test]
    fn nonpositive_subdegree_and_invalid_values_are_unavailable() {
        for value in [
            0.0,
            -0.0,
            -0.1,
            -100.0,
            0.999_999,
            f64::NAN,
            f64::INFINITY,
            f64::NEG_INFINITY,
        ] {
            let display = DisplayTemperature::from_celsius(value);
            assert!(!display.is_available());
            assert_eq!(
                &encode(display, DisplayTemperature::from_celsius(63.9))[6..12],
                &[0xee, 0xee, 0xee, 6, 3, 9]
            );
        }
        let unavailable = DisplayTemperature::UNAVAILABLE;
        assert_eq!(&encode(unavailable, unavailable)[6..12], &[0xee; 6]);
        assert_eq!(
            &encode(DisplayTemperature::from_celsius(1.0), unavailable)[6..12],
            &[0, 1, 0, 0xee, 0xee, 0xee]
        );
    }

    #[test]
    fn finite_high_readings_saturate_without_integer_overflow() {
        for value in [100.0, 100.4, 150.0, f64::from(i32::MAX), f64::MAX] {
            let display = DisplayTemperature::from_celsius(value);
            assert!(display.is_available());
            assert_eq!(display.digits(), [9, 9, 0]);
        }
    }

    #[test]
    fn rounding_carries_and_stays_within_three_digits() {
        for (value, expected) in [
            (42.54, [4, 2, 5]),
            (42.59, [4, 2, 6]),
            (9.96, [1, 0, 0]),
            (99.0, [9, 9, 0]),
            (99.9, [9, 9, 9]),
            (99.95, [9, 9, 9]),
            (99.99, [9, 9, 9]),
        ] {
            assert_eq!(DisplayTemperature::from_celsius(value).digits(), expected);
        }
    }

    #[test]
    fn every_representable_tenth_encodes_with_correct_place_values() {
        for tenths in 10..=999_u16 {
            let display = DisplayTemperature::from_celsius(f64::from(tenths) / 10.0);
            let report = encode(display, DisplayTemperature::UNAVAILABLE);
            assert_eq!(
                [report[6], report[7], report[8]],
                [
                    (tenths / 100) as u8,
                    (tenths / 10 % 10) as u8,
                    (tenths % 10) as u8,
                ]
            );
            assert_eq!(
                report[12],
                report[1..12]
                    .iter()
                    .fold(0_u8, |sum, v| sum.wrapping_add(*v))
            );
        }
    }

    #[test]
    fn serialized_values_remain_celsius_or_null() {
        for (value, expected) in [(42.5, 42.5), (99.99, 99.9), (100.0, 99.0)] {
            assert_eq!(
                serde_json::to_value(DisplayTemperature::from_celsius(value)).unwrap(),
                serde_json::json!(expected)
            );
        }
        assert_eq!(
            serde_json::to_value(DisplayTemperature::UNAVAILABLE).unwrap(),
            serde_json::Value::Null
        );
    }
}
