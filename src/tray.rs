//! Native Windows tray/settings UI. Only the owned worker performs device I/O.

use crate::{
    config::{self, Config, ConfigDocument, DEFAULT_DISPLAY_INTERVAL_MS, DEFAULT_SCAN_INTERVAL_MS},
    runtime::{self, RunOptions},
    sensor::{self, HwinfoProduct, RegistryHive, RegistrySource, SensorReading, SensorSelector},
    startup,
};
use native_windows_gui as nwg;
use std::{
    cell::{Cell, RefCell},
    fmt,
    fs::{self, File, OpenOptions},
    io::Read,
    path::{Path, PathBuf},
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
};

#[derive(Clone, Default)]
struct SourceChoice {
    source: Option<RegistrySource>,
    title: String,
}

impl fmt::Display for SourceChoice {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.title)
    }
}

#[derive(Clone, Default)]
struct SensorChoice {
    identity: Option<SensorSelector>,
    available: bool,
    title: String,
}

impl fmt::Display for SensorChoice {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.title)
    }
}

#[derive(Clone, Default)]
struct IntervalChoice {
    milliseconds: u64,
    custom: bool,
}

impl fmt::Display for IntervalChoice {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.custom {
            write!(formatter, "{}ms (사용자 설정)", self.milliseconds)
        } else {
            write!(formatter, "{}초", self.milliseconds / 1000)
        }
    }
}

#[derive(Clone)]
struct Draft {
    source: Option<RegistrySource>,
    cpu: Option<SensorSelector>,
    gpu: Option<SensorSelector>,
    display_interval_ms: u64,
    interval_ms: u64,
}

impl Default for Draft {
    fn default() -> Self {
        Self {
            source: None,
            cpu: None,
            gpu: None,
            display_interval_ms: DEFAULT_DISPLAY_INTERVAL_MS,
            interval_ms: DEFAULT_SCAN_INTERVAL_MS,
        }
    }
}

impl Draft {
    fn from_config(config: &Config) -> Self {
        Self {
            source: Some(config.registry.clone()),
            cpu: Some(config.cpu.clone()),
            gpu: Some(config.gpu.clone()),
            display_interval_ms: config.display_interval_ms,
            interval_ms: config.interval_ms,
        }
    }
}

struct Worker {
    stop: Arc<AtomicBool>,
    thread: JoinHandle<Result<(), String>>,
}

const WORKER_PANIC_ERROR: &str =
    "표시 작업이 예기치 않게 종료되었습니다. 표시부 정리는 확인되지 않았습니다.";

fn worker_entry(
    job: impl FnOnce() -> Result<(), String> + std::panic::UnwindSafe,
    notify: impl FnOnce(),
) -> Result<(), String> {
    let outcome = std::panic::catch_unwind(job);
    // Panics unwind resources but may skip display clearing and final status.
    // Notify before dropping a potentially panicking payload; join handles failure.
    notify();
    outcome.unwrap_or_else(|_| Err(WORKER_PANIC_ERROR.into()))
}

struct TrayUi {
    // Unbind before controls are dropped; Weak callbacks cannot retain the UI.
    handlers: RefCell<Vec<nwg::EventHandler>>,
    tray: nwg::TrayNotification,
    menu_settings: nwg::MenuItem,
    menu_start: nwg::MenuItem,
    menu_stop: nwg::MenuItem,
    menu_exit: nwg::MenuItem,
    menu: nwg::Menu,
    notice: nwg::Notice,
    source_label: nwg::Label,
    source: nwg::ComboBox<SourceChoice>,
    cpu_label: nwg::Label,
    cpu: nwg::ComboBox<SensorChoice>,
    gpu_label: nwg::Label,
    gpu: nwg::ComboBox<SensorChoice>,
    display_interval_label: nwg::Label,
    display_interval: nwg::ComboBox<IntervalChoice>,
    scan_interval_label: nwg::Label,
    scan_interval: nwg::ComboBox<IntervalChoice>,
    hint: nwg::Label,
    startup_toggle: nwg::CheckBox,
    startup_status: nwg::Label,
    selection_status: nwg::Label,
    worker_status: nwg::Label,
    refresh: nwg::Button,
    apply: nwg::Button,
    close: nwg::Button,
    window: nwg::Window,
    icon: nwg::Icon,
    path: PathBuf,
    startup_control: Result<startup::Controller, String>,
    startup_snapshot: RefCell<Option<startup::Snapshot>>,
    document: RefCell<Option<ConfigDocument>>,
    load_error: RefCell<Option<String>>,
    allow_create: Cell<bool>,
    draft: RefCell<Draft>,
    rebuilding: Cell<bool>,
    exiting: Cell<bool>,
    worker: RefCell<Option<Worker>>,
    // Preserve worker failures across refreshes until the next Start attempt.
    worker_error: RefCell<Option<String>>,
    exit_error: RefCell<Option<String>>,
}

