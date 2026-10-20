# Verification

## Automated checks

```powershell
cargo fmt --check
cargo clippy --locked --all-targets --target x86_64-pc-windows-msvc -- -D warnings
cargo test --locked --target x86_64-pc-windows-msvc
```

The tests cover temperature conversion and missing values, packet framing and
checksums, HID profile validation, short writes, sensor identity and unit parsing,
consistent registry snapshots, writer conflicts, configuration backups and
concurrent edits, scheduling, worker cleanup, and startup registration.

## Runtime checks

Validated on Windows x64 with a FLUX PRO display:

- Gadget sensor reads, device-profile probing and normal temperature output.
- Independent display/scan intervals and live configuration updates.
- Sensor selection, Apply, Refresh, tray reopening, Stop/restart and GUI Exit.
- Recovery when HWiNFO exports become available after the app starts.
- Duplicate-instance handling and clearing the display on normal Stop.
- Startup registration/removal and execution of the registered command.
- Package checksums, license notices and README image paths.

Boundary temperatures are tested in code; arbitrary high temperatures have not
been forced onto the device. Actual Windows logoff/reboot startup, USB unplug/
reconnect, additional controller revisions and extended-duration operation
require separate validation.

## Limitations

HWiNFO Gadget exports do not include a measurement timestamp or heartbeat.
A readable value and a running HWiNFO process do not prove measurement freshness.
HID write success does not acknowledge the physical screen's contents.

Some builds have triggered Bitdefender detections. The cause and false-positive
status remain unresolved; successful later builds do not clear earlier findings.
Release notes must retain this known issue until it is resolved.

Record hardware, application versions, workload and duration when measuring
resources. Measure the GUI and worker together and report HWiNFO separately.
Local logs, dumps, configuration and measurement files belong in ignored storage.
