use crate::{
    config::{self, Config},
    device::DisplayDevice,
    process_watch::{ProcessState, ProcessWatch, ReporterIdentity},
    protocol, sensor,
};
use serde::{Deserialize, Serialize};
use std::fmt::Write as _;
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

#[derive(Debug, Clone, Serialize)]
pub struct Temperature {
    /// Original finite sensor reading, before display-specific conversion.
    pub celsius: Option<f64>,
    /// Encoding preview for this observation; not an acknowledgement from the device.
    pub display_celsius: protocol::DisplayTemperature,
    pub error: Option<String>,
}
impl From<Result<f64, String>> for Temperature {
    fn from(value: Result<f64, String>) -> Self {
        match value {
            Ok(v) if v.is_finite() => {
                let display_celsius = protocol::DisplayTemperature::from_celsius(v);
                Self {
                    celsius: Some(v),
                    display_celsius,
                    error: (!display_celsius.is_available())
                        .then(|| "Readings below 1°C cannot be shown on the display.".into()),
                }
            }
            Ok(_) => Self {
                celsius: None,
                display_celsius: protocol::DisplayTemperature::UNAVAILABLE,
                error: Some("Temperature is not a finite Celsius measurement.".into()),
            },
            Err(error) => Self {
                celsius: None,
                display_celsius: protocol::DisplayTemperature::UNAVAILABLE,
                error: Some(error),
            },
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Sample {
    pub observed_unix_ms: u128,
    pub reporter: Option<ReporterIdentity>,
    pub cpu: Temperature,
    pub gpu: Temperature,
    pub conflicting_writer: Option<String>,
    pub measurement_freshness: &'static str,
}
impl Sample {
    fn valid(&self) -> bool {
        self.cpu.display_celsius.is_available() && self.gpu.display_celsius.is_available()
    }
    fn report(&self) -> [u8; 65] {
        protocol::encode(self.cpu.display_celsius, self.gpu.display_celsius)
    }
}

pub fn capture(config: &Config, watch: &mut ProcessWatch) -> Sample {
    let state = watch.check(&config.registry.product);
    capture_with_state(config, state)
}

fn capture_with_state(config: &Config, state: ProcessState) -> Sample {
    let rows = match (state.source_error, state.reporter.as_ref()) {
        (Some(error), _) => Err(error),
        (None, None) => Err("HWiNFO reporter identity is unavailable.".into()),
        (None, Some(_)) => sensor::list_sensors(&config.registry),
    };
    let selected = |selector| {
        rows.as_ref()
            .map_err(Clone::clone)
            .and_then(|rows| sensor::select_reading(rows, selector))
    };
    Sample {
        observed_unix_ms: now_ms(),
        reporter: state.reporter,
        cpu: selected(&config.cpu).into(),
        gpu: selected(&config.gpu).into(),
        conflicting_writer: state.conflicting_writer,
        measurement_freshness: "unknown: Gadget exports have no measurement timestamp; process presence is not freshness evidence",
    }
}

pub fn sample(config: &Config, count: u32) -> Result<(), String> {
    let mut watch = ProcessWatch::new();
    let mut valid = true;
    for index in 0..count {
        let sample = capture(config, &mut watch);
        valid &= sample.valid();
        let report_hex = report_hex(&sample.report());
        println!(
            "{}",
            serde_json::json!({"sample": sample, "report_hex": report_hex, "device_opened": false, "device_written": false})
        );
        if index + 1 < count {
            thread::sleep(Duration::from_millis(config.interval_ms));
        }
    }
    if valid {
        Ok(())
    } else {
        Err(
            "One or more selected readings cannot be displayed. No device reports were sent."
                .into(),
        )
    }
}

fn report_hex(report: &[u8; protocol::REPORT_LEN]) -> String {
    let mut text = String::with_capacity(protocol::REPORT_LEN * 3 - 1);
    for (index, byte) in report.iter().enumerate() {
        if index != 0 {
            text.push(' ');
        }
        write!(&mut text, "{byte:02X}").expect("writing to a String cannot fail");
    }
    text
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Instance {
    session: String,
    pid: u32,
    started_unix_ms: u128,
}

struct RunGuard {
    _lock: File,
    dir: PathBuf,
    instance: Instance,
}
impl RunGuard {
    fn acquire() -> Result<Self, String> {
        let dir = config::state_dir()?;
        fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(dir.join("instance.lock"))
            .map_err(|e| e.to_string())?;
        lock.try_lock().map_err(|e| {
            format!("Cannot acquire the instance lock (another instance may be running): {e}")
        })?;
        let instance = Instance {
            session: format!(
                "{}-{}",
                std::process::id(),
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_nanos()
            ),
            pid: std::process::id(),
            started_unix_ms: now_ms(),
        };
        let guard = Self {
            _lock: lock,
            dir,
            instance,
        };
        guard.write_json("instance.json", &guard.instance)?;
        Ok(guard)
    }
    fn write_json(&self, name: &str, value: &impl Serialize) -> Result<(), String> {
        let temp = self
            .dir
            .join(format!("{name}.{}.tmp", self.instance.session));
        let mut created = false;
        let result = (|| {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temp)
                .map_err(|e| e.to_string())?;
            created = true;
            serde_json::to_writer(&mut file, value).map_err(|e| e.to_string())?;
            file.write_all(b"\n")
                .and_then(|()| file.flush())
                .map_err(|e| e.to_string())?;
            drop(file);
            fs::rename(&temp, self.dir.join(name)).map_err(|e| format!("Cannot update {name}: {e}"))
        })();
        if created && result.is_err() {
            let _ = fs::remove_file(&temp);
        }
        result
    }
    fn stop_requested(&self) -> bool {
        read_small(&self.dir.join("stop.request")).is_ok_and(|value| value == self.instance.session)
    }
}
impl Drop for RunGuard {
    fn drop(&mut self) {
        if read_instance(&self.dir).is_ok_and(|instance| instance.session == self.instance.session)
        {
            let _ = fs::remove_file(self.dir.join("instance.json"));
        }
        if self.stop_requested() {
            let _ = fs::remove_file(self.dir.join("stop.request"));
        }
    }
}
fn read_small(path: &Path) -> Result<String, String> {
    let mut value = String::new();
    File::open(path)
        .map_err(|e| e.to_string())?
        .take(65_537)
        .read_to_string(&mut value)
        .map_err(|e| e.to_string())?;
    if value.len() > 65_536 {
        return Err("State file exceeds 64 KiB.".into());
    }
    Ok(value)
}
fn read_instance(dir: &Path) -> Result<Instance, String> {
    serde_json::from_str(&read_small(&dir.join("instance.json"))?).map_err(|e| e.to_string())
}

pub fn stop() -> Result<(), String> {
    let dir = config::state_dir()?;
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .open(dir.join("instance.lock"))
        .map_err(|e| format!("No accessible instance lock: {e}"))?;
    match lock.try_lock() {
        Ok(()) => return Err("No instance is running.".into()),
        Err(std::fs::TryLockError::WouldBlock) => {}
        Err(error) => return Err(format!("Cannot inspect instance lock: {error}")),
    }
    let instance = read_instance(&dir)?;
    fs::write(dir.join("stop.request"), &instance.session).map_err(|e| e.to_string())?;
    println!(
        "Stop requested for session {}. Check `status` for completion.",
        instance.session
    );
    Ok(())
}

pub fn status() -> Result<(), String> {
    let dir = config::state_dir()?;
    let running = match OpenOptions::new()
        .read(true)
        .write(true)
        .open(dir.join("instance.lock"))
    {
        Ok(file) => match file.try_lock() {
            Ok(()) => false,
            Err(std::fs::TryLockError::WouldBlock) => true,
            Err(e) => return Err(e.to_string()),
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
        Err(e) => return Err(e.to_string()),
    };
    let last_status = match read_small(&dir.join("status.json")) {
        Ok(value) => {
            serde_json::from_str::<serde_json::Value>(&value).map_err(|e| e.to_string())?
        }
        Err(error) => serde_json::json!({"unavailable": error}),
    };
    println!(
        "{}",
        serde_json::json!({"instance_lock_held": running, "last_status": last_status})
    );
    Ok(())
}

#[derive(Clone, Copy)]
pub struct RunOptions {
    pub dry_run: bool,
    pub count: Option<u32>,
    pub leave_once: bool,
}

const CONFIG_RELOAD_INTERVAL: Duration = Duration::from_secs(1);

struct Schedule {
    scan_interval: Duration,
    display_interval: Duration,
    next_scan: Instant,
    next_display: Instant,
    next_reload: Instant,
}

impl Schedule {
    fn new(now: Instant, scan_ms: u64, display_ms: u64) -> Self {
        Self {
            scan_interval: Duration::from_millis(scan_ms),
            display_interval: Duration::from_millis(display_ms),
            next_scan: now,
            next_display: now,
            next_reload: now,
        }
    }

    fn update_intervals(&mut self, now: Instant, scan_ms: u64, display_ms: u64) {
        let scan = Duration::from_millis(scan_ms);
        let display = Duration::from_millis(display_ms);
        if self.scan_interval != scan {
            self.scan_interval = scan;
            self.next_scan = now;
        }
        if self.display_interval != display {
            self.display_interval = display;
            self.next_display = now;
        }
    }

    fn scan_completed(&mut self, now: Instant) {
        self.next_scan = next_deadline(self.next_scan, self.scan_interval, now);
    }

    fn display_completed(&mut self, now: Instant) {
        self.next_display = next_deadline(self.next_display, self.display_interval, now);
    }

    fn reload_completed(&mut self, now: Instant) {
        self.next_reload = next_deadline(self.next_reload, CONFIG_RELOAD_INTERVAL, now);
    }

    fn next_event(&self) -> Instant {
        self.next_scan.min(self.next_display).min(self.next_reload)
    }

    fn due_work(&self, now: Instant) -> (bool, bool) {
        (now >= self.next_scan, now >= self.next_display)
    }
}

fn sensor_read_due(cached: Option<&Sample>, state: &ProcessState, scheduled: bool) -> bool {
    scheduled
        || state.source_error.is_some()
        || state.reporter.is_none()
        || cached.is_none_or(|sample| sample.reporter != state.reporter)
}

fn next_deadline(previous: Instant, interval: Duration, now: Instant) -> Instant {
    let next = previous + interval;
    if next <= now {
        // Skip missed periods without catch-up writes.
        now + interval
    } else {
        next
    }
}

pub fn run(path: &Path, options: RunOptions) -> Result<(), String> {
    let interrupted = Arc::new(AtomicBool::new(false));
    let signal = Arc::clone(&interrupted);
    ctrlc::set_handler(move || signal.store(true, Ordering::Relaxed)).map_err(|e| e.to_string())?;
    run_with_stop(path, options, interrupted)
}

/// Run with a caller-owned stop flag; no process-global signal handler.
pub fn run_with_stop(
    path: &Path,
    options: RunOptions,
    interrupted: Arc<AtomicBool>,
) -> Result<(), String> {
    let mut document = config::load_document(path)?
        .ok_or_else(|| format!("Configuration {} does not exist.", path.display()))?;
    let guard = RunGuard::acquire()?;
    let mut watch = ProcessWatch::new();
    let mut device: Option<DisplayDevice> = None;
    let mut last_sample = None;
    let mut last_output_error = None;
    let mut last_health = String::new();
    let mut any_failure = false;
    let mut config_error = None;
    let mut display_iterations = 0u32;
    let mut last_display_attempt_unix_ms = None;
    let mut last_display_sample_unix_ms = None;
    let mut last_prepared_report_hex = None;
    // Attempt display cleanup even if the loop returns a status-write error.
    let outcome = (|| {
        let mut schedule = Schedule::new(
            Instant::now(),
            document.config.interval_ms,
            document.config.display_interval_ms,
        );
        loop {
            if interrupted.load(Ordering::Relaxed) || guard.stop_requested() {
                break;
            }
            let mut status_changed = false;
            if Instant::now() >= schedule.next_reload {
                let previous_error = config_error.clone();
                // Reload independently; retain valid settings and readings on reload errors.
                match config::load_document(path) {
                    Ok(Some(updated)) => {
                        if updated.revision != document.revision {
                            let now = Instant::now();
                            status_changed = updated.config != document.config;
                            schedule.update_intervals(
                                now,
                                updated.config.interval_ms,
                                updated.config.display_interval_ms,
                            );
                            if updated.config.registry != document.config.registry
                                || updated.config.cpu != document.config.cpu
                                || updated.config.gpu != document.config.gpu
                            {
                                last_sample = None;
                                schedule.next_scan = now;
                            }
                            if updated.config.device_path != document.config.device_path {
                                // Recheck writer conflicts before opening the replacement device.
                                device = None;
                            }
                            document = updated;
                        }
                        config_error = None;
                    }
                    Ok(None) => {
                        config_error = Some(format!(
                            "Configuration {} is missing; continuing with the last valid settings.",
                            path.display()
                        ));
                    }
                    Err(error) => {
                        config_error =
                            Some(format!("{error}; continuing with the last valid settings."));
                    }
                }
                status_changed |= config_error != previous_error;
                schedule.reload_completed(Instant::now());
            }
            let config = &document.config;
            let (scan_due, display_due) = schedule.due_work(Instant::now());
            if scan_due || display_due {
                // Share a fresh process snapshot across work due in this iteration.
                let state = watch.check(&config.registry.product);
                // Include work that became due during the snapshot without shifting deadlines.
                let (scan_due, display_due) = schedule.due_work(Instant::now());
                if sensor_read_due(last_sample.as_ref(), &state, scan_due) {
                    last_sample = Some(capture_with_state(config, state));
                    if scan_due {
                        schedule.scan_completed(Instant::now());
                    }
                    // Reporter changes can trigger early reads without postponing periodic scans.
                } else if let Some(sample) = &mut last_sample {
                    sample.conflicting_writer = state.conflicting_writer;
                }
                if display_due {
                    if interrupted.load(Ordering::Relaxed) || guard.stop_requested() {
                        return Ok(());
                    }
                    let Some(sample) = &last_sample else {
                        return Err(
                            "No temperature sample was captured for the display frame.".into()
                        );
                    };
                    last_display_attempt_unix_ms = Some(now_ms());
                    last_display_sample_unix_ms = Some(sample.observed_unix_ms);
                    // Real output and dry-run share a frame; scans preserve frame history.
                    let report = sample.report();
                    last_prepared_report_hex = Some(report_hex(&report));
                    let mut output_error = None;
                    if !options.dry_run {
                        if let Some(conflict) = &sample.conflicting_writer {
                            device = None;
                            output_error = Some(format!("Output paused: {conflict}"));
                        } else {
                            if device.is_none() {
                                match DisplayDevice::open(config.device_path.as_deref()) {
                                    Ok(opened) => device = Some(opened),
                                    Err(error) => output_error = Some(error),
                                }
                            }
                            if let Some(opened) = &device
                                && let Err(error) = opened.write_report(&report)
                            {
                                output_error = Some(error);
                                device = None;
                            }
                        }
                    }
                    last_output_error = output_error;
                    display_iterations = display_iterations.saturating_add(1);
                    schedule.display_completed(Instant::now());
                }
                status_changed = true;
            }
            if status_changed {
                let Some(sample) = &last_sample else {
                    return Err("No temperature observation is available for status.".into());
                };
                any_failure |=
                    !sample.valid() || last_output_error.is_some() || config_error.is_some();
                let phase = if last_output_error.is_some() {
                    "output_unavailable"
                } else if !sample.valid() {
                    "missing_temperature"
                } else if options.dry_run {
                    "dry_run"
                } else {
                    "displaying"
                };
                let health = format!(
                    "{phase}; cpu_error={:?}; gpu_error={:?}; output_error={last_output_error:?}; config_error={config_error:?}",
                    sample.cpu.error, sample.gpu.error
                );
                if health != last_health {
                    eprintln!("{health}");
                    last_health = health;
                }
                let value = serde_json::json!({
                    "session": guard.instance.session, "pid": guard.instance.pid,
                    "phase": phase, "dry_run": options.dry_run, "sample": sample,
                    "sample_role": "latest_observation",
                    "output_error": last_output_error, "config_error": config_error,
                    "config_path": path, "scan_interval_ms": config.interval_ms,
                    "display_interval_ms": config.display_interval_ms,
                    "display_iterations": display_iterations,
                    "last_display_attempt_unix_ms": last_display_attempt_unix_ms,
                    "last_display_sample_unix_ms": last_display_sample_unix_ms,
                    "last_prepared_report_hex": last_prepared_report_hex
                });
                guard.write_json("status.json", &value)?;
            }
            if options
                .count
                .is_some_and(|count| display_iterations >= count)
            {
                break;
            }
            let next = schedule.next_event();
            while Instant::now() < next {
                if interrupted.load(Ordering::Relaxed) || guard.stop_requested() {
                    return Ok(());
                }
                thread::sleep(
                    next.saturating_duration_since(Instant::now())
                        .min(Duration::from_millis(250)),
                );
            }
        }
        Ok::<(), String>(())
    })();
    let mut clear_error = None;
    let mut cleared = false;
    if !options.dry_run
        && !options.leave_once
        && let Some(opened) = &device
    {
        if let Some(conflict) = watch
            .check(&document.config.registry.product)
            .conflicting_writer
        {
            clear_error = Some(format!(
                "Did not clear while another writer may be active: {conflict}"
            ));
        } else {
            match opened.write_report(&protocol::encode(
                protocol::DisplayTemperature::UNAVAILABLE,
                protocol::DisplayTemperature::UNAVAILABLE,
            )) {
                Ok(()) => cleared = true,
                Err(error) => clear_error = Some(error),
            }
        }
    }
    let final_status = guard.write_json("status.json", &serde_json::json!({"session": guard.instance.session, "pid": guard.instance.pid, "phase": "stopped", "dry_run": options.dry_run, "sample": last_sample, "sample_role": "latest_observation", "output_error": last_output_error, "config_error": config_error, "config_path": path, "scan_interval_ms": document.config.interval_ms, "display_interval_ms": document.config.display_interval_ms, "display_iterations": display_iterations, "last_display_attempt_unix_ms": last_display_attempt_unix_ms, "last_display_sample_unix_ms": last_display_sample_unix_ms, "last_prepared_report_hex": last_prepared_report_hex, "had_failure": any_failure, "display_cleared": cleared, "clear_error": clear_error, "run_error": outcome.as_ref().err(), "left_once": options.leave_once}));
    let mut errors = Vec::new();
    if let Err(error) = outcome {
        errors.push(format!("Run: {error}"));
    }
    if let Err(error) = final_status {
        errors.push(format!("Final status: {error}"));
    }
    if let Some(error) = clear_error {
        errors.push(format!(
            "Display cleanup failed; the last temperature may remain visible: {error}"
        ));
    }
    if options.count.is_some() && any_failure {
        errors.push(
            "The bounded run encountered missing temperatures, output failures or configuration reload errors; see status."
                .into(),
        );
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("\n"))
    }
}

pub fn background(path: &Path, options: RunOptions) -> Result<(), String> {
    use std::os::windows::process::CommandExt;
    use std::process::{Command, Stdio};
    let path = path.canonicalize().map_err(|e| e.to_string())?;
    Config::load(&path)?;
    let mut command = Command::new(std::env::current_exe().map_err(|e| e.to_string())?);
    command
        .arg("run")
        .arg("--config")
        .arg(path)
        .creation_flags(0x0800_0000)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if options.dry_run {
        command.arg("--dry-run");
    }
    if options.leave_once {
        command.arg("--once");
    } else if let Some(count) = options.count {
        command.arg("--samples").arg(count.to_string());
    }
    let mut child = command.spawn().map_err(|e| e.to_string())?;
    thread::sleep(Duration::from_millis(400));
    if let Some(status) = child.try_wait().map_err(|e| e.to_string())? {
        if !status.success() {
            return Err(format!(
                "Background process exited with {status}. Run in the foreground for the error details."
            ));
        }
        println!("Background process completed; check `status`.");
    } else {
        println!(
            "Background process launched (PID {}). Use `status` to inspect output and `stop` to exit.",
            child.id()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cached_temperatures_survive_only_until_poll_or_reporter_change() {
        let identity = ReporterIdentity {
            pid: 42,
            start_time: 100,
        };
        let cached = Sample {
            observed_unix_ms: 123,
            reporter: Some(identity.clone()),
            cpu: Ok(50.0).into(),
            gpu: Ok(40.0).into(),
            conflicting_writer: None,
            measurement_freshness: "unknown",
        };
        let mut state = ProcessState {
            reporter: Some(identity.clone()),
            source_error: None,
            conflicting_writer: None,
        };
        assert!(!sensor_read_due(Some(&cached), &state, false));
        assert!(sensor_read_due(Some(&cached), &state, true));
        assert!(sensor_read_due(None, &state, false));

        // Writer detection is refreshed separately on every display frame; a
        // new conflict must not require another registry read to pause output.
        state.conflicting_writer = Some("iUnity.exe".into());
        assert!(!sensor_read_due(Some(&cached), &state, false));

        for changed in [
            ReporterIdentity {
                pid: 43,
                ..identity
            },
            ReporterIdentity {
                start_time: 101,
                ..identity
            },
        ] {
            state.reporter = Some(changed);
            assert!(sensor_read_due(Some(&cached), &state, false));
        }
        state.reporter = Some(identity);
        state.source_error = Some("process identity is uncertain".into());
        assert!(sensor_read_due(Some(&cached), &state, false));
        state.source_error = None;
        state.reporter = None;
        assert!(sensor_read_due(Some(&cached), &state, false));
    }

    #[test]
    fn missing_reporter_replaces_temperatures_without_reading_registry() {
        let config: Config = serde_json::from_str(include_str!("../config.example.json")).unwrap();
        let sample = capture_with_state(
            &config,
            ProcessState {
                reporter: None,
                source_error: None,
                conflicting_writer: Some("iUnity.exe".into()),
            },
        );
        assert!(sample.cpu.celsius.is_none());
        assert!(sample.gpu.celsius.is_none());
        assert_eq!(
            sample.cpu.error.as_deref(),
            Some("HWiNFO reporter identity is unavailable.")
        );
        assert_eq!(sample.conflicting_writer.as_deref(), Some("iUnity.exe"));
    }

    #[test]
    fn deadlines_avoid_processing_drift_and_skip_missed_events() {
        let start = Instant::now();
        let normal_finish = start + Duration::from_millis(200);
        assert_eq!(
            next_deadline(start, Duration::from_secs(1), normal_finish),
            start + Duration::from_secs(1)
        );
        let mut schedule = Schedule::new(start, 2000, 5000);
        let delayed_finish = start + Duration::from_secs(9);
        schedule.scan_completed(delayed_finish);
        schedule.display_completed(delayed_finish);
        schedule.reload_completed(delayed_finish);
        assert_eq!(schedule.next_scan, start + Duration::from_secs(11));
        assert_eq!(schedule.next_display, start + Duration::from_secs(14));
        assert_eq!(schedule.next_event(), start + Duration::from_secs(10));
        assert_eq!(
            next_deadline(
                schedule.next_scan,
                schedule.scan_interval,
                schedule.next_scan + Duration::from_millis(200)
            ),
            start + Duration::from_secs(13)
        );
    }

    fn scheduled_events(scan_ms: u64, display_ms: u64, until_ms: u64) -> (Vec<u64>, Vec<u64>) {
        let start = Instant::now();
        let mut now = start;
        let mut schedule = Schedule::new(start, scan_ms, display_ms);
        let mut scans = Vec::new();
        let mut displays = Vec::new();
        while now <= start + Duration::from_millis(until_ms) {
            let elapsed = now.duration_since(start).as_millis() as u64;
            let completed = now + Duration::from_millis(12);
            let (scan_due, display_due) = schedule.due_work(now);
            if scan_due {
                scans.push(elapsed);
                schedule.scan_completed(completed);
            }
            if display_due {
                displays.push(elapsed);
                schedule.display_completed(completed);
            }
            if now >= schedule.next_reload {
                schedule.reload_completed(completed);
            }
            let next = schedule.next_event();
            assert!(
                next > now,
                "completed events must not leave a busy-loop deadline"
            );
            now = next;
        }
        (scans, displays)
    }

    #[test]
    fn scan_and_display_events_have_independent_nonrounded_cadences() {
        let (scans, displays) = scheduled_events(2000, 5000, 10_000);
        assert_eq!(scans, [0, 2000, 4000, 6000, 8000, 10_000]);
        assert_eq!(displays, [0, 5000, 10_000]);
        let (scans, displays) = scheduled_events(5000, 2000, 10_000);
        assert_eq!(scans, [0, 5000, 10_000]);
        assert_eq!(displays, [0, 2000, 4000, 6000, 8000, 10_000]);
        let (scans, displays) = scheduled_events(1500, 2200, 6600);
        assert_eq!(scans, [0, 1500, 3000, 4500, 6000]);
        assert_eq!(displays, [0, 2200, 4400, 6600]);
    }

    #[test]
    fn snapshot_latency_coalesces_newly_due_work_without_moving_future_deadlines() {
        let start = Instant::now();
        let mut schedule = Schedule::new(start, 3000, 1000);
        schedule.next_scan = start + Duration::from_millis(5);
        assert_eq!(schedule.due_work(start), (false, true));

        let after_snapshot = start + Duration::from_millis(10);
        assert_eq!(schedule.due_work(after_snapshot), (true, true));
        schedule.scan_completed(after_snapshot);
        schedule.display_completed(after_snapshot);

        assert_eq!(schedule.next_scan, start + Duration::from_millis(3005));
        assert_eq!(schedule.next_display, start + Duration::from_millis(1000));
        assert_eq!(schedule.due_work(after_snapshot), (false, false));
        assert_eq!(
            schedule.due_work(start + Duration::from_millis(1000)),
            (false, true)
        );
        assert_eq!(schedule.next_reload, start);
    }

    #[test]
    fn live_interval_changes_make_only_the_changed_event_due_immediately() {
        let start = Instant::now();
        let mut schedule = Schedule::new(start, 2000, 5000);
        schedule.scan_completed(start);
        schedule.display_completed(start);
        schedule.reload_completed(start);
        let changed = start + Duration::from_millis(700);
        schedule.update_intervals(changed, 2000, 1000);
        assert_eq!(schedule.next_display, changed);
        assert_eq!(schedule.next_scan, start + Duration::from_secs(2));
        assert_eq!(schedule.next_reload, start + Duration::from_secs(1));
        schedule.display_completed(changed + Duration::from_millis(20));
        assert_eq!(schedule.next_display, start + Duration::from_millis(1700));

        let changed = start + Duration::from_millis(900);
        schedule.update_intervals(changed, 2700, 1000);
        assert_eq!(schedule.next_scan, changed);
        assert_eq!(schedule.next_display, start + Duration::from_millis(1700));
        schedule.scan_completed(changed + Duration::from_millis(20));
        assert_eq!(schedule.next_scan, start + Duration::from_millis(3600));

        schedule.update_intervals(start + Duration::from_secs(1), 2700, 1000);
        assert_eq!(schedule.next_scan, start + Duration::from_millis(3600));
        assert_eq!(schedule.next_display, start + Duration::from_millis(1700));
        assert_eq!(schedule.next_reload, start + Duration::from_secs(1));
    }

    #[test]
    fn display_conversion_preserves_the_original_measurement_and_source_errors() {
        for value in [-1.0, 0.0, 0.99, 1.0, 42.59, 99.9, 100.4, f64::MAX] {
            let temperature = Temperature::from(Ok(value));
            assert_eq!(temperature.celsius, Some(value));
            assert_eq!(temperature.display_celsius.is_available(), value >= 1.0);
            assert_eq!(temperature.error.is_some(), value < 1.0);
        }
        for value in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            let temperature = Temperature::from(Ok(value));
            assert!(temperature.celsius.is_none());
            assert!(!temperature.display_celsius.is_available());
            assert!(temperature.error.is_some());
        }
        assert_eq!(
            Temperature::from(Err("missing".into())).error.as_deref(),
            Some("missing")
        );
    }

    #[test]
    fn frame_uses_prepared_display_values_and_keeps_missing_slots_independent() {
        let sample = Sample {
            observed_unix_ms: 123,
            reporter: None,
            cpu: Ok(100.4).into(),
            gpu: Ok(42.59).into(),
            conflicting_writer: None,
            measurement_freshness: "unknown",
        };
        assert!(sample.valid());
        assert_eq!(&sample.report()[6..12], &[9, 9, 0, 4, 2, 6]);
        let json = serde_json::to_value(&sample).unwrap();
        assert_eq!(json["cpu"]["celsius"], 100.4);
        assert_eq!(json["cpu"]["display_celsius"], 99.0);
        assert_eq!(json["gpu"]["celsius"], 42.59);
        assert_eq!(json["gpu"]["display_celsius"], 42.6);
        for missing in [Ok(0.0), Ok(-2.0), Ok(0.5), Err("sensor missing".into())] {
            let sample = Sample {
                cpu: missing.into(),
                ..sample.clone()
            };
            assert!(!sample.valid());
            assert_eq!(&sample.report()[6..12], &[0xee, 0xee, 0xee, 4, 2, 6]);
        }
    }
}