impl TrayUi {
    fn build(path: &Path) -> Result<Rc<Self>, String> {
        let (document, load_error) = match config::load_document(path) {
            Ok(document) => (document, None),
            Err(error) => (None, Some(error)),
        };
        let draft = document
            .as_ref()
            .map(|document| Draft::from_config(&document.config))
            .unwrap_or_default();
        let allow_create = document.is_none() && load_error.is_none();
        let mut ui = Self {
            handlers: RefCell::new(Vec::new()),
            tray: Default::default(),
            menu_settings: Default::default(),
            menu_start: Default::default(),
            menu_stop: Default::default(),
            menu_exit: Default::default(),
            menu: Default::default(),
            notice: Default::default(),
            source_label: Default::default(),
            source: Default::default(),
            cpu_label: Default::default(),
            cpu: Default::default(),
            gpu_label: Default::default(),
            gpu: Default::default(),
            display_interval_label: Default::default(),
            display_interval: Default::default(),
            scan_interval_label: Default::default(),
            scan_interval: Default::default(),
            hint: Default::default(),
            startup_toggle: Default::default(),
            startup_status: Default::default(),
            selection_status: Default::default(),
            worker_status: Default::default(),
            refresh: Default::default(),
            apply: Default::default(),
            close: Default::default(),
            window: Default::default(),
            icon: nwg::Icon::from_system(nwg::OemIcon::Information),
            path: path.to_owned(),
            startup_control: startup::Controller::new(path),
            startup_snapshot: RefCell::new(None),
            document: RefCell::new(document),
            load_error: RefCell::new(load_error),
            allow_create: Cell::new(allow_create),
            draft: RefCell::new(draft),
            rebuilding: Cell::new(false),
            exiting: Cell::new(false),
            worker: RefCell::new(None),
            worker_error: RefCell::new(None),
            exit_error: RefCell::new(None),
        };
        ui.build_controls().map_err(|error| error.to_string())?;
        let ui = Rc::new(ui);
        let weak = Rc::downgrade(&ui);
        let handler =
            nwg::full_bind_event_handler(&ui.window.handle, move |event, data, handle| {
                if let Some(ui) = weak.upgrade() {
                    ui.handle_event(event, data, handle);
                }
            });
        ui.handlers.borrow_mut().push(handler);
        ui.refresh_exports();
        ui.refresh_startup();
        ui.update_worker_status();
        Ok(ui)
    }

    fn build_controls(&mut self) -> Result<(), nwg::NwgError> {
        nwg::Window::builder()
            .title("FLUX PRO · 온도 센서 설정")
            .size((760, 590))
            .center(true)
            .flags(nwg::WindowFlags::WINDOW)
            .icon(Some(&self.icon))
            .build(&mut self.window)?;
        nwg::TrayNotification::builder()
            .parent(&self.window)
            .icon(Some(&self.icon))
            .tip(Some("FLUX PRO · 센서 설정"))
            .build(&mut self.tray)?;
        nwg::Menu::builder()
            .popup(true)
            .parent(&self.window)
            .build(&mut self.menu)?;
        for (item, text) in [
            (&mut self.menu_settings, "센서 설정…"),
            (&mut self.menu_start, "표시 시작"),
            (&mut self.menu_stop, "표시 중지"),
            (&mut self.menu_exit, "종료"),
        ] {
            nwg::MenuItem::builder()
                .text(text)
                .parent(&self.menu)
                .build(item)?;
        }
        nwg::Notice::builder()
            .parent(&self.window)
            .build(&mut self.notice)?;
        for (label, text, y) in [
            (&mut self.source_label, "HWiNFO 내보내기 위치", 24),
            (&mut self.cpu_label, "CPU 표시부", 94),
            (&mut self.gpu_label, "GPU 표시부", 164),
        ] {
            nwg::Label::builder()
                .text(text)
                .position((20, y))
                .size((700, 22))
                .parent(&self.window)
                .build(label)?;
        }
        nwg::ComboBox::builder()
            .position((20, 49))
            .size((720, 200))
            .parent(&self.window)
            .build(&mut self.source)?;
        for (combo, y) in [(&mut self.cpu, 119), (&mut self.gpu, 189)] {
            nwg::ComboBox::builder()
                .position((20, y))
                .size((720, 250))
                .parent(&self.window)
                .build(combo)?;
        }
        // Create each native label immediately before its combo for accessibility.
        for (label, combo, text, x) in [
            (
                &mut self.display_interval_label,
                &mut self.display_interval,
                "표시 갱신 주기",
                20,
            ),
            (
                &mut self.scan_interval_label,
                &mut self.scan_interval,
                "데이터 스캔 주기",
                390,
            ),
        ] {
            nwg::Label::builder()
                .text(text)
                .position((x, 234))
                .size((350, 22))
                .parent(&self.window)
                .build(label)?;
            nwg::ComboBox::builder()
                .position((x, 260))
                .size((350, 200))
                .parent(&self.window)
                .build(combo)?;
        }
        nwg::Label::builder()
            .text("HWiNFO에서 Gadget으로 내보낸 온도 센서만 선택할 수 있습니다.")
            .position((20, 300))
            .size((720, 46))
            .parent(&self.window)
            .build(&mut self.hint)?;
        nwg::CheckBox::builder()
            .text("Windows 로그인 시 자동 시작")
            .position((20, 350))
            .size((720, 26))
            .parent(&self.window)
            .build(&mut self.startup_toggle)?;
        nwg::Label::builder()
            .position((20, 382))
            .size((720, 56))
            .parent(&self.window)
            .build(&mut self.startup_status)?;
        nwg::Label::builder()
            .position((20, 445))
            .size((720, 40))
            .parent(&self.window)
            .build(&mut self.selection_status)?;
        nwg::Label::builder()
            .position((20, 488))
            .size((720, 38))
            .parent(&self.window)
            .build(&mut self.worker_status)?;
        for (button, text, x) in [
            (&mut self.refresh, "새로고침", 20),
            (&mut self.apply, "적용", 550),
            (&mut self.close, "닫기", 650),
        ] {
            nwg::Button::builder()
                .text(text)
                .position((x, 545))
                .size((90, 30))
                .parent(&self.window)
                .build(button)?;
        }
        Ok(())
    }

