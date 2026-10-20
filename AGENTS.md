# Project rules

- Use safe Rust in application and tests: keep `#![forbid(unsafe_code)]` and the Cargo lint. Dependencies and the operating system contain their own low-level implementations; do not claim the entire dependency graph is unsafe-free.
- Use only public documentation/source and ordinary OS device observations. Never derive this project from proprietary FanControl/HWiNFO internals or copy vendor binaries.
- Preserve exact sensor identity, missing values, ambiguity, units, and source errors. HWiNFO Gadget exports have no measurement timestamp/heartbeat; a stationary temperature is not a failure signal.
- Scope device output to validated FLUX PRO HID profiles. Probes, listing, tests, and dry-run must not send HID reports. Only one application may write to the display.
- Keep the display worker synchronous and bounded. Use a small native Windows tray/settings UI, with no browser or managed GUI runtime. Avoid per-poll subprocesses, broad hardware scanners, and unbounded logs. Measure combined UI/worker memory, CPU and size before making performance claims.
- Do not stop/uninstall iUnity, change startup settings, or publish releases as incidental build steps. Package and review concrete artifacts first.
- Keep machine configuration, diagnostics, binaries, credentials, and `target/` out of Git. Include dependency license notices in release packages.
- Validate with `cargo fmt --check`, `cargo clippy --locked --all-targets -- -D warnings`, `cargo test --locked`, release build and read-only real-device probes. Label untested hardware and lifecycle cases explicitly.
