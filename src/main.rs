#![forbid(unsafe_code)]

#[cfg(not(all(windows, target_arch = "x86_64")))]
compile_error!("hwinfo-flux-pro supports Windows x64 only.");

mod config;
mod device;
mod process_watch;
mod protocol;
mod runtime;
mod sensor;
mod startup;
mod tray;

use config::Config;
use sensor::{HwinfoProduct, RegistryHive, RegistrySource, SensorReading, SensorSelector};
use std::{
    io::{self, Write},
    path::PathBuf,
};

const HELP: &str = "hwinfo-flux-pro — HWiNFO Gadget to Antec FLUX PRO (Windows x64)

  tray [--config PATH] [--settings]        Tray controls and sensor dropdowns
       [--background]                     Launch the tray without a console
  list                                    List Gadget registry exports (read only)
  probe [--device PATH]                    Validate USB descriptor (no reports sent)
  init [--config PATH] [--auto]             Create a new configuration
  sample [--config PATH] [--samples N]      Inspect temperatures; no HID access
  run [--config PATH] [--background]       Update the display until Ctrl+C or stop
      [--dry-run] [--samples N | --once]    --once leaves its one report displayed
  status                                  Inspect the instance lock and last status
  stop                                    Request a cooperative stop and clear
  --version                               Print version

Default config/state: %LOCALAPPDATA%\\hwinfo-flux-pro
Double-clicking the EXE opens the tray and sensor settings.
Stop iUnity before running. HWiNFO must export the selected temperatures to Gadget.
No measurement timestamps are available; a running HWiNFO cannot prove freshness.";

#[derive(Debug)]
struct Arguments {
    command: String,
    config: Option<PathBuf>,
    device: Option<String>,
    count: Option<u32>,
    dry_run: bool,
    background: bool,
    auto: bool,
    once: bool,
    settings: bool,
}

fn parse(args: impl IntoIterator<Item = String>) -> Result<Arguments, String> {
    let mut args = args.into_iter();
    let first = args.next();
    let default_tray = first.is_none();
    let command = match first.as_deref() {
        None => "tray".into(),
        Some("--help" | "-h" | "help") => "help".into(),
        Some("--version" | "-V") => "version".into(),
        Some(value) => value.to_owned(),
    };
    if ![
        "help", "version", "list", "probe", "init", "sample", "run", "status", "stop", "tray",
    ]
    .contains(&command.as_str())
    {
        return Err(format!("Unknown command: {command}. Use --help."));
    }
    let mut result = Arguments {
        command,
        config: None,
        device: None,
        count: None,
        dry_run: false,
        background: default_tray,
        auto: false,
        once: false,
        settings: default_tray,
    };
    while let Some(flag) = args.next() {
        match flag.as_str() {
            "--config"
                if ["init", "sample", "run", "tray"].contains(&result.command.as_str())
                    && result.config.is_none() =>
            {
                result.config = Some(PathBuf::from(args.next().ok_or("--config needs a path")?))
            }
            "--device" if result.command == "probe" && result.device.is_none() => {
                result.device = Some(args.next().ok_or("--device needs a path")?)
            }
            "--samples"
                if ["sample", "run"].contains(&result.command.as_str())
                    && result.count.is_none()
                    && !result.once =>
            {
                let count = args
                    .next()
                    .ok_or("--samples needs a positive count")?
                    .parse::<u32>()
                    .map_err(|_| "--samples needs a positive integer")?;
                if count == 0 {
                    return Err("--samples must be positive".into());
                }
                result.count = Some(count);
            }
            "--once" if result.command == "run" && !result.once && result.count.is_none() => {
                result.once = true;
                result.count = Some(1);
            }
            "--dry-run" if result.command == "run" && !result.dry_run => result.dry_run = true,
            "--background"
                if ["run", "tray"].contains(&result.command.as_str()) && !result.background =>
            {
                result.background = true
            }
            "--settings" if result.command == "tray" && !result.settings => result.settings = true,
            "--auto" if result.command == "init" && !result.auto => result.auto = true,
            _ => {
                return Err(format!(
                    "Unknown, duplicate, or inapplicable option: {flag}"
                ));
            }
        }
    }
    Ok(result)
}