    fn handle_event(&self, event: nwg::Event, data: nwg::EventData, handle: nwg::ControlHandle) {
        use nwg::Event;
        match event {
            Event::OnWindowClose if handle == self.window.handle => {
                if let nwg::EventData::OnWindowClose(close) = data {
                    close.close(false);
                }
                self.window.set_visible(false);
            }
            Event::OnContextMenu if handle == self.tray.handle || handle == self.window.handle => {
                self.update_worker_status();
                let (x, y) = nwg::GlobalCursor::position();
                self.menu.popup(x, y);
            }
            Event::OnMousePress(nwg::MousePressEvent::MousePressLeftUp)
                if handle == self.tray.handle =>
            {
                self.show_settings();
            }
            Event::OnMenuItemSelected if handle == self.menu_settings.handle => {
                self.show_settings()
            }
            Event::OnMenuItemSelected if handle == self.menu_start.handle => self.start_worker(),
            Event::OnMenuItemSelected if handle == self.menu_stop.handle => self.stop_worker(false),
            Event::OnMenuItemSelected if handle == self.menu_exit.handle => self.stop_worker(true),
            Event::OnButtonClick if handle == self.refresh.handle => {
                let message = self.reload_document();
                self.refresh_exports();
                self.refresh_startup();
                self.update_worker_status();
                if let Some(message) = message {
                    self.selection_status.set_text(&message);
                }
            }
            Event::OnButtonClick if handle == self.apply.handle => self.apply_settings(),
            Event::OnButtonClick if handle == self.startup_toggle.handle => self.toggle_startup(),
            Event::OnButtonClick if handle == self.close.handle => self.window.set_visible(false),
            Event::OnComboxBoxSelection if !self.rebuilding.get() => {
                if handle == self.source.handle {
                    let source = self
                        .source
                        .selection()
                        .and_then(|index| self.source.collection().get(index).cloned())
                        .and_then(|choice| choice.source);
                    self.draft.borrow_mut().source = source;
                    // Preserve absent sensor identities as placeholders.
                    self.refresh_exports();
                } else if handle == self.cpu.handle || handle == self.gpu.handle {
                    let combo = if handle == self.cpu.handle {
                        &self.cpu
                    } else {
                        &self.gpu
                    };
                    let identity = selected_identity(combo);
                    if handle == self.cpu.handle {
                        self.draft.borrow_mut().cpu = identity;
                    } else {
                        self.draft.borrow_mut().gpu = identity;
                    }
                    self.update_apply_enabled();
                } else if handle == self.display_interval.handle
                    || handle == self.scan_interval.handle
                {
                    if handle == self.display_interval.handle {
                        if let Some(milliseconds) = selected_interval(&self.display_interval) {
                            self.draft.borrow_mut().display_interval_ms = milliseconds;
                        }
                    } else if let Some(milliseconds) = selected_interval(&self.scan_interval) {
                        self.draft.borrow_mut().interval_ms = milliseconds;
                    }
                    self.update_interval_hint();
                    self.update_apply_enabled();
                }
            }
            Event::OnNotice if handle == self.notice.handle => self.worker_finished(),
            _ => {}
        }
    }

    fn show_settings(&self) {
        if self.exiting.get() {
            return;
        }
        let message = self.reload_document();
        self.refresh_exports();
        self.refresh_startup();
        self.update_worker_status();
        if let Some(message) = message {
            self.selection_status.set_text(&message);
        }
        self.window.set_visible(true);
        self.window.restore();
        self.window.set_focus();
    }

    /// Unchanged revisions preserve the draft; changed revisions replace it visibly.
    fn reload_document(&self) -> Option<String> {
        match config::load_document(&self.path) {
            Ok(Some(document)) => {
                let changed = self
                    .document
                    .borrow()
                    .as_ref()
                    .is_none_or(|previous| previous.revision != document.revision);
                if changed {
                    *self.draft.borrow_mut() = Draft::from_config(&document.config);
                }
                *self.document.borrow_mut() = Some(document);
                *self.load_error.borrow_mut() = None;
                self.allow_create.set(false);
                changed.then(|| "설정 파일이 변경되어 현재 저장된 설정을 불러왔습니다. 센서 선택과 주기를 확인하세요.".into())
            }
            Ok(None) if self.allow_create.get() => {
                *self.load_error.borrow_mut() = None;
                None
            }
            Ok(None) => {
                let error = "기존 설정 파일이 없어졌습니다. 마지막 선택을 유지하며 적용을 중지했습니다. 파일을 복원한 뒤 새로고침하세요.".to_owned();
                *self.load_error.borrow_mut() = Some(error.clone());
                Some(error)
            }
            Err(error) => {
                *self.load_error.borrow_mut() = Some(error.clone());
                Some(format!(
                    "설정을 읽지 못해 마지막 선택을 유지합니다. 파일을 확인한 뒤 새로고침하세요: {error}"
                ))
            }
        }
    }

