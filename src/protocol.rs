//! Independent encoder based on the published FLUX PRO display protocol.
//! https://github.com/renanmt/linux-flux-pro/blob/9c6d713607b5e6cfcab95c024c88568f7559f711/docs/PROTOCOL.md

pub const REPORT_LEN: usize = 65;

/// A prepared display value, separate from the original sensor measurement.
/// The private representation admits only whole degrees 1..=99 or unavailable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(transparent)]
pub struct DisplayTemperature(Option<u8>);

impl DisplayTemperature {
    pub const UNAVAILABLE: Self = Self(None);

    pub fn from_celsius(value: f64) -> Self {
        if !value.is_finite() || value < 1.0 {
            return Self::UNAVAILABLE;
        }
        // Truncate the fractional part and saturate before the integer cast.
        // This also bounds finite readings larger than an integer can hold.
        Self(Some(value.trunc().min(99.0) as u8))
    }

    pub fn is_available(self) -> bool {
        self.0.is_some()
    }

    fn digits(self) -> [u8; 3] {
        match self.0 {
            Some(value) => [value / 10, value % 10, 0],
            None => [0xee; 3],
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
    fn whole_degree_packet_checksum_and_windows_padding() {
        let report = encode(
            DisplayTemperature::from_celsius(42.5),
            DisplayTemperature::from_celsius(63.2),
        );
        assert_eq!(
            &report[..13],
            &[0, 0x55, 0xaa, 1, 1, 6, 4, 2, 0, 6, 3, 0, 0x16]
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
                &[0xee, 0xee, 0xee, 6, 3, 0]
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
        for value in [
            99.0,
            99.9,
            100.0,
            100.4,
            150.0,
            f64::from(i32::MAX),
            f64::MAX,
        ] {
            let display = DisplayTemperature::from_celsius(value);
            assert!(display.is_available());
            assert_eq!(display.digits(), [9, 9, 0]);
        }
    }

    #[test]
    fn every_positive_whole_degree_encodes_without_a_fraction() {
        for degree in 1..=99_u8 {
            let display = DisplayTemperature::from_celsius(f64::from(degree) + 0.75);
            let report = encode(display, DisplayTemperature::UNAVAILABLE);
            assert_eq!(
                [report[6], report[7], report[8]],
                [degree / 10, degree % 10, 0]
            );
            assert_eq!(
                report[12],
                report[1..12]
                    .iter()
                    .fold(0_u8, |sum, v| sum.wrapping_add(*v))
            );
        }
    }
}