fn sources() -> [RegistrySource; 4] {
    [
        RegistrySource {
            hive: RegistryHive::CurrentUser,
            product: HwinfoProduct::Hwinfo64,
        },
        RegistrySource {
            hive: RegistryHive::LocalMachine,
            product: HwinfoProduct::Hwinfo64,
        },
        RegistrySource {
            hive: RegistryHive::CurrentUser,
            product: HwinfoProduct::Hwinfo32,
        },
        RegistrySource {
            hive: RegistryHive::LocalMachine,
            product: HwinfoProduct::Hwinfo32,
        },
    ]
}
fn list() {
    for source in sources() {
        let result = match sensor::list_sensors(&source) {
            Ok(rows) => serde_json::json!({"source": source, "sensors": rows}),
            Err(error) => serde_json::json!({"source": source, "error": error}),
        };
        println!("{result}");
    }
}
fn choose(rows: &[SensorReading], label: &str) -> Result<SensorSelector, String> {
    print!("Select {label} row number: ");
    io::stdout().flush().map_err(|e| e.to_string())?;
    let mut answer = String::new();
    io::stdin()
        .read_line(&mut answer)
        .map_err(|e| e.to_string())?;
    let index = answer
        .trim()
        .parse::<usize>()
        .ok()
        .and_then(|n| n.checked_sub(1))
        .ok_or("Enter a listed positive row number")?;
    let row = rows.get(index).ok_or("The selected row does not exist")?;
    Ok(SensorSelector {
        sensor: row.sensor.clone(),
        label: row.label.clone(),
    })
}
fn auto_select(rows: &[SensorReading], cpu: bool) -> Result<SensorSelector, String> {
    let matches: Vec<_> = rows
        .iter()
        .filter(|row| {
            if cpu {
                row.sensor.starts_with("CPU [")
                    && ["CPU (Tctl/Tdie)", "CPU Package"].contains(&row.label.as_str())
            } else {
                (row.sensor.starts_with("GPU [") || row.sensor.starts_with("dGPU ["))
                    && ["GPU Temperature", "GPU 온도"].contains(&row.label.as_str())
            }
        })
        .collect();
    if matches.len() != 1 {
        return Err(format!(
            "Automatic {} selection found {} matches; use interactive `init`.",
            if cpu { "CPU" } else { "GPU" },
            matches.len()
        ));
    }
    Ok(SensorSelector {
        sensor: matches[0].sensor.clone(),
        label: matches[0].label.clone(),
    })
}
fn init(path: &std::path::Path, auto: bool) -> Result<(), String> {
    if path.exists() {
        return Err(format!(
            "{} already exists; it was not changed.",
            path.display()
        ));
    }
    let mut candidates = Vec::new();
    for source in sources() {
        if let Ok(rows) = sensor::list_sensors(&source) {
            let rows: Vec<_> = rows
                .into_iter()
                .filter(|row| row.celsius.is_some())
                .collect();
            if rows.len() >= 2 {
                candidates.push((source, rows));
            }
        }
    }
    if candidates.is_empty() {
        return Err("No registry source has two valid temperatures. Enable HWiNFO Gadget reporting, then use `list`.".into());
    }
    let source_index = if candidates.len() == 1 {
        0
    } else {
        for (index, (source, _)) in candidates.iter().enumerate() {
            println!(
                "{}: {}",
                index + 1,
                serde_json::to_string(source).map_err(|e| e.to_string())?
            );
        }
        if auto {
            return Err("Multiple registry sources contain temperatures; use interactive `init` to select the intended source.".into());
        }
        print!("Select registry source number (normally current_user/hwinfo64): ");
        io::stdout().flush().map_err(|e| e.to_string())?;
        let mut answer = String::new();
        io::stdin()
            .read_line(&mut answer)
            .map_err(|e| e.to_string())?;
        answer
            .trim()
            .parse::<usize>()
            .ok()
            .and_then(|n| n.checked_sub(1))
            .filter(|n| *n < candidates.len())
            .ok_or("Enter a listed registry source number")?
    };
    let (registry, rows) = candidates.swap_remove(source_index);
    println!(
        "Registry source: {}",
        serde_json::to_string(&registry).map_err(|e| e.to_string())?
    );
    for (index, row) in rows.iter().enumerate() {
        println!(
            "{}: {} / {} = {:?} Celsius",
            index + 1,
            row.sensor,
            row.label,
            row.celsius
        );
    }
    let (cpu, gpu) = if auto {
        (auto_select(&rows, true)?, auto_select(&rows, false)?)
    } else {
        (choose(&rows, "CPU")?, choose(&rows, "GPU")?)
    };
    let config = Config {
        schema_version: 1,
        registry,
        cpu,
        gpu,
        interval_ms: config::DEFAULT_SCAN_INTERVAL_MS,
        display_interval_ms: config::DEFAULT_DISPLAY_INTERVAL_MS,
        device_path: None,
    };
    config.save_new(path)?;
    println!(
        "Created {}. Use `sample` to verify, then `run --background`.",
        path.display()
    );
    Ok(())
}
fn execute() -> Result<(), String> {
    let args = parse(std::env::args().skip(1))?;
    match args.command.as_str() {
        "help" => println!("{HELP}"),
        "version" => println!("hwinfo-flux-pro {}", env!("CARGO_PKG_VERSION")),
        "tray" => {
            let path = args.config.map(Ok).unwrap_or_else(config::default_path)?;
            if args.background {
                launch_tray(&path, args.settings)?;
            } else {
                tray::run(&path, args.settings)?;
            }
        }
        "list" => list(),
        "probe" => {
            println!(
                "{}",
                serde_json::to_string(&device::list_devices()?).map_err(|e| e.to_string())?
            );
            println!(
                "{}",
                serde_json::json!({"validated": device::probe(args.device.as_deref())?, "reports_sent": 0})
            );
        }
        "init" => init(
            &args.config.map(Ok).unwrap_or_else(config::default_path)?,
            args.auto,
        )?,
        "sample" => runtime::sample(
            &Config::load(&args.config.map(Ok).unwrap_or_else(config::default_path)?)?,
            args.count.unwrap_or(1),
        )?,
        "run" => {
            let path = args.config.map(Ok).unwrap_or_else(config::default_path)?;
            let options = runtime::RunOptions {
                dry_run: args.dry_run,
                count: args.count,
                leave_once: args.once,
            };
            if args.background {
                runtime::background(&path, options)?;
            } else {
                runtime::run(&path, options)?;
            }
        }
        "status" => runtime::status()?,
        "stop" => runtime::stop()?,
        _ => unreachable!(),
    }
    Ok(())
}