    fn refresh_exports(&self) {
        let draft = self.draft.borrow().clone();
        let mut source_choices = vec![SourceChoice {
            source: None,
            title: "내보내기 위치를 선택하세요".into(),
        }];
        let mut selected_rows = None;
        for source in sources() {
            let rows = sensor::list_sensors(&source);
            let selected = draft.source.as_ref() == Some(&source);
            if source.product == HwinfoProduct::Hwinfo64 || rows.is_ok() || selected {
                source_choices.push(SourceChoice {
                    title: source_title(&source, rows.is_ok()),
                    source: Some(source),
                });
            }
            if selected {
                selected_rows = Some(rows);
            }
        }
        let source_selection = source_choices
            .iter()
            .position(|choice| choice.source == draft.source);
        let (rows, message) = match selected_rows {
            Some(Ok(rows)) => {
                let count = rows
                    .iter()
                    .filter(|row| row.celsius.is_some() && row.error.is_none())
                    .count();
                let message = if count == 0 {
                    "내보낸 온도가 없습니다. HWiNFO의 Gadget 설정에서 센서를 선택한 뒤 새로고침하세요.".into()
                } else {
                    format!("선택 가능한 온도 {count}개 · 변경 내용은 적용을 누르면 저장됩니다.")
                };
                (rows, message)
            }
            Some(Err(error)) => (
                Vec::new(),
                format!("선택한 위치의 센서를 읽지 못했습니다: {error}"),
            ),
            None => (
                Vec::new(),
                "내보내기 위치와 CPU/GPU 표시부의 센서를 각각 선택하세요.".into(),
            ),
        };
        let (cpu_choices, cpu_index) = sensor_choices(&rows, draft.cpu.as_ref());
        let (gpu_choices, gpu_index) = sensor_choices(&rows, draft.gpu.as_ref());
        let configured_intervals = self.document.borrow().as_ref().map(|document| {
            (
                document.config.display_interval_ms,
                document.config.interval_ms,
            )
        });
        let (display_choices, display_index) = interval_choices(
            draft.display_interval_ms,
            configured_intervals.map(|intervals| intervals.0),
        );
        let (scan_choices, scan_index) = interval_choices(
            draft.interval_ms,
            configured_intervals.map(|intervals| intervals.1),
        );
        self.rebuilding.set(true);
        self.source.set_collection(source_choices);
        self.source.set_selection(source_selection);
        self.cpu.set_collection(cpu_choices);
        self.cpu.set_selection(Some(cpu_index));
        self.gpu.set_collection(gpu_choices);
        self.gpu.set_selection(Some(gpu_index));
        self.display_interval.set_collection(display_choices);
        self.display_interval.set_selection(Some(display_index));
        self.scan_interval.set_collection(scan_choices);
        self.scan_interval.set_selection(Some(scan_index));
        self.rebuilding.set(false);
        self.selection_status.set_text(&message);
        self.update_interval_hint();
        self.update_apply_enabled();
    }

    fn update_interval_hint(&self) {
        self.hint.set_text(if self.draft.borrow().display_interval_ms > 1_000 {
            "HWiNFO Gadget 온도를 정수 단위로 표시합니다. 100°C 이상은 99.0°C로 표시합니다.\r\n표시 갱신 주기가 1초를 넘으면 표시부가 깜박일 수 있습니다."
        } else {
            "HWiNFO Gadget 온도를 정수 단위로 표시합니다. 100°C 이상은 99.0°C로 표시합니다."
        });
    }

    fn refresh_startup(&self) {
        let result = self
            .startup_control
            .as_ref()
            .map_err(Clone::clone)
            .and_then(startup::Controller::read);
        self.render_startup(result);
    }

    fn render_startup(&self, result: Result<startup::Snapshot, String>) {
        match result {
            Ok(snapshot) => {
                let (state, editable, message) = match snapshot.status {
                    startup::Status::Disabled => (
                        nwg::CheckBoxState::Unchecked,
                        true,
                        "선택 즉시 저장됩니다. 로그인하면 설정 창 없이 트레이에서 시작합니다.".to_owned(),
                    ),
                    startup::Status::Enabled => (
                        nwg::CheckBoxState::Checked,
                        true,
                        "자동 시작이 등록되어 있습니다. EXE를 이동하기 전에는 이 옵션을 해제하세요.".to_owned(),
                    ),
                    startup::Status::OtherRegistration => (
                        nwg::CheckBoxState::Indeterminate,
                        false,
                        "다른 실행 경로의 등록이 있어 변경하지 않습니다. 기존 EXE에서 자동 시작을 해제한 뒤 새로고침하세요.".to_owned(),
                    ),
                };
                self.startup_toggle
                    .set_tristate(state == nwg::CheckBoxState::Indeterminate);
                self.startup_toggle.set_check_state(state);
                self.startup_toggle
                    .set_enabled(editable && !self.exiting.get());
                self.startup_status.set_text(&message);
                *self.startup_snapshot.borrow_mut() = Some(snapshot);
            }
            Err(error) => {
                self.startup_toggle.set_tristate(true);
                self.startup_toggle
                    .set_check_state(nwg::CheckBoxState::Indeterminate);
                self.startup_toggle.set_enabled(false);
                self.startup_status
                    .set_text(&format!("자동 시작 상태를 확인하지 못했습니다: {error}"));
                *self.startup_snapshot.borrow_mut() = None;
            }
        }
    }

