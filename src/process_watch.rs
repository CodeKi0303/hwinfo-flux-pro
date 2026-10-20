//! Minimal process presence checks, not ownership or sensor freshness evidence.

use crate::sensor::HwinfoProduct;
use serde::Serialize;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ReporterIdentity {
    pub pid: u32,
    pub start_time: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProcessState {
    pub reporter: Option<ReporterIdentity>,
    pub source_error: Option<String>,
    pub conflicting_writer: Option<String>,
}

#[derive(Default)]
pub struct ProcessWatch;

impl ProcessWatch {
    pub fn new() -> Self {
        Self
    }

    #[cfg(windows)]
    pub fn check(&mut self, product: &HwinfoProduct) -> ProcessState {
        use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System};

        // sysinfo 0.38 caches names/start times by PID. Recreate the snapshot
        // to handle PID reuse and release its handles after each check.
        let mut system = System::new();
        let refreshed = system.refresh_processes_specifics(
            ProcessesToUpdate::All,
            true,
            ProcessRefreshKind::nothing().without_tasks(),
        );
        if refreshed == 0 || system.processes().is_empty() {
            return unavailable(
                "process enumeration returned no processes; reporter and writer-conflict status are unknown",
            );
        }
        summarize(
            system.processes().values().map(|process| {
                (
                    process.name().to_str(),
                    ReporterIdentity {
                        pid: process.pid().as_u32(),
                        start_time: process.start_time(),
                    },
                )
            }),
            product,
        )
    }

    #[cfg(not(windows))]
    pub fn check(&mut self, _product: &HwinfoProduct) -> ProcessState {
        unavailable(
            "process presence checking requires Windows; reporter and writer-conflict status are unknown",
        )
    }
}

fn unavailable(reason: &str) -> ProcessState {
    ProcessState {
        reporter: None,
        source_error: Some(reason.into()),
        // Unknown writer state blocks output.
        conflicting_writer: Some("writer-conflict status could not be determined".into()),
    }
}

#[cfg(any(windows, test))]
fn summarize<'a>(
    processes: impl Iterator<Item = (Option<&'a str>, ReporterIdentity)>,
    product: &HwinfoProduct,
) -> ProcessState {
    const WRITERS: [&str; 3] = [
        "AntecHardwareMonitorWindowsService.exe",
        "iUnityService.exe",
        "iUnity.exe",
    ];
    let expected = match product {
        HwinfoProduct::Hwinfo64 => "HWiNFO64.exe",
        HwinfoProduct::Hwinfo32 => "HWiNFO32.exe",
    };
    let mut reporter = None;
    let mut reporter_count = 0;
    let mut conflicting_writer: Option<(&str, u32)> = None;
    let mut conflict_count = 0;
    let mut unknown_name = false;
    for (name, identity) in processes {
        let Some(name) = name else {
            unknown_name = true;
            continue;
        };
        if name.eq_ignore_ascii_case(expected) {
            reporter_count += 1;
            reporter = Some(identity.clone());
        }
        if WRITERS
            .iter()
            .any(|writer| name.eq_ignore_ascii_case(writer))
        {
            conflict_count += 1;
            if conflicting_writer.is_none_or(|(_, pid)| identity.pid < pid) {
                conflicting_writer = Some((name, identity.pid));
            }
        }
    }
    if unknown_name {
        return unavailable(
            "at least one process name is unreadable; reporter and writer-conflict status are unknown",
        );
    }
    let source_error = match reporter_count {
        0 => Some(format!("{expected} is not present in the process snapshot")),
        1 if reporter
            .as_ref()
            .is_some_and(|identity| identity.start_time == 0) =>
        {
            reporter = None;
            Some(format!(
                "{expected} is present but its start time could not be read"
            ))
        }
        1 => None,
        count => {
            reporter = None;
            Some(format!(
                "{count} {expected} processes are present; reporter identity is ambiguous"
            ))
        }
    };
    ProcessState {
        reporter,
        source_error,
        conflicting_writer: conflicting_writer.map(|(name, pid)| {
            if conflict_count == 1 {
                format!("known display writer {name} is running (PID {pid})")
            } else {
                format!("known display writer {name} is running (PID {pid}); {conflict_count} known writers found")
            }
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn process(name: &str, pid: u32, start_time: u64) -> (Option<&str>, ReporterIdentity) {
        (Some(name), ReporterIdentity { pid, start_time })
    }

    #[test]
    fn requires_one_reporter_with_a_known_start_time() {
        let single = summarize(
            [process("hwinfo64.EXE", 42, 100)].into_iter(),
            &HwinfoProduct::Hwinfo64,
        );
        assert_eq!(
            single.reporter,
            Some(ReporterIdentity {
                pid: 42,
                start_time: 100
            })
        );
        assert!(single.source_error.is_none());
        for processes in [
            vec![process("HWiNFO32.exe", 42, 100)],
            vec![process("HWiNFO64.exe", 42, 0)],
            vec![
                process("HWiNFO64.exe", 42, 100),
                process("HWiNFO64.exe", 43, 101),
            ],
        ] {
            let state = summarize(processes.into_iter(), &HwinfoProduct::Hwinfo64);
            assert!(state.reporter.is_none());
            assert!(state.source_error.is_some());
        }
    }

    #[test]
    fn detects_known_writers_without_relying_on_start_time_or_order() {
        let rows = [
            process("HWINFO64.exe", 42, 100),
            process("IUNITY.exe", 60, 0),
            process("AntecHardwareMonitorWindowsService.exe", 20, 0),
            process("unrelated-iUnity.exe", 10, 0),
        ];
        let forward = summarize(rows.clone().into_iter(), &HwinfoProduct::Hwinfo64);
        let reverse = summarize(rows.into_iter().rev(), &HwinfoProduct::Hwinfo64);
        assert_eq!(forward.conflicting_writer, reverse.conflicting_writer);
        assert!(
            forward
                .conflicting_writer
                .unwrap()
                .contains("2 known writers")
        );
        assert!(forward.reporter.is_some());
    }

    #[test]
    fn unknown_process_names_do_not_imply_no_conflict() {
        let state = summarize(
            [(
                None,
                ReporterIdentity {
                    pid: 9,
                    start_time: 0,
                },
            )]
            .into_iter(),
            &HwinfoProduct::Hwinfo64,
        );
        assert!(state.reporter.is_none());
        assert!(state.source_error.unwrap().contains("unknown"));
        assert!(state.conflicting_writer.is_some());
    }
}