fn launch_tray(path: &std::path::Path, settings: bool) -> Result<(), String> {
    use std::os::windows::process::CommandExt;
    use std::process::{Command, Stdio};
    let mut command = Command::new(std::env::current_exe().map_err(|e| e.to_string())?);
    command
        .arg("tray")
        .arg("--config")
        .arg(std::path::absolute(path).map_err(|e| e.to_string())?)
        .creation_flags(0x0800_0000)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if settings {
        command.arg("--settings");
    }
    let child = command.spawn().map_err(|e| e.to_string())?;
    println!("Tray process launched (PID {}).", child.id());
    Ok(())
}

fn main() {
    if let Err(error) = execute() {
        let first = std::env::args().nth(1);
        if first.as_deref().is_none_or(|arg| arg == "tray") {
            native_windows_gui::simple_message("hwinfo-flux-pro", &error);
        } else {
            eprintln!("Error: {error}");
        }
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_ambiguous_cli_options() {
        for command in [
            "sample --background",
            "run --once --samples 1",
            "run --samples 1 --once",
            "sample --samples 0",
            "status --config a",
            "run --config a --config b",
        ] {
            assert!(
                parse(command.split_whitespace().map(str::to_owned)).is_err(),
                "{command}"
            );
        }
        assert!(
            parse(
                "run --background --dry-run --samples 2"
                    .split_whitespace()
                    .map(str::to_owned)
            )
            .is_ok()
        );
    }

    #[test]
    fn double_click_opens_settings_without_changing_cli_help() {
        let default = parse(std::iter::empty()).unwrap();
        assert_eq!(default.command, "tray");
        assert!(default.background && default.settings);
        assert_eq!(parse(["--help".into()]).unwrap().command, "help");
        assert!(parse(["run".into(), "--settings".into()]).is_err());
        assert!(parse(["tray".into(), "--settings".into(), "--background".into()]).is_ok());
    }

    #[test]
    fn login_launch_preserves_config_path_without_showing_settings() {
        let path = r"C:\Users\테스트 사용자\FLUX PRO\config.json";
        let args = parse(["tray", "--background", "--config", path].map(str::to_owned)).unwrap();
        assert_eq!(args.command, "tray");
        assert!(args.background);
        assert!(!args.settings);
        assert_eq!(args.config.as_deref(), Some(std::path::Path::new(path)));
    }
}