    fn toggle_startup(&self) {
        if self.exiting.get() {
            return;
        }
        let enabled = self.startup_toggle.check_state() == nwg::CheckBoxState::Checked;
        let result = (|| {
            if enabled {
                Config::load(&self.path)
                    .map_err(|error| format!("먼저 유효한 센서 설정을 저장하세요: {error}"))?;
            }
            let controller = self.startup_control.as_ref().map_err(Clone::clone)?;
            let expected = self.startup_snapshot.borrow();
            let expected = expected
                .as_ref()
                .ok_or("자동 시작 상태를 새로고침한 뒤 다시 시도하세요.")?;
            controller.set_enabled(expected, enabled)
        })();
        match result {
            Ok(snapshot) => self.render_startup(Ok(snapshot)),
            Err(error) => {
                // Render the observed registry state after failed writes.
                self.refresh_startup();
                nwg::modal_error_message(&self.window, "자동 시작", &error);
            }
        }
    }

    fn update_apply_enabled(&self) {
        let draft = self.draft.borrow();
        let enabled = !self.exiting.get()
            && self.load_error.borrow().is_none()
            && draft.source.is_some()
            && draft.cpu.is_some()
            && draft.gpu.is_some()
            && draft.cpu != draft.gpu
            && selected_interval(&self.display_interval).is_some()
            && selected_interval(&self.scan_interval).is_some()
            && selected_available(&self.cpu)
            && selected_available(&self.gpu);
        self.apply.set_enabled(enabled);
    }

    fn apply_settings(&self) {
        let result = self.save_settings();
        match result {
            Ok(()) => {
                self.refresh_exports();
                self.selection_status
                    .set_text("설정을 저장했습니다. 실행 중이면 다음 갱신 때 반영합니다. 중지 상태라면 메뉴의 ‘표시 시작’을 선택하세요.");
                // Saving settings preserves the worker's running/stopped state.
                self.update_worker_status();
            }
            Err(error) => {
                self.selection_status
                    .set_text(&format!("설정을 적용하지 않았습니다: {error}"));
                nwg::modal_error_message(&self.window, "센서 설정", &error);
            }
        }
    }

    fn save_settings(&self) -> Result<(), String> {
        if let Some(error) = self.load_error.borrow().as_ref() {
            return Err(format!("기존 설정을 읽지 못해 덮어쓰지 않습니다: {error}"));
        }
        let draft = self.draft.borrow().clone();
        let registry = draft.source.ok_or("내보내기 위치를 선택하세요.")?;
        let cpu = draft.cpu.ok_or("CPU 표시부의 온도 센서를 선택하세요.")?;
        let gpu = draft.gpu.ok_or("GPU 표시부의 온도 센서를 선택하세요.")?;
        if cpu == gpu {
            return Err("CPU와 GPU 표시부에는 서로 다른 센서를 선택하세요.".into());
        }
        // Revalidate exact sensor identities against a fresh snapshot before saving.
        let rows = sensor::list_sensors(&registry)?;
        sensor::select_reading(&rows, &cpu).map_err(|error| format!("CPU 센서: {error}"))?;
        sensor::select_reading(&rows, &gpu).map_err(|error| format!("GPU 센서: {error}"))?;
        let saved = {
            let document = self.document.borrow();
            let candidate = Config {
                schema_version: 1,
                registry,
                cpu,
                gpu,
                display_interval_ms: draft.display_interval_ms,
                interval_ms: draft.interval_ms,
                device_path: document
                    .as_ref()
                    .and_then(|document| document.config.device_path.clone()),
            };
            candidate.validate()?;
            config::save_if_unchanged(
                &self.path,
                document.as_ref().map(|document| &document.revision),
                &candidate,
            )?
        };
        *self.document.borrow_mut() = Some(saved);
        self.allow_create.set(false);
        Ok(())
    }

    fn start_worker(&self) {
        if self.exiting.get() || self.worker.borrow().is_some() {
            return;
        }
        *self.worker_error.borrow_mut() = None;
        if let Err(error) = Config::load(&self.path) {
            *self.worker_error.borrow_mut() = Some(format!("시작하지 못했습니다: {error}"));
            self.update_worker_status();
            self.window.set_visible(true);
            return;
        }
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let path = self.path.clone();
        let sender = self.notice.sender();
        let thread = thread::Builder::new()
            .name("flux-display".into())
            .spawn(move || {
                worker_entry(
                    move || {
                        runtime::run_with_stop(
                            &path,
                            RunOptions {
                                dry_run: false,
                                count: None,
                                leave_once: false,
                            },
                            thread_stop,
                        )
                    },
                    move || sender.notice(),
                )
            });
        match thread {
            Ok(thread) => {
                *self.worker.borrow_mut() = Some(Worker { stop, thread });
                self.update_worker_status();
            }
            Err(error) => {
                *self.worker_error.borrow_mut() =
                    Some(format!("표시 작업을 시작하지 못했습니다: {error}"));
                self.update_worker_status();
            }
        }
    }

