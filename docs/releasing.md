# Windows releases

The release target is `x86_64-pc-windows-msvc`. Rust is pinned in
`rust-toolchain.toml`; Cargo dependencies are pinned by the committed `Cargo.lock`.
Build on Windows with that Rust toolchain and the Visual Studio C++ build tools
and Windows SDK already installed. The scripts do not install application
software, drivers, HWiNFO, or iUnity. GitHub-hosted Windows runners provide the
native build tools and rustup prepares the pinned Rust toolchain.

Run the same checks as CI from the repository root:

```powershell
cargo fmt --check
cargo clippy --locked --all-targets --target x86_64-pc-windows-msvc -- -D warnings
cargo test --locked --target x86_64-pc-windows-msvc
./scripts/package-release.ps1
```

Check each command's exit status before continuing. Packaging always runs a
locked release build through `scripts/build-release.ps1`. That helper adds
`--remap-path-prefix` mappings for the project, Cargo home, user profile and
toolchain using a temporary Cargo configuration. Existing configured target
flags, including static CRT linkage, remain in place. It rejects environment
overrides that would bypass these mappings; it never changes those variables.
Run the helper directly when only the executable is needed. Inspect release
bytes for local paths as well as source files; compiler path remapping does not
rewrite literal strings or every native-library debug record.

Packaging produces a versioned standalone `.exe`, a ZIP,
`SHA256SUMS`, license notices, and build information in
`dist/hwinfo-flux-pro-VERSION-x86_64-pc-windows-msvc/`. The ZIP contains
`hwinfo-flux-pro.exe`, `config.example.json`, `README.md`, `LICENSE`,
`THIRD-PARTY-NOTICES.txt`, `RUST-THIRD-PARTY-NOTICES.html`, and `BUILD-INFO.txt`.
It also includes exactly three README screenshots under `docs/images/`:
`flux-pro-settings.png`, `hwinfo-gadget-cpu.png`, and `hwinfo-gadget-gpu.png`.
The same explicit image list drives copying, ZIP entries, and `SHA256SUMS` so
relative README links work after extraction. No other `docs/` contents are copied.
The ZIP has ten file entries; `SHA256SUMS` covers eleven standalone package files,
including the ZIP and all three images.
Prefer distributing the ZIP so the instructions and license notices travel with
the executable. Redistributing the standalone EXE also requires accompanying
license notices. HWiNFO is a separate prerequisite and is never bundled.

The 0.2 GUI and display worker share one Windows x64 executable. The ZIP layout
does not need a second GUI executable, a browser runtime, a managed runtime, or
an external `.manifest` file. Embed the application manifest in the release EXE;
verify its Common Controls v6 dependency, system DPI-awareness, and `asInvoker`
execution level from the built artifact. The selected native-windows-gui features
use Windows controls and the notification area. This does not establish a memory
budget: measure the complete UI/worker process with settings both open and hidden.
Only Windows x64/MSVC is currently a release target.

Keep `panic = "unwind"` for the desktop worker's panic-reporting boundary. The
worker catches an ordinary panic and wakes the UI with an error; aborting would
terminate the whole application. Unwinding does not prove the runtime's explicit
display-clear path completed. Do not remove features or declare an antivirus
false positive merely because a different build profile was not detected. Local
candidate packaging does not complete interactive GUI or antivirus verification.

Packaging prepares all assets in a unique `dist/.staging-*` directory and moves
the complete directory to the versioned destination only after every asset and
checksum succeeds. An existing versioned destination causes an error before the
build and is preserved. Previous output is never deleted or overwritten. Failed
staging directories remain for inspection and are excluded from CI uploads. If
rebuilding the same version locally, first preserve or relocate its previous
output yourself; a new application version must use its own Cargo version.

The notices generator follows normal/build dependencies in Cargo's resolved
Windows dependency graph, excluding dev-only edges. It reads license texts from
the downloaded crates and includes the public FLUX PRO protocol reference. It
selects MIT where available, the additional Unicode terms when required, and
the BSD option for bundled native HIDAPI. Unknown license expressions and a
changed HIDAPI version fail packaging pending review. This is a checked policy
for the current dependency set, not a general SPDX parser. Review changed source
notices and nested native code whenever updating dependencies. Rust's own
standard-library copyright report is copied from the exact build toolchain.