    fn stop_worker(&self, exit: bool) {
        if exit {
            self.exiting.set(true);
        }
        let has_worker = {
            let worker = self.worker.borrow();
            if let Some(worker) = worker.as_ref() {
                worker.stop.store(true, Ordering::Relaxed);
                true
            } else {
                false
            }
        };
        self.update_worker_status();
        self.refresh.set_enabled(!self.exiting.get());
        self.source.set_enabled(!self.exiting.get());
        self.cpu.set_enabled(!self.exiting.get());
        self.gpu.set_enabled(!self.exiting.get());
        self.display_interval.set_enabled(!self.exiting.get());
        self.scan_interval.set_enabled(!self.exiting.get());
        self.startup_toggle.set_enabled(
            !self.exiting.get()
                && self
                    .startup_snapshot
                    .borrow()
                    .as_ref()
                    .is_some_and(|snapshot| snapshot.status != startup::Status::OtherRegistration),
        );
        self.update_apply_enabled();
        if !has_worker && self.exiting.get() {
            nwg::stop_thread_dispatch();
        }
    }

    fn worker_finished(&self) {
        let worker = self.worker.borrow_mut().take();
        let Some(worker) = worker else { return };
        let result = worker
            .thread
            .join()
            .unwrap_or_else(|_| Err(WORKER_PANIC_ERROR.into()));
        if let Err(error) = &result {
            *self.worker_error.borrow_mut() = Some(error.clone());
        }
        self.update_worker_status();
        if let Err(error) = result {
            if self.exiting.get() {
                *self.exit_error.borrow_mut() = Some(error.clone());
                nwg::modal_error_message(&self.window, "종료 상태 확인", &error);
            } else {
                self.window.set_visible(true);
            }
        }
        if self.exiting.get() {
            nwg::stop_thread_dispatch();
        }
    }

    fn update_worker_status(&self) {
        let (running, stopping) = {
            let worker = self.worker.borrow();
            (
                worker.is_some(),
                worker
                    .as_ref()
                    .is_some_and(|worker| worker.stop.load(Ordering::Relaxed)),
            )
        };
        self.menu_start.set_enabled(!running && !self.exiting.get());
        self.menu_stop
            .set_enabled(running && !stopping && !self.exiting.get());
        self.menu_settings.set_enabled(!self.exiting.get());
        self.menu_exit.set_enabled(!self.exiting.get());
        let worker_error = self.worker_error.borrow();
        let message = if stopping {
            "표시 작업을 중지하고 있습니다…".into()
        } else if running {
            current_worker_status().unwrap_or_else(|| {
                "표시 작업 실행 중 · 창을 닫아도 트레이에서 계속 동작합니다.".into()
            })
        } else if let Some(error) = worker_error.as_ref() {
            format!("표시 작업 오류: {error}")
        } else if let Some(error) = self.load_error.borrow().as_ref() {
            format!("설정 파일을 읽지 못했습니다. 원본을 확인한 뒤 새로고침하세요: {error}")
        } else {
            "표시 중지됨 · 창 또는 트레이 아이콘을 우클릭해 ‘표시 시작’을 선택하세요.".into()
        };
        self.worker_status.set_text(&message);
        self.tray.set_tip(if stopping {
            "FLUX PRO · 중지 중"
        } else if running {
            "FLUX PRO · 표시 작업 실행 중"
        } else if worker_error.is_some() {
            "FLUX PRO · 표시 작업 오류"
        } else {
            "FLUX PRO · 표시 중지됨"
        });
    }

    fn finish(&self) -> Result<(), String> {
        // Join the owned worker before releasing Notice/window resources.
        if let Some(worker) = self.worker.borrow_mut().take() {
            worker.stop.store(true, Ordering::Relaxed);
            worker
                .thread
                .join()
                .map_err(|_| WORKER_PANIC_ERROR.to_owned())??;
        }
        self.exit_error.borrow_mut().take().map_or(Ok(()), Err)
    }
}

impl Drop for TrayUi {
    fn drop(&mut self) {
        for handler in self.handlers.get_mut().drain(..) {
            nwg::unbind_event_handler(&handler);
        }
    }
}

fn interval_choices(selected_ms: u64, configured_ms: Option<u64>) -> (Vec<IntervalChoice>, usize) {
    let mut choices: Vec<_> = [1_000, 2_000, 3_000, 5_000, 10_000, 30_000, 60_000]
        .into_iter()
        .map(|milliseconds| IntervalChoice {
            milliseconds,
            custom: false,
        })
        .collect();
    // Keep the saved custom interval available while a preset is pending.
    for milliseconds in [Some(selected_ms), configured_ms].into_iter().flatten() {
        if !choices
            .iter()
            .any(|choice| choice.milliseconds == milliseconds)
        {
            choices.push(IntervalChoice {
                milliseconds,
                custom: true,
            });
        }
    }
    choices.sort_unstable_by_key(|choice| choice.milliseconds);
    let selected = choices
        .iter()
        .position(|choice| choice.milliseconds == selected_ms)
        .expect("the exact selected interval was inserted above");
    (choices, selected)
}

fn selected_interval(combo: &nwg::ComboBox<IntervalChoice>) -> Option<u64> {
    combo.selection().and_then(|index| {
        combo
            .collection()
            .get(index)
            .map(|choice| choice.milliseconds)
    })
}

fn selected_identity(combo: &nwg::ComboBox<SensorChoice>) -> Option<SensorSelector> {
    combo.selection().and_then(|index| {
        combo
            .collection()
            .get(index)
            .and_then(|choice| choice.identity.clone())
    })
}

fn selected_available(combo: &nwg::ComboBox<SensorChoice>) -> bool {
    combo.selection().is_some_and(|index| {
        combo
            .collection()
            .get(index)
            .is_some_and(|choice| choice.available)
    })
}

fn sensor_choices(
    rows: &[SensorReading],
    selected: Option<&SensorSelector>,
) -> (Vec<SensorChoice>, usize) {
    let mut choices = vec![SensorChoice {
        title: "온도 센서를 선택하세요".into(),
        ..Default::default()
    }];
    for row in rows {
        if let Some(value) = row.celsius.filter(|value| value.is_finite())
            && row.error.is_none()
        {
            choices.push(SensorChoice {
                identity: Some(SensorSelector {
                    sensor: row.sensor.clone(),
                    label: row.label.clone(),
                }),
                available: true,
                title: format!("{} / {}  ·  {:.1} °C", row.sensor, row.label, value),
            });
        }
    }
    let index = selected.map_or(0, |selected| {
        choices
            .iter()
            .position(|choice| choice.identity.as_ref() == Some(selected))
            .unwrap_or_else(|| {
                choices.push(SensorChoice {
                    identity: Some(selected.clone()),
                    available: false,
                    title: format!("[현재 없음] {} / {}", selected.sensor, selected.label),
                });
                choices.len() - 1
            })
    });
    (choices, index)
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

fn source_title(source: &RegistrySource, available: bool) -> String {
    let hive = match source.hive {
        RegistryHive::CurrentUser => "현재 사용자",
        RegistryHive::LocalMachine => "컴퓨터 전체",
    };
    let product = match source.product {
        HwinfoProduct::Hwinfo64 => "HWiNFO64",
        HwinfoProduct::Hwinfo32 => "HWiNFO32",
    };
    format!(
        "{hive} · {product}{}",
        if available {
            ""
        } else {
            " · 현재 읽기 불가"
        }
    )
}

fn read_state_json(path: &Path) -> Option<serde_json::Value> {
    let file = File::open(path).ok()?;
    let mut bytes = Vec::new();
    file.take(65_537).read_to_end(&mut bytes).ok()?;
    if bytes.len() > 65_536 {
        return None;
    }
    serde_json::from_slice(&bytes).ok()
}

fn current_worker_status() -> Option<String> {
    let dir = config::state_dir().ok()?;
    let instance = read_state_json(&dir.join("instance.json"))?;
    let status = read_state_json(&dir.join("status.json"))?;
    let own_pid = u64::from(std::process::id());
    if instance.get("pid")?.as_u64()? != own_pid
        || status.get("pid")?.as_u64()? != own_pid
        || instance.get("session")?.as_str()? != status.get("session")?.as_str()?
    {
        return None;
    }
    if let Some(error) = status.get("config_error").and_then(|error| error.as_str()) {
        return Some(format!(
            "새 설정을 반영하지 못해 이전 설정을 유지합니다: {error}"
        ));
    }
    match status.get("phase")?.as_str()? {
        "displaying" => {
            Some("온도를 표시하고 있습니다. 센서 값은 HWiNFO Gadget 설정을 따릅니다.".into())
        }
        "missing_temperature" => {
            Some("선택한 온도를 읽지 못했습니다. Gadget 내보내기와 센서 선택을 확인하세요.".into())
        }
        "output_unavailable" => Some(format!(
            "표시부 출력 대기: {}",
            status
                .get("output_error")
                .and_then(|error| error.as_str())
                .unwrap_or("장치 상태를 확인하세요.")
        )),
        _ => None,
    }
}

pub fn run(path: &Path, open_settings: bool) -> Result<(), String> {
    let dir = config::state_dir()?;
    fs::create_dir_all(&dir).map_err(|error| error.to_string())?;
    let tray_lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(dir.join("tray.lock"))
        .map_err(|error| error.to_string())?;
    match tray_lock.try_lock() {
        Ok(()) => {}
        // A background login launch yields silently to an existing instance.
        Err(std::fs::TryLockError::WouldBlock) if !open_settings => return Ok(()),
        Err(error) => {
            return Err(format!(
                "트레이 앱이 이미 실행 중이거나 잠금을 사용할 수 없습니다. 알림 영역의 FLUX PRO 아이콘을 여세요: {error}"
            ));
        }
    }
    nwg::init().map_err(|error| error.to_string())?;
    nwg::Font::set_global_family("Segoe UI").map_err(|error| error.to_string())?;
    let ui = TrayUi::build(path)?;
    let configured = ui.document.borrow().is_some();
    if configured {
        ui.start_worker();
    }
    if open_settings || !configured {
        ui.show_settings();
    }
    nwg::dispatch_thread_events();
    let result = ui.finish();
    drop(ui);
    drop(tray_lock);
    result
}

#[cfg(test)]
mod tests {
    use super::{
        Draft, SensorSelector, WORKER_PANIC_ERROR, interval_choices, sensor_choices, worker_entry,
    };
    use crate::{config::Config, sensor::SensorReading};
    use std::{
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
            mpsc,
        },
        thread,
        time::Duration,
    };

    #[test]
    fn worker_termination_notifies_once_after_resources_drop_for_all_outcomes() {
        struct Resource(Arc<AtomicUsize>);
        impl Drop for Resource {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }

        for outcome in ["success", "returned_error", "panic"] {
            let drops = Arc::new(AtomicUsize::new(0));
            let notifications = Arc::new(AtomicUsize::new(0));
            let job_drops = Arc::clone(&drops);
            let notice_drops = Arc::clone(&drops);
            let notice_count = Arc::clone(&notifications);
            let (sender, receiver) = mpsc::channel();
            let worker = thread::spawn(move || {
                worker_entry(
                    move || {
                        let _resource = Resource(job_drops);
                        match outcome {
                            "success" => Ok(()),
                            "returned_error" => Err("ordinary worker error".into()),
                            "panic" => panic!("worker termination regression"),
                            _ => unreachable!(),
                        }
                    },
                    move || {
                        notice_count.fetch_add(1, Ordering::SeqCst);
                        sender.send(notice_drops.load(Ordering::SeqCst)).unwrap();
                    },
                )
            });
            assert_eq!(
                receiver.recv_timeout(Duration::from_secs(5)).unwrap(),
                1,
                "{outcome}: owned resource must drop before terminal notification"
            );
            let result = worker.join().expect("ordinary job panic must be recovered");
            match outcome {
                "success" => assert_eq!(result, Ok(())),
                "returned_error" => assert_eq!(result, Err("ordinary worker error".into())),
                "panic" => assert_eq!(result, Err(WORKER_PANIC_ERROR.into())),
                _ => unreachable!(),
            }
            assert_eq!(drops.load(Ordering::SeqCst), 1);
            assert_eq!(notifications.load(Ordering::SeqCst), 1);
            assert!(matches!(
                receiver.try_recv(),
                Err(mpsc::TryRecvError::Disconnected)
            ));
        }
    }

    fn reading(sensor: &str, label: &str) -> SensorReading {
        SensorReading {
            sensor: sensor.into(),
            label: label.into(),
            index: "0".into(),
            value: "42.0 °C".into(),
            value_raw: "42".into(),
            celsius: Some(42.0),
            error: None,
        }
    }

    #[test]
    fn new_settings_default_to_one_second_display_and_two_second_scan() {
        let draft = Draft::default();
        assert_eq!(draft.display_interval_ms, 1_000);
        assert_eq!(draft.interval_ms, 2_000);
        let (display, display_index) = interval_choices(draft.display_interval_ms, None);
        let (scan, scan_index) = interval_choices(draft.interval_ms, None);
        assert_eq!(display[display_index].to_string(), "1초");
        assert_eq!(scan[scan_index].to_string(), "2초");
    }

    #[test]
    fn configured_nonpreset_intervals_survive_draft_and_dropdown_refresh() {
        let mut configured: Config =
            serde_json::from_str(include_str!("../config.example.json")).unwrap();
        configured.display_interval_ms = 1_500;
        configured.interval_ms = 2_500;
        let draft = Draft::from_config(&configured);
        assert_eq!(draft.display_interval_ms, 1_500);
        assert_eq!(draft.interval_ms, 2_500);
        for milliseconds in [draft.display_interval_ms, draft.interval_ms] {
            let (choices, index) = interval_choices(milliseconds, Some(milliseconds));
            assert_eq!(choices[index].milliseconds, milliseconds);
            assert_eq!(
                choices[index].to_string(),
                format!("{milliseconds}ms (사용자 설정)")
            );
            assert_eq!(
                choices
                    .iter()
                    .filter(|choice| choice.milliseconds == milliseconds)
                    .count(),
                1
            );
            // A pending preset selection must not erase the saved custom option.
            let (refreshed, pending_index) = interval_choices(3_000, Some(milliseconds));
            assert_eq!(refreshed[pending_index].milliseconds, 3_000);
            assert!(
                refreshed
                    .iter()
                    .any(|choice| choice.milliseconds == milliseconds)
            );
        }
    }

    #[test]
    fn refreshing_preserves_identity_after_reorder_and_marks_absent_selection() {
        let selected = SensorSelector {
            sensor: "CPU".into(),
            label: "Package".into(),
        };
        let rows = [reading("GPU", "Temperature"), reading("CPU", "Package")];
        let (choices, index) = sensor_choices(&rows, Some(&selected));
        assert_eq!(choices[index].identity.as_ref(), Some(&selected));
        assert!(choices[index].available);
        let (choices, index) = sensor_choices(&rows[..1], Some(&selected));
        assert_eq!(choices[index].identity.as_ref(), Some(&selected));
        assert!(!choices[index].available);
        assert!(choices[index].title.starts_with("[현재 없음]"));
    }

    #[test]
    fn unconfigured_slots_and_invalid_temperatures_are_not_selected() {
        let mut invalid = reading("CPU", "Package");
        invalid.celsius = None;
        invalid.error = Some("ambiguous duplicate".into());
        let (choices, index) = sensor_choices(&[invalid, reading("GPU", "Temperature")], None);
        assert_eq!(index, 0);
        assert_eq!(choices.len(), 2);
        assert!(choices[index].identity.is_none());
        assert!(!choices[index].available);
    }
}