The GUI adds native-windows-gui 1.0.13, bitflags 1.3.2, lazy_static 1.5.1,
muldiv 0.2.1, and build-only winapi-build 0.1.1. Their declarations offer MIT;
the existing dual-license policy selects MIT. The native-windows-gui and
winapi-build crate archives omit their license files, so the notices generator
contains version-specific upstream MIT texts with pinned source links. The
winapi-build source was matched to its pinned upstream revision. The NWG 1.0.13
archive declares MIT, but its recorded VCS revision was unavailable during this
review; the copied license comes from the official repository and is not claimed
to be an exact source-revision match. Recheck these exceptions on version changes.

NWG's `no-styling` feature avoids its temporary-manifest activation path when
the application already embeds Common Controls v6. Its `high-dpi` implementation
uses system DPI; do not claim per-monitor-v2 support merely by changing a manifest
flag. Check settings at the Windows scale factors used for validation, tray menu
operation, settings close/reopen, worker stop/restart, and application exit. See
[Microsoft's manifest guidance](https://learn.microsoft.com/en-us/windows/win32/sbscs/application-manifests)
and [DPI-awareness guidance](https://learn.microsoft.com/en-us/windows/win32/hidpi/setting-the-default-dpi-awareness-for-a-process).

The initial build is **unsigned**. Windows may show an unknown-publisher or
SmartScreen warning. A SHA-256 checksum detects changed bytes but does not prove
publisher identity. If code signing is added, sign the EXE before packaging and
recompute every checksum; never place signing keys in the repository. ZIP entry
names and timestamps are deterministic, but byte-for-byte reproducible compiler
output is not claimed.

Before publishing a release, record real FLUX PRO display verification, HWiNFO
exit/restart, missing/renamed sensors, device reconnect, and single-writer
behavior. Unit tests and dry runs do not establish physical display behavior.
Measure the release EXE's size, idle working set/private memory, and CPU use over
a stated interval; distinguish cold startup from steady state and describe the
machine and sample duration. Keep unverified cases explicit in release notes.

`scripts/measure-runtime.ps1` is a PowerShell 7 harness for an authorized real
display test. Pass `-Executable`, `-Config`, a new `-OutputDirectory` under
`.local/`, and optionally `-Seconds 30`. It starts its own bounded display run,
samples memory/CPU/status every 500 ms, then requests a stop using that exact
session token. It refuses to start while another instance is running and does
not force-kill a process. This test sends real display reports. Its measured
child excludes HWiNFO and the observer; first-five-second data is startup data,
not proof of a cold OS cache. `run --samples N` counts display frames at the
configured `display_interval_ms` (default one second), while `sample --samples N`
counts registry observations at `interval_ms` (default two seconds). A runtime
status update may also describe a scan/config event; use the display iteration
and attempt timestamp fields when distinguishing transmissions from observations.

To prepare a GitHub release after configuring the intended remote, update the
Cargo version and lockfile, complete the checks above, commit the reviewed
changes, and push a matching version tag such as `v0.1.0`. The tag workflow checks
that the tag matches the package version, rebuilds/tests, and creates a **draft**
GitHub Release with the artifacts. It never publishes the draft automatically.
Review the assets, checksums, hardware verification, and notes before publishing
in GitHub. A retry does not overwrite an existing release; inspect an existing
draft before deciding how to update it.

Review the complete published Git history as well as the current files. A source
archive omits Git history; pushing an existing branch can expose earlier versions
of removed files. Keep private investigation records outside published history.

CI and release build jobs have read-only repository permissions. Only the final
draft-release job has `contents: write`, and it does not execute project source.
GitHub actions are pinned to verified full commit IDs. See the upstream
[checkout](https://github.com/actions/checkout/releases),
[upload-artifact](https://github.com/actions/upload-artifact/releases), and
[download-artifact](https://github.com/actions/download-artifact/releases)
release pages when updating pins, and the
[GitHub CLI release documentation](https://cli.github.com/manual/gh_release_create)
for draft-release semantics.
