use crate::catalog::{
    CatalogModule, Layer, ManifestOption, OptionKind, Platform, SourceMode, SourceSpec, Target,
};
use crate::config::{Identity, validate_hostname, validate_username};
use crate::install::{
    BlockDevice, BootMode, DiskLayout, FileSystem, InstallPlan, InstallRequest, confirmation_token,
};
use crate::read_command::Cancellation;
use serde_json::Value;
use std::{collections::BTreeMap, env, fs, io, os::unix::fs::PermissionsExt, path::PathBuf};
use terminalib::{
    CellStyle, FrameBuffer, KEY_DOWN, KEY_FAIL, KEY_LEFT, KEY_PASTE, KEY_RIGHT, KEY_UP,
    MOUSE_EVENT, Mouse, MouseEvent, Rgb,
    terminal::{Terminal, associated_key_text},
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Mode {
    Install,
    Configure,
}

#[derive(Clone, Debug)]
pub struct ModuleRow {
    pub repository: usize,
    pub module: CatalogModule,
    pub selected: bool,
    pub values: BTreeMap<String, Value>,
}

#[derive(Clone)]
pub struct Session {
    pub mode: Mode,
    pub repositories: Vec<SourceSpec>,
    pub modules: Vec<ModuleRow>,
    pub flake: String,
    pub detected_flake: String,
    pub selector: String,
    pub identity: Identity,
    pub saved_identity: Identity,
    pub inherited_identity: Identity,
    pub suggested_identity: Identity,
    pub install: InstallRequest,
    pub disks: Vec<BlockDevice>,
    pub plan: Option<InstallPlan>,
    pub catalog_warning: Option<String>,
}

impl Session {
    pub fn new(
        mode: Mode,
        repositories: Vec<SourceSpec>,
        install: InstallRequest,
        flake: String,
        selector: String,
    ) -> Self {
        Self {
            mode,
            repositories,
            modules: Vec::new(),
            flake,
            detected_flake: String::new(),
            selector,
            identity: Identity::default(),
            saved_identity: Identity::default(),
            inherited_identity: Identity::default(),
            suggested_identity: Identity::default(),
            install,
            disks: Vec::new(),
            plan: None,
            catalog_warning: None,
        }
    }

    fn identity_value(&self, field: Field) -> (Option<&str>, &'static str) {
        let (edited, inherited, fallback) = match field {
            Field::Username => (
                &self.identity.username,
                &self.inherited_identity.username,
                &self.suggested_identity.username,
            ),
            Field::Hostname => (
                &self.identity.hostname,
                &self.inherited_identity.hostname,
                &self.suggested_identity.hostname,
            ),
            _ => unreachable!("system identity field"),
        };
        if let Some(value) = edited {
            (Some(value), "override")
        } else if let Some(value) = inherited {
            (Some(value), "inherited")
        } else {
            (fallback.as_deref(), "selector unset")
        }
    }

    pub fn target(&self) -> Target {
        Target::new(target_platform(self.mode), std::env::consts::ARCH)
    }

    pub fn host_flake(&self) -> &str {
        if self.flake.is_empty() {
            &self.detected_flake
        } else {
            &self.flake
        }
    }
}

#[derive(Clone, Debug)]
pub enum Action {
    LoadCatalog,
    RefreshCatalog,
    RefreshDisks,
    EditPartitions(String),
    Calamares,
    ReviewInstall,
    ApplyInstall,
    Save,
    Rebuild,
}

const BACK_TAB: i32 = -1;
const PAGE_UP: i32 = -2;
const PAGE_DOWN: i32 = -3;

const BG: Rgb = Rgb::new(12, 17, 28);
const PANEL: Rgb = Rgb::new(25, 32, 50);
const FOCUS: Rgb = Rgb::new(43, 60, 88);
const LINE: Rgb = Rgb::new(61, 76, 103);
const TEXT: Rgb = Rgb::new(231, 239, 251);
const MUTED: Rgb = Rgb::new(150, 164, 191);
const BLUE: Rgb = Rgb::new(123, 211, 247);
const GREEN: Rgb = Rgb::new(152, 225, 190);
const RED: Rgb = Rgb::new(255, 151, 154);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Page {
    Sources,
    Modules,
    System,
    Storage,
    Review,
}

impl Page {
    fn title(self) -> &'static str {
        match self {
            Self::Sources => "Module sources",
            Self::Modules => "Select modules",
            Self::System => "System settings",
            Self::Storage => "Installation storage",
            Self::Review => "Review changes",
        }
    }

    fn subtitle(self) -> &'static str {
        match self {
            Self::Sources => "Add local or Git module repositories, then load their catalog.",
            Self::Modules => {
                "Space toggles switches. Enter opens options. Ctrl+R restores a default."
            }
            Self::System => "Set the hostname, user and desktop for the new system.",
            Self::Storage => "Choose how to prepare or reuse the target filesystem.",
            Self::Review => "Check the selected modules and changes before applying.",
        }
    }
}

const INSTALL_PAGES: &[Page] = &[
    Page::Sources,
    Page::Modules,
    Page::System,
    Page::Storage,
    Page::Review,
];
const CONFIG_PAGES: &[Page] = &[Page::Sources, Page::Modules, Page::System, Page::Review];

#[derive(Clone, Copy)]
enum Field {
    Source(usize),
    Reference(usize),
    Search,
    Parameter(usize, usize),
    Flake,
    Selector,
    Hostname,
    Username,
    Timezone,
    Locale,
    Keymap,
    Desktop,
    Swap,
    Target,
}

#[derive(Clone, Copy)]
enum Setting {
    Layout,
    Boot,
    Filesystem,
    Encryption,
    Network,
    FormatRoot,
    FormatEsp,
}

#[derive(Clone, Copy)]
enum DeviceSlot {
    Disk,
    Root,
    Esp,
    AlongsideEsp,
    BootDisk,
    PartitionEditor,
}

#[derive(Clone)]
enum Control {
    Edit(Field),
    SelectModule(usize),
    ToggleParameter(usize, Vec<usize>),
    ResetParameter(usize, usize),
    AddRepository,
    ToggleSource(usize),
    RemoveRepository(usize),
    Change(Setting),
    Choose(DeviceSlot),
    ChooseParameter(usize, usize),
    Page(usize),
    ShowPlan,
    Send(Action),
    Confirm(Action),
}

struct Row {
    text: String,
    control: Option<Control>,
    color: Rgb,
}

impl Row {
    fn action(label: impl Into<String>, control: Control) -> Self {
        Self {
            text: label.into(),
            control: Some(control),
            color: TEXT,
        }
    }

    fn note(label: impl Into<String>) -> Self {
        Self {
            text: label.into(),
            control: None,
            color: MUTED,
        }
    }

    fn field(label: &str, value: &str, field: Field) -> Self {
        Self::action(
            format!("{label:<18} {}", if value.is_empty() { "—" } else { value }),
            Control::Edit(field),
        )
    }
}

struct Editor {
    text: String,
    cursor: usize,
    target: EditTarget,
    error: Option<String>,
}

#[derive(Clone)]
enum EditTarget {
    Field(Field),
    Confirm(Action, String),
}

struct Choice {
    value: Option<String>,
    label: String,
}

#[derive(Clone, Copy)]
enum PickerTarget {
    Device(DeviceSlot),
    Parameter(usize, usize),
}

struct Picker {
    target: PickerTarget,
    title: String,
    choices: Vec<Choice>,
    cursor: usize,
    offset: usize,
}

enum Modal {
    Edit(Editor),
    Pick(Picker),
    Notice {
        title: &'static str,
        text: String,
        offset: usize,
    },
}

#[derive(Clone, Copy)]
struct Area {
    x: i32,
    y: i32,
    w: i32,
    h: i32,
}

impl Area {
    fn contains(self, x: u16, y: u16) -> bool {
        let x = i32::from(x);
        let y = i32::from(y);
        x >= self.x && x < self.x + self.w && y >= self.y && y < self.y + self.h
    }
}

#[derive(Default)]
struct Ui {
    page: usize,
    focus: usize,
    offset: usize,
    search: String,
    modal: Option<Modal>,
    message: Option<(String, bool)>,
    applied: bool,
    catalog_dirty: bool,
    calamares: bool,
    loading: bool,
    nav_hits: Vec<(Area, usize)>,
    exit_hit: Option<Area>,
    accept_hit: Option<Area>,
    cancel_hit: Option<Area>,
    hits: Vec<(Area, usize)>,
    picker_hits: Vec<(Area, usize)>,
    visible_rows: usize,
}

enum Event {
    Key(i32),
    Text(String),
    Mouse(Mouse),
    Idle,
}

enum Outcome {
    Continue,
    Quit,
    Execute(Action),
}

type ActionHandler<T> = fn(&mut T, Action, &mut Session) -> Result<String, String>;

struct CatalogJob<T> {
    receiver: std::sync::mpsc::Receiver<(T, Session, Result<String, String>)>,
    stop: Cancellation,
}

impl<T: Send + 'static> CatalogJob<T> {
    fn start(
        mut controller: T,
        mut session: Session,
        action: Action,
        handle: ActionHandler<T>,
    ) -> Self {
        let (sender, receiver) = std::sync::mpsc::channel();
        let stop = Cancellation::default();
        let cancellation = stop.clone();
        std::thread::spawn(move || {
            let allow_network =
                session.mode == Mode::Install || matches!(action, Action::RefreshCatalog);
            let result = cancellation.scope(allow_network, || {
                handle(&mut controller, action, &mut session)
            });
            let _ = sender.send((controller, session, result));
        });
        Self { receiver, stop }
    }
}

impl<T> Drop for CatalogJob<T> {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}

pub fn run<T: Send + 'static>(
    mut state: Session,
    controller: T,
    handle: ActionHandler<T>,
) -> Result<(), String> {
    use std::sync::mpsc::TryRecvError;

    let error = |err: io::Error| format!("Terminal error: {err}");
    let mut terminal = open_terminal().map_err(error)?;
    let mut frame = FrameBuffer::default();
    let mut ui = Ui {
        loading: true,
        calamares: state.mode == Mode::Install && executable_in_path("calamares"),
        ..Ui::default()
    };
    let mut controller = Some(controller);
    let mut job = None::<CatalogJob<T>>;
    let mut redraw = true;
    loop {
        if let Some(active) = &job {
            match active.receiver.try_recv() {
                Ok((returned, updated, result)) => {
                    controller = Some(returned);
                    job = None;
                    ui.loading = false;
                    match result {
                        Ok(message) => {
                            state = updated;
                            ui.catalog_dirty = state.catalog_warning.is_some();
                            if let Some(warning) = &state.catalog_warning {
                                ui.failure("Catalog incomplete", warning.clone());
                            } else {
                                ui.message = Some((message, true));
                            }
                        }
                        Err(message) => {
                            ui.catalog_dirty = true;
                            ui.failure("Catalog unavailable", message);
                        }
                    }
                    redraw = true;
                }
                Err(TryRecvError::Empty) => {}
                Err(TryRecvError::Disconnected) => {
                    return Err("The catalog worker stopped unexpectedly.".into());
                }
            }
        }
        let (height, width) = terminal.getsize();
        if redraw || height != frame.height() || width != frame.width() {
            frame.resize(width, height);
            ui.render(&mut frame, &state);
            frame.present(&mut terminal.stdout).map_err(error)?;
            redraw = false;
        }
        if ui.loading && job.is_none() {
            job = Some(CatalogJob::start(
                controller.take().expect("idle controller"),
                state.clone(),
                Action::LoadCatalog,
                handle,
            ));
        }
        let event = match read_event(&mut terminal) {
            Ok(Event::Idle) => continue,
            Ok(event) => event,
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            Err(err) => return Err(error(err)),
        };
        redraw = true;
        match ui.event(&mut state, event) {
            Outcome::Continue => {}
            Outcome::Quit => return terminal.restore().map_err(error),
            Outcome::Execute(Action::RefreshCatalog) => {
                ui.loading = true;
                job = Some(CatalogJob::start(
                    controller.take().expect("idle controller"),
                    state.clone(),
                    Action::RefreshCatalog,
                    handle,
                ));
            }
            Outcome::Execute(action) => {
                terminal.restore().map_err(error)?;
                drop(terminal);
                if matches!(action, Action::EditPartitions(_) | Action::Calamares) {
                    state.plan = None;
                }
                if matches!(action, Action::Calamares) {
                    state.disks.clear();
                }
                let applied = matches!(action, Action::ApplyInstall);
                let review = matches!(action, Action::ReviewInstall);
                match handle(
                    controller.as_mut().expect("idle controller"),
                    action,
                    &mut state,
                ) {
                    Ok(message) => {
                        ui.applied |= applied;
                        ui.message = Some((message, true));
                    }
                    Err(message) => {
                        if review || applied {
                            state.plan = None;
                        }
                        ui.failure("Action failed", message);
                    }
                }
                terminal = open_terminal().map_err(error)?;
                frame.invalidate();
            }
        }
    }
}

fn open_terminal() -> io::Result<Terminal> {
    let mut terminal = Terminal::wannajustwork()?;
    terminal.enable_bracketed_paste()?;
    Ok(terminal)
}

fn read_event(terminal: &mut Terminal) -> io::Result<Event> {
    let key = terminal.get_key(100, false)?;
    if key == MOUSE_EVENT {
        return Ok(Event::Mouse(terminal.getmouse()));
    }
    if key == KEY_PASTE {
        return Ok(Event::Text(terminal.take_paste().unwrap_or_default()));
    }
    let buffer = terminal.reading_buffer.lock().unwrap();
    let len = *terminal.readed.lock().unwrap();
    let bytes = &buffer[..len];
    match bytes {
        b"\x1b[Z" => return Ok(Event::Key(BACK_TAB)),
        b"\x1b[5~" => return Ok(Event::Key(PAGE_UP)),
        b"\x1b[6~" => return Ok(Event::Key(PAGE_DOWN)),
        _ => {}
    }
    if let Some(text) = associated_key_text(bytes) {
        return Ok(Event::Text(text));
    }
    if let Ok(text) = std::str::from_utf8(bytes)
        && !text.is_empty()
        && text.chars().all(|character| !character.is_control())
    {
        return Ok(Event::Text(text.into()));
    }
    Ok(if key == KEY_FAIL {
        Event::Idle
    } else {
        Event::Key(key)
    })
}

fn pages(mode: Mode) -> &'static [Page] {
    match mode {
        Mode::Install => INSTALL_PAGES,
        Mode::Configure => CONFIG_PAGES,
    }
}

fn selected_count(state: &Session) -> usize {
    state
        .modules
        .iter()
        .filter(|module| module.selected)
        .count()
}

fn module_label(row: &ModuleRow, repositories: usize) -> String {
    if repositories > 1 {
        format!("[repo {}] {}", row.repository + 1, row.module.path)
    } else {
        row.module.path.clone()
    }
}

fn boot_name(boot: &BootMode) -> &'static str {
    match boot {
        BootMode::Uefi => "UEFI",
        BootMode::Bios => "BIOS",
    }
}

fn filesystem_name(filesystem: &FileSystem) -> &'static str {
    match filesystem {
        FileSystem::Ext4 => "ext4",
        FileSystem::Btrfs => "btrfs",
        FileSystem::Xfs => "XFS",
    }
}

fn yes_no(value: bool) -> &'static str {
    if value { "Yes" } else { "No" }
}

fn partition_name(value: &str) -> &str {
    if value.is_empty() {
        "Choose device"
    } else {
        value
    }
}

impl Ui {
    fn screen(&self, state: &Session) -> Page {
        pages(state.mode)[self.page]
    }

    fn rows(&self, state: &Session) -> Vec<Row> {
        let mut rows = match self.screen(state) {
            Page::Sources => {
                let mut rows = Vec::new();
                for (i, repo) in state.repositories.iter().enumerate() {
                    rows.push(Row::field(
                        &format!("Repository {}", i + 1),
                        &repo.repository,
                        Field::Source(i),
                    ));
                    rows.push(Row::action(
                        format!("Source mode        {}", source_mode(repo.mode)),
                        Control::ToggleSource(i),
                    ));
                    rows.push(Row::field(
                        "Git reference",
                        &repo.reference,
                        Field::Reference(i),
                    ));
                    rows.push(Row::action(
                        format!("Remove repository {}", i + 1),
                        Control::RemoveRepository(i),
                    ));
                }
                rows.push(Row::action("+ Add repository", Control::AddRepository));
                if state
                    .repositories
                    .iter()
                    .all(|repo| !repo.repository.trim().is_empty())
                {
                    rows.push(Row::action(
                        "Load catalog",
                        Control::Send(Action::RefreshCatalog),
                    ));
                } else {
                    rows.push(Row::note(
                        "Load catalog · enter every repository source first",
                    ));
                }
                if state
                    .repositories
                    .iter()
                    .any(|repo| repo.mode == SourceMode::Pinned)
                {
                    rows.push(Row::note("Pinned sources reuse their saved checkout. Change the ref to load another revision."));
                }
                if self.calamares {
                    rows.push(Row::action(
                        "Launch Calamares (external installer)…",
                        Control::Confirm(Action::Calamares),
                    ));
                    rows.push(Row::note(
                        "Calamares handles installation separately from this workflow.",
                    ));
                }
                if state.mode == Mode::Configure {
                    let detected = if state.detected_flake.is_empty() {
                        "Automatic from selector".to_string()
                    } else {
                        format!("{} (automatic)", state.detected_flake)
                    };
                    let flake = if state.flake.is_empty() {
                        &detected
                    } else {
                        &state.flake
                    };
                    rows.push(Row::field("Host flake", flake, Field::Flake));
                    rows.push(Row::field(
                        "Selector file",
                        &state.selector,
                        Field::Selector,
                    ));
                }
                rows
            }
            Page::Modules => {
                let mut rows = vec![Row::field("Filter modules", &self.search, Field::Search)];
                let query = self.search.to_lowercase();
                let mut found = false;
                let target = state.target();
                if state.catalog_warning.is_some() {
                    rows.push(Row::note(
                        "Some module sources are unavailable. Refresh before applying changes.",
                    ));
                } else if self.catalog_dirty {
                    rows.push(Row::note(
                        "Sources changed. Reload the catalog before applying changes.",
                    ));
                }
                for (index, row) in state.modules.iter().enumerate() {
                    let module = &row.module;
                    if !query.is_empty()
                        && !module.path.to_lowercase().contains(&query)
                        && !module.manifest.title.to_lowercase().contains(&query)
                        && !module.manifest.description.to_lowercase().contains(&query)
                        && !module.manifest.options.iter().any(|option| {
                            option.title.to_lowercase().contains(&query)
                                || option
                                    .path
                                    .iter()
                                    .any(|part| part.to_lowercase().contains(&query))
                        })
                    {
                        continue;
                    }
                    found = true;
                    let checked = if row.selected { "[×]" } else { "[ ]" };
                    let title = format!(
                        "{checked} {} · {}",
                        module_label(row, state.repositories.len()),
                        module.manifest.title
                    );
                    rows.push(mark_available(
                        Row::action(title, Control::SelectModule(index)),
                        module.manifest.available_on(&target),
                    ));
                    if row.selected {
                        rows.extend(parameter_rows(index, row, &target));
                    }
                }
                if !found {
                    rows.push(Row::note(if state.modules.is_empty() {
                        "Load module sources to populate the catalog."
                    } else {
                        "No modules match the filter."
                    }));
                }
                rows
            }
            Page::System if state.mode == Mode::Configure => {
                [("Hostname", Field::Hostname), ("Username", Field::Username)]
                    .into_iter()
                    .map(|(label, field)| {
                        let (value, origin) = state.identity_value(field);
                        Row::field(
                            label,
                            &format!("{} ({origin})", value.unwrap_or("—")),
                            field,
                        )
                    })
                    .collect()
            }
            Page::System => self.system_rows(&state.install),
            Page::Storage => self.storage_rows(state),
            Page::Review => self.review_rows(state),
        };
        if self.page > 0 {
            rows.push(Row::action("← Back", Control::Page(self.page - 1)));
        }
        if self.page + 1 < pages(state.mode).len() {
            rows.push(Row::action("Continue →", Control::Page(self.page + 1)));
        }
        rows
    }

    fn system_rows(&self, request: &InstallRequest) -> Vec<Row> {
        vec![
            Row::field("Hostname", &request.hostname, Field::Hostname),
            Row::field("Username", &request.username, Field::Username),
            Row::field("Timezone", &request.timezone, Field::Timezone),
            Row::field("Locale", &request.locale, Field::Locale),
            Row::field("Console keymap", &request.keymap, Field::Keymap),
            Row::field(
                "Desktop",
                request.desktop.as_deref().unwrap_or("None"),
                Field::Desktop,
            ),
            Row::action(
                format!("NetworkManager     {}", yes_no(request.network_manager)),
                Control::Change(Setting::Network),
            ),
        ]
    }

    fn storage_rows(&self, state: &Session) -> Vec<Row> {
        let request = &state.install;
        let layout = match &request.layout {
            DiskLayout::Erase { .. } => "Erase a disk",
            DiskLayout::Alongside { .. } => "Install in unallocated space",
            DiskLayout::Partitions { .. } => "Use existing partitions",
            DiskLayout::Mounted { .. } => "Use a mounted target",
            DiskLayout::InPlace => "Convert this Linux installation to NixOS",
        };
        let mut rows = vec![
            Row::action(
                format!("Layout             {layout}"),
                Control::Change(Setting::Layout),
            ),
            Row::action("Refresh disk list", Control::Send(Action::RefreshDisks)),
        ];
        match &request.layout {
            DiskLayout::InPlace => {
                rows.push(Row::note(
                    "Reuse the current root filesystem (/). The next boot starts NixOS.",
                ));
                rows.push(Row::note("Review the boot changes and the previous system's /old-root location before applying."));
            }
            DiskLayout::Erase { disk } => rows.push(Row::action(
                format!("Target disk        {}", partition_name(disk)),
                Control::Choose(DeviceSlot::Disk),
            )),
            DiskLayout::Alongside { disk, esp } => {
                rows.push(Row::note(
                    "Use an existing GPT disk with unallocated space.",
                ));
                rows.push(Row::action(
                    format!("Target disk        {}", partition_name(disk)),
                    Control::Choose(DeviceSlot::Disk),
                ));
                rows.push(if disk.is_empty() {
                    Row::note("EFI partition      Choose a target disk first")
                } else {
                    Row::action(
                        format!(
                            "EFI partition      {}",
                            esp.as_deref().unwrap_or("Create dedicated EFI")
                        ),
                        Control::Choose(DeviceSlot::AlongsideEsp),
                    )
                });
            }
            DiskLayout::Partitions {
                root,
                esp,
                boot_disk,
                format_root,
                format_esp,
            } => {
                rows.push(Row::action(
                    format!("Root partition     {}", partition_name(root)),
                    Control::Choose(DeviceSlot::Root),
                ));
                rows.push(Row::action(
                    format!("Format root        {}", yes_no(*format_root)),
                    Control::Change(Setting::FormatRoot),
                ));
                if matches!(request.boot_mode, BootMode::Uefi) {
                    rows.push(Row::action(
                        format!(
                            "EFI partition      {}",
                            esp.as_deref().unwrap_or("Choose device")
                        ),
                        Control::Choose(DeviceSlot::Esp),
                    ));
                    rows.push(Row::action(
                        format!("Format EFI         {}", yes_no(*format_esp)),
                        Control::Change(Setting::FormatEsp),
                    ));
                } else {
                    rows.push(Row::action(
                        format!(
                            "Boot disk          {}",
                            boot_disk.as_deref().unwrap_or("Choose device")
                        ),
                        Control::Choose(DeviceSlot::BootDisk),
                    ));
                }
            }
            DiskLayout::Mounted { target } => rows.push(Row::field(
                "Target mount",
                &target.to_string_lossy(),
                Field::Target,
            )),
        }
        rows.push(if matches!(request.layout, DiskLayout::InPlace) {
            Row::note(format!(
                "Boot mode          {} (current system)",
                boot_name(&request.boot_mode)
            ))
        } else if matches!(request.layout, DiskLayout::Alongside { .. }) {
            Row::note("Boot mode          UEFI (required for this layout)")
        } else {
            Row::action(
                format!("Boot mode          {}", boot_name(&request.boot_mode)),
                Control::Change(Setting::Boot),
            )
        });
        if !matches!(
            request.layout,
            DiskLayout::Mounted { .. } | DiskLayout::InPlace
        ) {
            rows.push(Row::action(
                format!(
                    "Filesystem         {}",
                    filesystem_name(&request.filesystem)
                ),
                Control::Change(Setting::Filesystem),
            ));
            rows.push(Row::action(
                format!("Encryption         {}", yes_no(request.encrypt)),
                Control::Change(Setting::Encryption),
            ));
            rows.push(Row::field(
                "Swap (GiB)",
                &request.swap_gib.to_string(),
                Field::Swap,
            ));
        }
        if request.layout != DiskLayout::InPlace {
            rows.push(Row::action(
                "Edit disk partitions (cfdisk)",
                Control::Choose(DeviceSlot::PartitionEditor),
            ));
        }
        rows
    }

    fn review_rows(&self, state: &Session) -> Vec<Row> {
        let mut rows = Vec::new();
        let active = state
            .modules
            .iter()
            .filter(|module| module.selected)
            .collect::<Vec<_>>();
        let target = state.target();
        rows.push(Row::note(format!(
            "{} selected components · {} repositories · {}",
            active.len(),
            state.repositories.len(),
            target.system,
        )));
        if state.mode == Mode::Configure {
            for (label, field, changed) in [
                (
                    "Hostname",
                    Field::Hostname,
                    state.identity.hostname != state.saved_identity.hostname,
                ),
                (
                    "Username",
                    Field::Username,
                    state.identity.username != state.saved_identity.username,
                ),
            ] {
                let (value, status) = state.identity_value(field);
                let change = if changed { " · change pending" } else { "" };
                rows.push(Row::note(format!(
                    "{label} · {} ({status}){change}",
                    value.unwrap_or("—")
                )));
            }
        }
        for module in active {
            let manifest = &module.module.manifest;
            let active_overrides = manifest
                .options
                .iter()
                .filter(|option| {
                    manifest.option_available_on(option, &target)
                        && module.values.contains_key(&option_key(option))
                })
                .count();
            rows.push(mark_available(
                Row::note(format!(
                    "  {} · {} active / {} saved overrides",
                    module_label(module, state.repositories.len()),
                    active_overrides,
                    module.values.len(),
                )),
                manifest.available_on(&target),
            ));
            let mut packages = package_groups(manifest);
            for option in &manifest.options {
                if is_package_switch(option) {
                    let Some(members) = packages.remove(option.path.as_slice()) else {
                        continue;
                    };
                    if !members.iter().any(|&index| {
                        module
                            .values
                            .contains_key(&option_key(&manifest.options[index]))
                    }) {
                        continue;
                    }
                    let available = available_members(manifest, &members, &target);
                    let active = !available.is_empty();
                    let targets = if active { &available } else { &members };
                    let title = &manifest.options[targets[0]].title;
                    rows.push(mark_parameter_availability(
                        Row::note(format!(
                            "    {} {title}",
                            switch_mark(module, targets, target.platform)
                        )),
                        active,
                        &target,
                    ));
                    continue;
                }
                if let Some(value) = module.values.get(&option_key(option)) {
                    rows.push(mark_parameter_availability(
                        Row::note(format!(
                            "    {} · {} = {}",
                            layer_name(option.layer),
                            option.title,
                            option_text(option.kind, value)
                        )),
                        manifest.option_available_on(option, &target),
                        &target,
                    ));
                }
            }
        }
        if self.catalog_dirty {
            rows.insert(
                0,
                Row::note("Reload the catalog on Module sources to continue."),
            );
            return rows;
        }
        match state.mode {
            Mode::Configure => {
                let selector_valid = PathBuf::from(&state.selector).is_absolute();
                rows.insert(
                    0,
                    if selector_valid {
                        Row::action("Save configuration", Control::Send(Action::Save))
                    } else {
                        Row::note(
                            "Save unavailable · set an absolute selector path on Module sources",
                        )
                    },
                );
                rows.insert(
                    1,
                    if selector_valid && !state.host_flake().trim().is_empty() {
                        Row::action("Rebuild", Control::Send(Action::Rebuild))
                    } else {
                        Row::note("Rebuild unavailable · load a selector with flakeDir")
                    },
                );
            }
            Mode::Install => {
                rows.push(Row::note(format!(
                    "{}@{} · {} · {}",
                    state.install.username,
                    state.install.hostname,
                    boot_name(&state.install.boot_mode),
                    if state.install.layout == DiskLayout::InPlace {
                        "existing root filesystem"
                    } else {
                        filesystem_name(&state.install.filesystem)
                    }
                )));
                if self.applied {
                    rows.insert(0, Row::note("Installation completed."));
                } else if let Some(plan) = &state.plan {
                    rows.insert(
                        0,
                        Row::action("View full installation plan", Control::ShowPlan),
                    );
                    rows.insert(
                        1,
                        Row::action(
                            "Apply installation plan…",
                            Control::Confirm(Action::ApplyInstall),
                        ),
                    );
                    rows.push(Row::note(format!(
                        "Install target · {}",
                        plan.target.display()
                    )));
                    for step in &plan.steps {
                        rows.push(Row::note(format!("  • {step}")));
                    }
                } else {
                    rows.insert(
                        0,
                        Row::action(
                            "Generate installation plan",
                            Control::Send(Action::ReviewInstall),
                        ),
                    );
                    rows.push(Row::note(
                        "Generate a plan to verify the storage and system settings.",
                    ));
                }
            }
        }
        rows
    }

    fn change_page(&mut self, page: usize) {
        self.page = page;
        self.focus = 0;
        self.offset = 0;
    }

    fn failure(&mut self, title: &'static str, message: String) {
        self.modal = Some(Modal::Notice {
            title,
            text: message.clone(),
            offset: 0,
        });
        self.message = Some((message, false));
    }

    fn event(&mut self, state: &mut Session, event: Event) -> Outcome {
        if self.loading {
            return match event {
                Event::Key(27) => Outcome::Quit,
                Event::Mouse(mouse)
                    if mouse.event == MouseEvent::LEFT
                        && self
                            .exit_hit
                            .is_some_and(|area| area.contains(mouse.x, mouse.y)) =>
                {
                    Outcome::Quit
                }
                _ => Outcome::Continue,
            };
        }
        if let Some(modal) = self.modal.take() {
            return self.modal_event(state, modal, event);
        }
        let rows = self.rows(state);
        if self.focus >= rows.len() {
            self.focus = rows.len().saturating_sub(1);
        }
        match event {
            Event::Key(27) => Outcome::Quit,
            Event::Key(14) if self.page + 1 < pages(state.mode).len() => {
                self.change_page(self.page + 1);
                Outcome::Continue
            }
            Event::Key(16) if self.page > 0 => {
                self.change_page(self.page - 1);
                Outcome::Continue
            }
            Event::Key(KEY_UP | KEY_LEFT | BACK_TAB) => {
                self.move_focus(&rows, false);
                Outcome::Continue
            }
            Event::Key(PAGE_UP) => {
                self.focus = self.focus.saturating_sub(self.visible_rows);
                Outcome::Continue
            }
            Event::Key(PAGE_DOWN) => {
                self.focus = (self.focus + self.visible_rows).min(rows.len().saturating_sub(1));
                Outcome::Continue
            }
            Event::Key(KEY_DOWN | KEY_RIGHT | 9) => {
                self.move_focus(&rows, true);
                Outcome::Continue
            }
            Event::Key(18) => {
                match rows.get(self.focus).and_then(|row| row.control.as_ref()) {
                    Some(Control::ToggleParameter(row, parameters)) => {
                        for &parameter in parameters {
                            reset_parameter(state, *row, parameter);
                        }
                    }
                    Some(Control::ChooseParameter(row, parameter)) => {
                        reset_parameter(state, *row, *parameter);
                    }
                    Some(Control::Edit(Field::Username)) if state.mode == Mode::Configure => {
                        state.identity.username = None;
                    }
                    Some(Control::Edit(Field::Hostname)) if state.mode == Mode::Configure => {
                        state.identity.hostname = None;
                    }
                    _ => {}
                }
                Outcome::Continue
            }
            Event::Key(10 | 13) => self.activate(
                state,
                rows.get(self.focus).and_then(|row| row.control.clone()),
            ),
            Event::Text(text) if text == " " => self.activate(
                state,
                rows.get(self.focus).and_then(|row| row.control.clone()),
            ),
            Event::Mouse(mouse) if mouse.event == MouseEvent::LEFT => {
                if self
                    .exit_hit
                    .is_some_and(|area| area.contains(mouse.x, mouse.y))
                {
                    return Outcome::Quit;
                }
                if let Some((_, page)) = self
                    .nav_hits
                    .iter()
                    .find(|(area, _)| area.contains(mouse.x, mouse.y))
                {
                    self.change_page(*page);
                    return Outcome::Continue;
                }
                if let Some((_, index)) = self
                    .hits
                    .iter()
                    .find(|(area, _)| area.contains(mouse.x, mouse.y))
                {
                    self.focus = *index;
                    self.activate(
                        state,
                        rows.get(self.focus).and_then(|row| row.control.clone()),
                    )
                } else {
                    Outcome::Continue
                }
            }
            Event::Mouse(mouse)
                if mouse.event == MouseEvent::WHEELUP || mouse.event == MouseEvent::WHEELDOWN =>
            {
                let max = rows.len().saturating_sub(self.visible_rows);
                self.offset = if mouse.event == MouseEvent::WHEELUP {
                    self.offset.saturating_sub(3)
                } else {
                    (self.offset + 3).min(max)
                };
                self.focus = self.focus.clamp(
                    self.offset,
                    (self.offset + self.visible_rows.saturating_sub(1))
                        .min(rows.len().saturating_sub(1)),
                );
                Outcome::Continue
            }
            _ => Outcome::Continue,
        }
    }

    fn move_focus(&mut self, rows: &[Row], forward: bool) {
        if forward {
            self.focus = (self.focus + 1).min(rows.len().saturating_sub(1));
        } else {
            self.focus = self.focus.saturating_sub(1);
        }
    }

    fn activate(&mut self, state: &mut Session, control: Option<Control>) -> Outcome {
        match control {
            Some(Control::Edit(field)) => {
                let text = self.field(state, field);
                self.modal = Some(Modal::Edit(Editor {
                    cursor: text.len(),
                    text,
                    target: EditTarget::Field(field),
                    error: None,
                }));
            }
            Some(Control::SelectModule(index)) => {
                state.modules[index].selected = !state.modules[index].selected;
                state.plan = None;
            }
            Some(Control::ToggleParameter(row, parameters)) => {
                let platform = target_platform(state.mode);
                state.modules[row].toggle(&parameters, platform);
                state.plan = None;
            }
            Some(Control::ResetParameter(row, parameter)) => {
                reset_parameter(state, row, parameter);
            }
            Some(Control::AddRepository) => {
                state.repositories.push(SourceSpec {
                    repository: String::new(),
                    reference: String::new(),
                    mode: SourceMode::Local,
                });
                self.catalog_dirty = true;
                state.plan = None;
                self.focus = (state.repositories.len() - 1) * 4;
                return self.activate(
                    state,
                    Some(Control::Edit(Field::Source(state.repositories.len() - 1))),
                );
            }
            Some(Control::ToggleSource(index)) => {
                let source = &mut state.repositories[index];
                source.mode = match source.mode {
                    SourceMode::Local => SourceMode::Pinned,
                    SourceMode::Pinned => SourceMode::Local,
                };
                self.catalog_dirty = true;
                state.plan = None;
            }
            Some(Control::RemoveRepository(index)) => {
                state.repositories.remove(index);
                // Keep the loaded catalog's repository indices intact until refresh.
                self.catalog_dirty = true;
                state.plan = None;
                self.message = Some((
                    "Repository removed. Reload the catalog to update its modules.".into(),
                    true,
                ));
            }
            Some(Control::Change(setting)) => {
                change_setting(&mut state.install, setting);
                state.plan = None;
            }
            Some(Control::Choose(slot)) => {
                let choices = device_choices(&state.disks, &state.install.layout, slot);
                if choices.is_empty() {
                    self.message = Some((
                        "No devices match this selection. Refresh the disk list.".into(),
                        false,
                    ));
                } else {
                    self.modal = Some(Modal::Pick(Picker {
                        target: PickerTarget::Device(slot),
                        title: match slot {
                            DeviceSlot::Disk => "Choose installation disk",
                            DeviceSlot::Root => "Choose root partition",
                            DeviceSlot::Esp => "Choose EFI System Partition",
                            DeviceSlot::AlongsideEsp => "Dedicated or existing EFI partition",
                            DeviceSlot::BootDisk => "Choose boot disk",
                            DeviceSlot::PartitionEditor => "Select disk for cfdisk",
                        }
                        .into(),
                        choices,
                        cursor: 0,
                        offset: 0,
                    }));
                }
            }
            Some(Control::ChooseParameter(row, parameter)) => {
                let module = &state.modules[row];
                let option = &module.module.manifest.options[parameter];
                if let Some(values) = &option.choices {
                    let current = module
                        .values
                        .get(&option_key(option))
                        .and_then(Value::as_str);
                    let cursor = current
                        .and_then(|current| values.iter().position(|value| value == current))
                        .map_or(0, |index| index + 1);
                    let default = option
                        .default_for(target_platform(state.mode))
                        .and_then(Value::as_str);
                    let mut choices = vec![Choice {
                        value: None,
                        label: default.map_or_else(
                            || "Use module default".into(),
                            |value| format!("Use module default ({})", choice_text(value)),
                        ),
                    }];
                    choices.extend(values.iter().map(|value| Choice {
                        value: Some(value.clone()),
                        label: choice_text(value).into(),
                    }));
                    self.modal = Some(Modal::Pick(Picker {
                        target: PickerTarget::Parameter(row, parameter),
                        title: option.title.clone(),
                        choices,
                        cursor,
                        offset: 0,
                    }));
                }
            }
            Some(Control::Page(page)) => {
                self.change_page(page);
            }
            Some(Control::ShowPlan) => {
                if let Some(plan) = &state.plan {
                    self.modal = Some(Modal::Notice {
                        title: "Installation plan",
                        text: plan.steps.join("\n\n"),
                        offset: 0,
                    });
                }
            }
            Some(Control::Confirm(action)) => {
                let word = match &action {
                    Action::ApplyInstall => match &state.plan {
                        Some(plan) => confirmation_token(plan),
                        None => return Outcome::Continue,
                    },
                    Action::EditPartitions(disk) => format!("PARTITION {disk}"),
                    Action::Calamares => "CALAMARES".into(),
                    _ => return Outcome::Continue,
                };
                self.modal = Some(Modal::Edit(Editor {
                    text: String::new(),
                    cursor: 0,
                    target: EditTarget::Confirm(action, word),
                    error: None,
                }));
            }
            Some(Control::Send(action)) => return Outcome::Execute(action),
            None => {}
        }
        Outcome::Continue
    }

    fn modal_event(&mut self, state: &mut Session, modal: Modal, event: Event) -> Outcome {
        let event = match event {
            Event::Mouse(mouse) if mouse.event == MouseEvent::LEFT => {
                if self
                    .cancel_hit
                    .is_some_and(|area| area.contains(mouse.x, mouse.y))
                {
                    return Outcome::Continue;
                }
                if self
                    .accept_hit
                    .is_some_and(|area| area.contains(mouse.x, mouse.y))
                {
                    Event::Key(13)
                } else {
                    Event::Mouse(mouse)
                }
            }
            event => event,
        };
        match modal {
            Modal::Edit(mut editor) => {
                match event {
                    Event::Key(27) => return Outcome::Continue,
                    Event::Key(10 | 13) => {
                        let result = match editor.target.clone() {
                            EditTarget::Field(field) => self.set_field(state, field, &editor.text),
                            EditTarget::Confirm(action, word) if editor.text == word => {
                                return Outcome::Execute(action);
                            }
                            EditTarget::Confirm(_, word) => {
                                Err(format!("Type {word} exactly to continue."))
                            }
                        };
                        match result {
                            Ok(()) => return Outcome::Continue,
                            Err(err) => editor.error = Some(err),
                        }
                    }
                    Event::Key(8 | 127) => {
                        if let Some(pos) = editor.previous() {
                            editor.text.drain(pos..editor.cursor);
                            editor.cursor = pos;
                        }
                    }
                    Event::Key(KEY_LEFT) => editor.cursor = editor.previous().unwrap_or(0),
                    Event::Key(KEY_RIGHT) => {
                        if let Some(ch) = editor.text[editor.cursor..].chars().next() {
                            editor.cursor += ch.len_utf8();
                        }
                    }
                    Event::Key(21) => {
                        editor.text.clear();
                        editor.cursor = 0;
                    }
                    Event::Key(18) => {
                        if state.mode == Mode::Configure
                            && let EditTarget::Field(field @ (Field::Username | Field::Hostname)) =
                                editor.target
                        {
                            if let Err(error) = self.set_field(state, field, "") {
                                editor.error = Some(error);
                            } else {
                                return Outcome::Continue;
                            }
                        }
                        if let EditTarget::Field(Field::Parameter(row, parameter)) = editor.target {
                            reset_parameter(state, row, parameter);
                            return Outcome::Continue;
                        }
                    }
                    Event::Text(text) => {
                        let text = text
                            .chars()
                            .filter_map(|ch| {
                                if ch == '\n' || ch == '\r' || ch == '\t' {
                                    Some(' ')
                                } else if ch.is_control() {
                                    None
                                } else {
                                    Some(ch)
                                }
                            })
                            .take(4096)
                            .collect::<String>();
                        if editor.text.len() + text.len() <= 16384 {
                            editor.text.insert_str(editor.cursor, &text);
                            editor.cursor += text.len();
                        }
                    }
                    _ => {}
                }
                self.modal = Some(Modal::Edit(editor));
            }
            Modal::Pick(mut picker) => {
                let select = match event {
                    Event::Key(27) => return Outcome::Continue,
                    Event::Key(18) => {
                        if let PickerTarget::Parameter(row, parameter) = picker.target {
                            reset_parameter(state, row, parameter);
                            return Outcome::Continue;
                        }
                        false
                    }
                    Event::Key(KEY_UP | BACK_TAB) => {
                        picker.cursor = picker.cursor.saturating_sub(1);
                        false
                    }
                    Event::Key(KEY_DOWN | 9) => {
                        picker.cursor = (picker.cursor + 1).min(picker.choices.len() - 1);
                        false
                    }
                    Event::Key(10 | 13) => true,
                    Event::Mouse(mouse) if mouse.event == MouseEvent::LEFT => {
                        if let Some((_, index)) = self
                            .picker_hits
                            .iter()
                            .find(|(area, _)| area.contains(mouse.x, mouse.y))
                        {
                            picker.cursor = *index;
                            true
                        } else {
                            false
                        }
                    }
                    Event::Mouse(mouse) if mouse.event == MouseEvent::WHEELUP => {
                        picker.cursor = picker.cursor.saturating_sub(3);
                        false
                    }
                    Event::Mouse(mouse) if mouse.event == MouseEvent::WHEELDOWN => {
                        picker.cursor = (picker.cursor + 3).min(picker.choices.len() - 1);
                        false
                    }
                    _ => false,
                };
                if select {
                    let choice = picker.choices.swap_remove(picker.cursor);
                    match (picker.target, choice.value) {
                        (PickerTarget::Device(DeviceSlot::PartitionEditor), Some(disk)) => {
                            return self.activate(
                                state,
                                Some(Control::Confirm(Action::EditPartitions(disk))),
                            );
                        }
                        (PickerTarget::Device(slot), Some(device)) => {
                            set_device(&mut state.install.layout, slot, device);
                            state.plan = None;
                        }
                        (PickerTarget::Parameter(row, parameter), Some(value)) => {
                            if let Err(error) =
                                self.set_field(state, Field::Parameter(row, parameter), &value)
                            {
                                self.failure("Invalid selection", error);
                            }
                        }
                        (PickerTarget::Parameter(row, parameter), None) => {
                            reset_parameter(state, row, parameter);
                        }
                        (PickerTarget::Device(_), None) => {}
                    }
                } else {
                    self.modal = Some(Modal::Pick(picker));
                }
            }
            Modal::Notice {
                title,
                text,
                mut offset,
            } => {
                match event {
                    Event::Key(27 | 10 | 13) => return Outcome::Continue,
                    Event::Key(KEY_UP | PAGE_UP) => offset = offset.saturating_sub(1),
                    Event::Key(KEY_DOWN | PAGE_DOWN) => offset = offset.saturating_add(1),
                    Event::Mouse(mouse) if mouse.event == MouseEvent::WHEELUP => {
                        offset = offset.saturating_sub(1)
                    }
                    Event::Mouse(mouse) if mouse.event == MouseEvent::WHEELDOWN => {
                        offset = offset.saturating_add(1)
                    }
                    _ => {}
                }
                self.modal = Some(Modal::Notice {
                    title,
                    text,
                    offset,
                });
            }
        }
        Outcome::Continue
    }

    fn field(&self, state: &Session, field: Field) -> String {
        match field {
            Field::Source(i) => state.repositories[i].repository.clone(),
            Field::Reference(i) => state.repositories[i].reference.clone(),
            Field::Search => self.search.clone(),
            Field::Parameter(row, parameter) => {
                let module = &state.modules[row];
                let option = &module.module.manifest.options[parameter];
                module
                    .values
                    .get(&option_key(option))
                    .or(option.default_for(target_platform(state.mode)))
                    .map(|value| option_text(option.kind, value))
                    .unwrap_or_default()
            }
            Field::Flake => state.flake.clone(),
            Field::Selector => state.selector.clone(),
            Field::Hostname | Field::Username if state.mode == Mode::Configure => {
                state.identity_value(field).0.unwrap_or_default().into()
            }
            Field::Hostname => state.install.hostname.clone(),
            Field::Username => state.install.username.clone(),
            Field::Timezone => state.install.timezone.clone(),
            Field::Locale => state.install.locale.clone(),
            Field::Keymap => state.install.keymap.clone(),
            Field::Desktop => state.install.desktop.clone().unwrap_or_default(),
            Field::Swap => state.install.swap_gib.to_string(),
            Field::Target => match &state.install.layout {
                DiskLayout::Mounted { target } => target.display().to_string(),
                _ => String::new(),
            },
        }
    }

    fn set_field(&mut self, state: &mut Session, field: Field, value: &str) -> Result<(), String> {
        let entered = value;
        let value = entered.trim();
        if value.is_empty()
            && matches!(
                field,
                Field::Source(_) | Field::Timezone | Field::Locale | Field::Keymap
            )
        {
            return Err("This field is required.".into());
        }
        match field {
            Field::Source(i) => {
                self.catalog_dirty |= state.repositories[i].repository != value;
                state.repositories[i].repository = value.into();
            }
            Field::Reference(i) => {
                self.catalog_dirty |= state.repositories[i].reference != value;
                state.repositories[i].reference = value.into();
            }
            Field::Search => {
                self.search = value.into();
                self.focus = 0;
                self.offset = 0;
                return Ok(());
            }
            Field::Parameter(row, parameter) => {
                let module = &mut state.modules[row];
                let option = &module.module.manifest.options[parameter];
                let input = if option.kind == OptionKind::String {
                    entered
                } else {
                    value
                };
                let value = parse_option(option.kind, input)?;
                if !option.accepts(&value) {
                    return Err("Choose a value from the list.".into());
                }
                module.values.insert(option_key(option), value);
            }
            Field::Flake => {
                if value.starts_with('-') {
                    return Err("A flake reference cannot start with a hyphen.".into());
                }
                state.flake = value.into();
            }
            Field::Selector => {
                if !PathBuf::from(value).is_absolute() {
                    return Err("Enter an absolute selector path.".into());
                }
                if state.selector != value {
                    state.selector = value.into();
                    state.flake.clear();
                    state.detected_flake.clear();
                    state.identity = Identity::default();
                    state.saved_identity = Identity::default();
                    state.inherited_identity = Identity::default();
                    state.suggested_identity = Identity::default();
                    self.catalog_dirty = true;
                }
            }
            Field::Hostname => {
                if state.mode == Mode::Configure {
                    if !value.is_empty() {
                        validate_hostname(value)?;
                    }
                    state.identity.hostname = (!value.is_empty()
                        && state.inherited_identity.hostname.as_deref() != Some(value))
                    .then(|| value.into());
                } else {
                    validate_hostname(value)?;
                    state.install.hostname = value.into();
                }
            }
            Field::Username => {
                if state.mode == Mode::Configure {
                    if !value.is_empty() {
                        validate_username(value, state.target().platform == Platform::Darwin)?;
                    }
                    state.identity.username = (!value.is_empty()
                        && state.inherited_identity.username.as_deref() != Some(value))
                    .then(|| value.into());
                } else {
                    validate_username(value, false)?;
                    state.install.username = value.into();
                }
            }
            Field::Timezone => state.install.timezone = value.into(),
            Field::Locale => state.install.locale = value.into(),
            Field::Keymap => state.install.keymap = value.into(),
            Field::Desktop => {
                if !matches!(
                    value,
                    "" | "none" | "gnome" | "plasma" | "xfce" | "niri" | "hyprland"
                ) {
                    return Err("Use none, gnome, plasma, xfce, niri or hyprland.".into());
                }
                state.install.desktop = (!matches!(value, "" | "none")).then(|| value.into());
            }
            Field::Swap => {
                state.install.swap_gib = value
                    .parse()
                    .map_err(|_| "Enter a whole number of GiB (0 for no swap).")?
            }
            Field::Target => {
                let target = PathBuf::from(value);
                if !target.is_absolute() {
                    return Err("Enter an absolute mount path.".into());
                }
                state.install.layout = DiskLayout::Mounted { target };
            }
        }
        state.plan = None;
        Ok(())
    }
}

impl Editor {
    fn previous(&self) -> Option<usize> {
        self.text[..self.cursor]
            .char_indices()
            .next_back()
            .map(|(index, _)| index)
    }
}

fn change_setting(request: &mut InstallRequest, setting: Setting) {
    match setting {
        Setting::Layout => {
            request.layout = match request.layout {
                DiskLayout::Erase { .. } => {
                    request.boot_mode = BootMode::Uefi;
                    DiskLayout::Alongside {
                        disk: String::new(),
                        esp: None,
                    }
                }
                DiskLayout::Alongside { .. } => DiskLayout::Partitions {
                    root: String::new(),
                    esp: None,
                    boot_disk: None,
                    format_root: false,
                    format_esp: false,
                },
                DiskLayout::Partitions { .. } => DiskLayout::Mounted {
                    target: PathBuf::from("/mnt"),
                },
                DiskLayout::Mounted { .. } => {
                    request.boot_mode = if std::path::Path::new("/sys/firmware/efi").is_dir() {
                        BootMode::Uefi
                    } else {
                        BootMode::Bios
                    };
                    request.swap_gib = 0;
                    DiskLayout::InPlace
                }
                DiskLayout::InPlace => DiskLayout::Erase {
                    disk: String::new(),
                },
            };
            request.encrypt = false;
        }
        Setting::Boot => {
            request.boot_mode = match request.boot_mode {
                BootMode::Uefi => BootMode::Bios,
                BootMode::Bios => BootMode::Uefi,
            }
        }
        Setting::Filesystem => {
            request.filesystem = match request.filesystem {
                FileSystem::Ext4 => FileSystem::Btrfs,
                FileSystem::Btrfs => FileSystem::Xfs,
                FileSystem::Xfs => FileSystem::Ext4,
            }
        }
        Setting::Encryption => request.encrypt = !request.encrypt,
        Setting::Network => request.network_manager = !request.network_manager,
        Setting::FormatRoot => {
            if let DiskLayout::Partitions { format_root, .. } = &mut request.layout {
                *format_root = !*format_root;
            }
        }
        Setting::FormatEsp => {
            if let DiskLayout::Partitions { format_esp, .. } = &mut request.layout {
                *format_esp = !*format_esp;
            }
        }
    }
}

fn device_choices(devices: &[BlockDevice], layout: &DiskLayout, slot: DeviceSlot) -> Vec<Choice> {
    fn visit(devices: &[BlockDevice], slot: DeviceSlot, choices: &mut Vec<Choice>) {
        for device in devices {
            let target = match slot {
                DeviceSlot::Disk | DeviceSlot::BootDisk | DeviceSlot::PartitionEditor => {
                    device.kind == "disk"
                }
                DeviceSlot::Root => ["part", "crypt", "lvm"].contains(&device.kind.as_str()),
                DeviceSlot::Esp => device.kind == "part",
                DeviceSlot::AlongsideEsp => {
                    device.kind == "part" && device.fstype.as_deref() == Some("vfat")
                }
            };
            if target {
                let size = format!("{:.1} GiB", device.size as f64 / 1024.0 / 1024.0 / 1024.0);
                let mut label = format!(
                    "{} · {size} · {}",
                    device.path,
                    device.fstype.as_deref().unwrap_or(&device.kind)
                );
                if !device.mountpoints.is_empty() {
                    label.push_str(&format!(" · {}", device.mountpoints.join(", ")));
                }
                choices.push(Choice {
                    value: Some(device.path.clone()),
                    label,
                });
            }
            visit(&device.children, slot, choices);
        }
    }
    let mut choices = Vec::new();
    if let (DiskLayout::Alongside { disk, .. }, DeviceSlot::AlongsideEsp) = (layout, slot) {
        choices.push(Choice {
            value: Some(String::new()),
            label: "Create a dedicated EFI partition".into(),
        });
        if let Some(target) = devices.iter().find(|device| device.path == *disk) {
            visit(&target.children, slot, &mut choices);
        }
    } else {
        visit(devices, slot, &mut choices);
    }
    choices
}

fn set_device(layout: &mut DiskLayout, slot: DeviceSlot, value: String) {
    match (layout, slot) {
        (DiskLayout::Erase { disk }, DeviceSlot::Disk) => *disk = value,
        (DiskLayout::Alongside { disk, esp }, DeviceSlot::Disk) => {
            if *disk != value {
                *disk = value;
                *esp = None;
            }
        }
        (DiskLayout::Alongside { esp, .. }, DeviceSlot::AlongsideEsp) => {
            *esp = (!value.is_empty()).then_some(value);
        }
        (DiskLayout::Partitions { root, .. }, DeviceSlot::Root) => *root = value,
        (DiskLayout::Partitions { esp, .. }, DeviceSlot::Esp) => *esp = Some(value),
        (DiskLayout::Partitions { boot_disk, .. }, DeviceSlot::BootDisk) => {
            *boot_disk = Some(value)
        }
        _ => {}
    }
}

impl Ui {
    fn render(&mut self, frame: &mut FrameBuffer, state: &Session) {
        self.hits.clear();
        self.picker_hits.clear();
        self.nav_hits.clear();
        self.exit_hit = None;
        self.accept_hit = None;
        self.cancel_hit = None;
        frame.begin_frame();
        let width = i32::from(frame.width());
        let height = i32::from(frame.height());
        fill(
            frame,
            Area {
                x: 0,
                y: 0,
                w: width,
                h: height,
            },
            BG,
        );
        if width < 44 || height < 14 {
            line(
                frame,
                0,
                0,
                width,
                "Resize to at least 44 × 14. Esc exits.",
                TEXT,
                BG,
            );
            return;
        }

        let product = match state.mode {
            Mode::Install => "NixOS Installer",
            Mode::Configure => "NixOS Configuration",
        };
        line(frame, 1, 3, width - 6, &format!("◆  {product}"), BLUE, BG);
        let count = format!("{} selected", selected_count(state));
        line(
            frame,
            1,
            width - count.len() as i32 - 3,
            count.len() as i32,
            &count,
            GREEN,
            BG,
        );
        let sidebar = if width >= 82 { 24 } else { 0 };
        let content = Area {
            x: 2 + sidebar,
            y: 3,
            w: width - 4 - sidebar,
            h: height - 6,
        };
        if sidebar != 0 {
            let rail = Area {
                x: 2,
                y: 3,
                w: sidebar - 1,
                h: height - 6,
            };
            box_panel(frame, rail, "WORKFLOW");
            for (i, page) in pages(state.mode).iter().enumerate() {
                let active = i == self.page;
                let mark = if active {
                    '›'
                } else if i < self.page {
                    '✓'
                } else {
                    '·'
                };
                let y = rail.y + 2 + i as i32 * 2;
                if y >= rail.y + rail.h - 1 {
                    break;
                }
                let background = if active { FOCUS } else { PANEL };
                fill(
                    frame,
                    Area {
                        x: rail.x + 1,
                        y,
                        w: rail.w - 2,
                        h: 1,
                    },
                    background,
                );
                line(
                    frame,
                    y,
                    rail.x + 2,
                    rail.w - 4,
                    &format!("{mark} {}", page.title()),
                    if active { BLUE } else { MUTED },
                    background,
                );
                self.nav_hits.push((
                    Area {
                        x: rail.x + 1,
                        y,
                        w: rail.w - 2,
                        h: 1,
                    },
                    i,
                ));
            }
        }
        let page = self.screen(state);
        box_panel(frame, content, page.title());
        let mut top = content.y + 2;
        if sidebar == 0 {
            line(
                frame,
                top,
                content.x + 2,
                content.w - 4,
                &format!("Step {} of {}", self.page + 1, pages(state.mode).len()),
                BLUE,
                PANEL,
            );
            top += 1;
        }
        let subtitle = if page == Page::Modules {
            format!("{} · {}", state.target().system, page.subtitle())
        } else if page == Page::System && state.mode == Mode::Configure {
            "Edit selector names. Ctrl+R restores the inherited value.".into()
        } else {
            page.subtitle().into()
        };
        line(
            frame,
            top,
            content.x + 2,
            content.w - 4,
            &subtitle,
            MUTED,
            PANEL,
        );
        top += 2;
        let rows = self.rows(state);
        self.visible_rows = (content.y + content.h - 2 - top).max(1) as usize;
        self.focus = self.focus.min(rows.len().saturating_sub(1));
        keep_visible(self.focus, &mut self.offset, self.visible_rows, rows.len());
        for (index, row) in rows
            .iter()
            .enumerate()
            .skip(self.offset)
            .take(self.visible_rows)
        {
            let y = top + (index - self.offset) as i32;
            let focused = index == self.focus;
            let background = if focused { FOCUS } else { PANEL };
            let area = Area {
                x: content.x + 1,
                y,
                w: content.w - 2,
                h: 1,
            };
            fill(frame, area, background);
            let marker = if focused { '›' } else { ' ' };
            line(
                frame,
                y,
                area.x + 1,
                area.w - 2,
                &format!("{marker} {}", row.text),
                row.color,
                background,
            );
            if row.control.is_some() {
                self.hits.push((area, index));
            }
        }
        if rows.len() > self.visible_rows {
            let bounds = format!(
                "{}–{} of {}",
                self.offset + 1,
                (self.offset + self.visible_rows).min(rows.len()),
                rows.len()
            );
            line(
                frame,
                content.y + content.h - 2,
                content.x + 2,
                content.w - 4,
                &bounds,
                MUTED,
                PANEL,
            );
        }

        let nav_y = height - 3;
        if self.page > 0 {
            self.nav_hits
                .push((button(frame, nav_y, 3, " ← Back "), self.page - 1));
        }
        if self.page + 1 < pages(state.mode).len() {
            self.nav_hits.push((
                button(frame, nav_y, width - 16, " Continue → "),
                self.page + 1,
            ));
        }
        self.exit_hit = Some(button(frame, nav_y, width / 2 - 5, " Esc  Exit "));
        let bottom = if self.loading {
            ("Loading module sources…   Esc Exit".into(), BLUE)
        } else if let Some((message, success)) = &self.message {
            (
                format!(
                    "{} {}",
                    if *success { '✓' } else { '!' },
                    message.split_whitespace().collect::<Vec<_>>().join(" ")
                ),
                if *success { GREEN } else { RED },
            )
        } else {
            (
                "↑↓ Move   Enter Open   Ctrl+N/P Step   Esc Exit".into(),
                MUTED,
            )
        };
        line(frame, height - 2, 3, width - 6, &bottom.0, bottom.1, BG);
        if let Some(modal) = &mut self.modal {
            let w = (width - 8).min(76);
            let h = match modal {
                Modal::Edit(_) => 9.min(height - 2),
                Modal::Pick(_) => 15.min(height - 2),
                Modal::Notice { .. } => 13.min(height - 2),
            };
            let area = Area {
                x: (width - w) / 2,
                y: (height - h) / 2,
                w,
                h,
            };
            match modal {
                Modal::Edit(editor) => {
                    render_editor(frame, area, editor, state);
                    self.accept_hit = Some(button(
                        frame,
                        area.y + area.h - 2,
                        area.x + area.w - 11,
                        " Accept ",
                    ));
                    self.cancel_hit =
                        Some(button(frame, area.y + area.h - 2, area.x + 2, " Cancel "));
                }
                Modal::Pick(picker) => {
                    render_picker(frame, area, picker, &mut self.picker_hits);
                    self.cancel_hit = Some(button(
                        frame,
                        area.y + area.h - 2,
                        area.x + area.w - 11,
                        " Cancel ",
                    ));
                }
                Modal::Notice {
                    title,
                    text,
                    offset,
                } => {
                    render_notice(frame, area, title, text, offset);
                    self.cancel_hit = Some(button(
                        frame,
                        area.y + area.h - 2,
                        area.x + area.w - 11,
                        " Close ",
                    ));
                }
            }
        }
    }
}

fn render_editor(frame: &mut FrameBuffer, area: Area, editor: &Editor, state: &Session) {
    let (title, help) = match &editor.target {
        EditTarget::Field(Field::Parameter(row, parameter)) => {
            let option = &state.modules[*row].module.manifest.options[*parameter];
            (
                option.title.as_str(),
                format!("{} · Ctrl+R resets · Enter saves", option_hint(option.kind)),
            )
        }
        EditTarget::Field(field) => (
            field_title(*field),
            "Enter saves · Esc cancels · Ctrl+U clears".to_string(),
        ),
        EditTarget::Confirm(_, word) => (
            "Confirm operation",
            format!("Type {word} to continue. Esc cancels."),
        ),
    };
    box_panel(frame, area, title);
    line(
        frame,
        area.y + 2,
        area.x + 2,
        area.w - 4,
        &help,
        if matches!(editor.target, EditTarget::Confirm(..)) {
            RED
        } else {
            MUTED
        },
        PANEL,
    );
    let input = Area {
        x: area.x + 2,
        y: area.y + 4,
        w: area.w - 4,
        h: 1,
    };
    let before = &editor.text[..editor.cursor];
    let after = &editor.text[editor.cursor..];
    let first = before
        .char_indices()
        .nth_back(((input.w - 6) / 2).max(0) as usize)
        .map(|(i, _)| i)
        .unwrap_or(0);
    let rendered = format!(
        "{}{}▏{}",
        if first > 0 { "…" } else { "" },
        &before[first..],
        after
    );
    fill(frame, input, FOCUS);
    line(
        frame,
        input.y,
        input.x + 1,
        input.w - 2,
        &rendered,
        TEXT,
        FOCUS,
    );
    if let Some(error) = &editor.error {
        line(frame, area.y + 6, area.x + 2, area.w - 4, error, RED, PANEL);
    } else if let EditTarget::Field(Field::Parameter(row, parameter)) = editor.target {
        line(
            frame,
            area.y + 6,
            area.x + 2,
            area.w - 4,
            &state.modules[row].module.manifest.options[parameter].description,
            MUTED,
            PANEL,
        );
    }
}

fn render_picker(
    frame: &mut FrameBuffer,
    area: Area,
    picker: &mut Picker,
    hits: &mut Vec<(Area, usize)>,
) {
    box_panel(frame, area, &picker.title);
    let visible = (area.h - 4).max(1) as usize;
    keep_visible(
        picker.cursor,
        &mut picker.offset,
        visible,
        picker.choices.len(),
    );
    for (index, choice) in picker
        .choices
        .iter()
        .enumerate()
        .skip(picker.offset)
        .take(visible)
    {
        let active = picker.cursor == index;
        let background = if active { FOCUS } else { PANEL };
        let hit = Area {
            x: area.x + 1,
            y: area.y + 2 + (index - picker.offset) as i32,
            w: area.w - 2,
            h: 1,
        };
        fill(frame, hit, background);
        let text = format!("{} {}", if active { '›' } else { ' ' }, choice.label);
        line(frame, hit.y, hit.x + 1, hit.w - 2, &text, TEXT, background);
        hits.push((hit, index));
    }
    line(
        frame,
        area.y + area.h - 2,
        area.x + 2,
        area.w - 14,
        if matches!(picker.target, PickerTarget::Parameter(..)) {
            "Enter selects · Ctrl+R resets · Esc cancels"
        } else {
            "Enter selects · Esc cancels"
        },
        MUTED,
        PANEL,
    );
}

fn field_title(field: Field) -> &'static str {
    match field {
        Field::Source(_) => "Repository source",
        Field::Reference(_) => "Git reference",
        Field::Search => "Filter modules",
        Field::Parameter(..) => "Module parameter",
        Field::Flake => "Host flake override (empty uses selector)",
        Field::Selector => "Absolute selector path",
        Field::Hostname => "Hostname",
        Field::Username => "Username",
        Field::Timezone => "Timezone",
        Field::Locale => "Locale",
        Field::Keymap => "Console keymap",
        Field::Desktop => "Desktop environment",
        Field::Swap => "Swap size in GiB",
        Field::Target => "Installation mount point",
    }
}

fn keep_visible(focus: usize, offset: &mut usize, visible: usize, count: usize) {
    *offset = (*offset).min(count.saturating_sub(visible));
    if focus < *offset {
        *offset = focus;
    } else if focus >= *offset + visible {
        *offset = focus + 1 - visible;
    }
}

fn fill(frame: &mut FrameBuffer, area: Area, background: Rgb) {
    let style = CellStyle::background(background);
    for y in area.y..area.y + area.h {
        for x in area.x..area.x + area.w {
            frame.put_styled(y, x, ' ', style);
        }
    }
}

fn box_panel(frame: &mut FrameBuffer, area: Area, title: &str) {
    fill(frame, area, PANEL);
    let style = CellStyle::new(Some(LINE), Some(PANEL));
    let bottom = area.y + area.h - 1;
    let right = area.x + area.w - 1;
    for x in area.x + 1..right {
        frame.put_styled(area.y, x, '─', style);
        frame.put_styled(bottom, x, '─', style);
    }
    for y in area.y + 1..bottom {
        frame.put_styled(y, area.x, '│', style);
        frame.put_styled(y, right, '│', style);
    }
    for (y, x, corner) in [
        (area.y, area.x, '╭'),
        (area.y, right, '╮'),
        (bottom, area.x, '╰'),
        (bottom, right, '╯'),
    ] {
        frame.put_styled(y, x, corner, style);
    }
    line(
        frame,
        area.y,
        area.x + 2,
        area.w - 4,
        &format!(" {title} "),
        BLUE,
        PANEL,
    );
}

fn line(
    frame: &mut FrameBuffer,
    y: i32,
    x: i32,
    width: i32,
    text: &str,
    color: Rgb,
    background: Rgb,
) {
    let style = CellStyle::new(Some(color), Some(background));
    let mut position = x;
    let end = x + width;
    for character in text.chars().filter(|character| !character.is_control()) {
        if position >= end {
            break;
        }
        frame.put_styled(y, position, character, style);
        let wide = frame
            .get(y, position + 1)
            .is_some_and(|cell| cell.continuation);
        if wide && position + 1 >= end {
            frame.put_styled(y, position, '…', style);
            break;
        }
        position += if wide { 2 } else { 1 };
    }
}

fn layer_name(layer: Layer) -> &'static str {
    match layer {
        Layer::Nixos => "nixos",
        Layer::Darwin => "darwin",
        Layer::Home => "home",
    }
}

pub fn option_key(option: &ManifestOption) -> String {
    format!("{}.{}", layer_name(option.layer), option.path.join("."))
}

impl ModuleRow {
    fn boolean(&self, index: usize, platform: Platform) -> Option<bool> {
        let option = &self.module.manifest.options[index];
        self.values
            .get(&option_key(option))
            .or(option.default_for(platform))
            .and_then(Value::as_bool)
    }

    fn toggle(&mut self, targets: &[usize], platform: Platform) {
        let enabled = !targets
            .iter()
            .all(|&i| self.boolean(i, platform) == Some(true));
        for &index in targets {
            let option = &self.module.manifest.options[index];
            let key = option_key(option);
            // Omit overrides which match the current target's default.
            if option.default_for(platform).and_then(Value::as_bool) == Some(enabled) {
                self.values.remove(&key);
            } else {
                self.values.insert(key, Value::Bool(enabled));
            }
        }
    }
}

fn reset_parameter(state: &mut Session, row: usize, parameter: usize) {
    let module = &mut state.modules[row];
    module
        .values
        .remove(&option_key(&module.module.manifest.options[parameter]));
    state.plan = None;
}

fn choice_text(value: &str) -> &str {
    if value.is_empty() { "\"\"" } else { value }
}

fn mark_available(mut row: Row, available: bool) -> Row {
    if !available {
        row.text = format!("Unavailable · {}", row.text.trim_start());
        row.color = MUTED;
    }
    row
}

fn mark_parameter_availability(mut row: Row, available: bool, target: &Target) -> Row {
    if !available {
        row.text
            .push_str(&format!(" — unavailable on {}", target.system));
        row.color = MUTED;
    }
    row
}

fn package_groups(manifest: &crate::catalog::Manifest) -> BTreeMap<&[String], Vec<usize>> {
    let mut packages = BTreeMap::<&[String], Vec<usize>>::new();
    for (index, option) in manifest
        .options
        .iter()
        .enumerate()
        .filter(|(_, option)| is_package_switch(option))
    {
        packages.entry(&option.path).or_default().push(index);
    }
    packages
}

fn available_members(
    manifest: &crate::catalog::Manifest,
    members: &[usize],
    target: &Target,
) -> Vec<usize> {
    members
        .iter()
        .copied()
        .filter(|&index| manifest.option_available_on(&manifest.options[index], target))
        .collect()
}

fn switch_mark(row: &ModuleRow, targets: &[usize], platform: Platform) -> &'static str {
    if targets
        .iter()
        .all(|&i| row.boolean(i, platform) == Some(true))
    {
        "[×]"
    } else if targets
        .iter()
        .all(|&i| row.boolean(i, platform) == Some(false))
    {
        "[ ]"
    } else {
        "[–]"
    }
}

fn parameter_rows(index: usize, row: &ModuleRow, target: &Target) -> Vec<Row> {
    let manifest = &row.module.manifest;
    let mut packages = package_groups(manifest);
    let mut rows = Vec::new();
    for (i, option) in manifest.options.iter().enumerate() {
        if is_package_switch(option) {
            let Some(members) = packages.remove(option.path.as_slice()) else {
                continue;
            };
            let available = available_members(manifest, &members, target);
            let active = !available.is_empty();
            let targets = if active { available } else { members };
            let title = &manifest.options[targets[0]].title;
            rows.push(mark_parameter_availability(
                Row::action(
                    format!("  {} {title}", switch_mark(row, &targets, target.platform)),
                    Control::ToggleParameter(index, targets),
                ),
                active,
                target,
            ));
            continue;
        }
        let active = manifest.option_available_on(option, target);
        let saved = row.values.get(&option_key(option));
        let label = format!("{} · {}", layer_name(option.layer), option.title);
        let setting = if option.kind == OptionKind::Packages {
            if saved.is_none() {
                continue;
            }
            Row::action(
                format!(
                    "  {} · Restore module packages (legacy replacement active)",
                    layer_name(option.layer)
                ),
                Control::ResetParameter(index, i),
            )
        } else if option.kind == OptionKind::Bool {
            Row::action(
                format!("  {} {label}", switch_mark(row, &[i], target.platform)),
                Control::ToggleParameter(index, vec![i]),
            )
        } else if option.choices.is_some() {
            let display = match saved {
                Some(value) if option.accepts(value) => {
                    choice_text(value.as_str().unwrap_or_default()).into()
                }
                Some(value) => format!("{} (unavailable value)", option_text(option.kind, value)),
                None => option
                    .default_for(target.platform)
                    .and_then(Value::as_str)
                    .map_or_else(
                        || "Default".into(),
                        |value| format!("{} (default)", choice_text(value)),
                    ),
            };
            Row::action(
                format!("  {label:<16} {display}  ›"),
                Control::ChooseParameter(index, i),
            )
        } else {
            let display = match (saved, option.default_for(target.platform)) {
                (Some(v), _) => option_text(option.kind, v),
                (None, Some(v)) => format!("{} (default)", option_text(option.kind, v)),
                _ => "Default".into(),
            };
            Row::field(&format!("  {label}"), &display, Field::Parameter(index, i))
        };
        rows.push(mark_parameter_availability(setting, active, target));
    }
    rows
}

fn is_package_switch(option: &ManifestOption) -> bool {
    option.kind == OptionKind::Bool
        && matches!(option.path.as_slice(), [root, _, group, _] if root == "components" && group == "enabledPackages")
}

fn target_platform(mode: Mode) -> Platform {
    if mode == Mode::Install || !cfg!(target_os = "macos") {
        Platform::Linux
    } else {
        Platform::Darwin
    }
}

fn source_mode(mode: SourceMode) -> &'static str {
    match mode {
        SourceMode::Local => "Local / updateable",
        SourceMode::Pinned => "Pinned remote Git",
    }
}

fn option_hint(kind: OptionKind) -> &'static str {
    match kind {
        OptionKind::Bool => "Toggle this switch on the Modules page",
        OptionKind::Integer => "Whole number",
        OptionKind::String => "Text",
        OptionKind::StringList => "JSON string list",
        OptionKind::Packages => "Restore the module packages on the Modules page",
    }
}

fn option_text(kind: OptionKind, value: &Value) -> String {
    match kind {
        OptionKind::String => value.as_str().unwrap_or_default().into(),
        OptionKind::Packages => value
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>()
            .join(", "),
        _ => value.to_string(),
    }
}

fn parse_option(kind: OptionKind, input: &str) -> Result<Value, String> {
    let error = || format!("Expected {}.", option_hint(kind));
    let value = match kind {
        OptionKind::Bool | OptionKind::Packages => return Err(option_hint(kind).into()),
        OptionKind::Integer => Value::from(input.parse::<i64>().map_err(|_| error())?),
        OptionKind::String => Value::String(input.to_owned()),
        OptionKind::StringList => serde_json::from_str(input).map_err(|_| error())?,
    };
    if kind.accepts(&value) {
        Ok(value)
    } else {
        Err(error())
    }
}

fn button(frame: &mut FrameBuffer, y: i32, x: i32, title: &str) -> Area {
    let area = Area {
        x,
        y,
        w: title.chars().count() as i32,
        h: 1,
    };
    line(frame, y, x, area.w, title, BLUE, FOCUS);
    area
}

fn render_notice(frame: &mut FrameBuffer, area: Area, title: &str, text: &str, offset: &mut usize) {
    box_panel(frame, area, title);
    let width = (area.w - 4) as usize;
    let mut content = Vec::new();
    for message in text.lines() {
        let mut current = String::new();
        for word in message.split_whitespace() {
            if !current.is_empty() && current.chars().count() + word.chars().count() + 1 > width {
                content.push(std::mem::take(&mut current));
            }
            if !current.is_empty() {
                current.push(' ');
            }
            current.push_str(word);
        }
        content.push(current);
    }
    let visible = (area.h - 4) as usize;
    *offset = (*offset).min(content.len().saturating_sub(visible));
    for (index, text) in content.iter().skip(*offset).take(visible).enumerate() {
        line(
            frame,
            area.y + 2 + index as i32,
            area.x + 2,
            width as i32,
            text,
            TEXT,
            PANEL,
        );
    }
    if content.len() > visible {
        line(
            frame,
            area.y + area.h - 2,
            area.x + 2,
            area.w - 14,
            "↑↓ Scroll",
            MUTED,
            PANEL,
        );
    }
}

fn executable_in_path(name: &str) -> bool {
    env::var_os("PATH").is_some_and(|path| {
        env::split_paths(&path).any(|dir| {
            fs::metadata(dir.join(name)).is_ok_and(|metadata| {
                metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
            })
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::Manifest;
    use serde_json::json;

    fn session() -> Session {
        Session::new(
            Mode::Configure,
            vec![SourceSpec {
                repository: "/modules".into(),
                reference: "main".into(),
                mode: SourceMode::Local,
            }],
            InstallRequest {
                layout: DiskLayout::Mounted {
                    target: "/mnt".into(),
                },
                boot_mode: BootMode::Uefi,
                filesystem: FileSystem::Ext4,
                encrypt: false,
                swap_gib: 0,
                hostname: "test-host".into(),
                username: "test-user".into(),
                timezone: "UTC".into(),
                locale: "en_US.UTF-8".into(),
                keymap: "us".into(),
                desktop: None,
                network_manager: true,
            },
            "/host".into(),
            "/selector.nix".into(),
        )
    }

    fn frame_text(frame: &FrameBuffer, row: i32) -> String {
        (0..i32::from(frame.width()))
            .filter_map(|x| frame.get(row, x))
            .map(|cell| cell.symbol.as_str())
            .collect()
    }

    #[test]
    fn rebuild_runs_on_first_keyboard_or_mouse_activation() {
        let mut state = session();
        state.detected_flake = std::mem::take(&mut state.flake);
        let mut ui = Ui {
            page: CONFIG_PAGES.len() - 1,
            focus: 1,
            ..Ui::default()
        };
        assert!(matches!(
            ui.event(&mut state, Event::Key(13)),
            Outcome::Execute(Action::Rebuild)
        ));
        assert!(ui.modal.is_none());

        let mut frame = FrameBuffer::new(98, 26);
        ui.render(&mut frame, &state);
        let area = ui.hits.iter().find(|(_, index)| *index == 1).unwrap().0;
        let click = Event::Mouse(Mouse {
            x: area.x as u16,
            y: area.y as u16,
            event: MouseEvent::LEFT,
            ..Mouse::default()
        });
        assert!(matches!(
            ui.event(&mut state, click),
            Outcome::Execute(Action::Rebuild)
        ));
        assert!(ui.modal.is_none());
    }

    #[test]
    fn responsive_frame_handles_mouse_navigation_and_renders_selection() {
        let mut state = session();
        let mut ui = Ui::default();
        let mut frame = FrameBuffer::new(98, 26);
        ui.render(&mut frame, &state);
        assert!(frame_text(&frame, 1).contains("NixOS Configuration"));
        let area = ui.nav_hits.iter().find(|(_, page)| *page == 1).unwrap().0;
        ui.event(
            &mut state,
            Event::Mouse(Mouse {
                x: area.x as u16,
                y: area.y as u16,
                event: MouseEvent::LEFT,
                ..Mouse::default()
            }),
        );
        assert_eq!(ui.screen(&state), Page::Modules);
        frame.resize(56, 18);
        ui.render(&mut frame, &state);
        assert!(frame_text(&frame, 5).contains("Step 2 of 4"));
        assert!(frame_text(&frame, 8).contains("Filter modules"));
    }

    #[test]
    fn configure_system_identity_edits_and_resets_inherited_values() {
        let mut state = session();
        state.inherited_identity = Identity {
            username: Some("original".into()),
            hostname: None,
        };
        state.suggested_identity.hostname = Some("local-node".into());
        let mut ui = Ui {
            page: 2,
            ..Ui::default()
        };
        assert!(
            ui.rows(&state)[0]
                .text
                .contains("local-node (selector unset)")
        );
        assert!(ui.rows(&state)[1].text.contains("original (inherited)"));
        assert!(
            ui.set_field(&mut state, Field::Username, "unsafe name")
                .is_err()
        );
        assert!(
            ui.set_field(&mut state, Field::Hostname, "bad.name")
                .is_err()
        );
        ui.set_field(&mut state, Field::Username, "new-user")
            .unwrap();
        ui.set_field(&mut state, Field::Hostname, "new-node")
            .unwrap();
        assert!(
            ui.review_rows(&state)
                .iter()
                .any(|row| row.text.contains("new-node (override) · change pending"))
        );
        ui.event(&mut state, Event::Key(18));
        assert!(state.identity.hostname.is_none());
        ui.activate(&mut state, Some(Control::Edit(Field::Username)));
        ui.event(&mut state, Event::Key(18));
        assert!(state.identity.is_empty());
        assert_eq!(ui.field(&state, Field::Username), "original");
    }

    #[test]
    fn validates_typed_overrides_and_can_restore_catalog_defaults() {
        let mut state = session();
        state.modules.push(ModuleRow {
            repository: 0,
            selected: true,
            values: BTreeMap::new(),
            module: CatalogModule {
                path: "test".into(),
                manifest: Manifest {
                    schema_version: 1,
                    title: "Example".into(),
                    description: "Example component".into(),
                    layers: BTreeMap::from([(Layer::Home, vec![target_platform(state.mode)])]),
                    systems: None,
                    options: vec![ManifestOption {
                        layer: Layer::Home,
                        platforms: None,
                        systems: None,
                        path: vec!["example".into(), "port".into()],
                        title: "Port".into(),
                        description: "Service port".into(),
                        kind: OptionKind::Integer,
                        choices: None,
                        default: Some(json!(5001)),
                        defaults_by_platform: BTreeMap::new(),
                    }],
                },
            },
        });
        let mut ui = Ui::default();
        assert!(
            ui.set_field(&mut state, Field::Parameter(0, 0), "hello; abort")
                .is_err()
        );
        ui.set_field(&mut state, Field::Parameter(0, 0), "6001")
            .unwrap();
        assert_eq!(state.modules[0].values["home.example.port"], json!(6001));
        ui.activate(&mut state, Some(Control::Edit(Field::Parameter(0, 0))));
        ui.event(&mut state, Event::Key(18));
        assert!(state.modules[0].values.is_empty());
        assert_eq!(ui.field(&state, Field::Parameter(0, 0)), "5001");
    }

    #[test]
    fn theme_picker_supports_keyboard_mouse_and_restoring_default() {
        let mut state = session();
        let key = "home.appearance.theme";
        let manifest = serde_json::from_value(json!({
            "schemaVersion": 1,
            "title": "Example",
            "description": "Example themes",
            "layers": {"home": ["linux", "darwin"]},
            "options": [{"layer": "home", "path": ["appearance", "theme"],
                "title": "Theme", "description": "Color scheme", "type": "string",
                "choices": ["nord", "light", ""], "default": "nord"},
                {"layer": "home", "path": ["appearance", "name"],
                    "title": "Name", "description": "Free text", "type": "string"}]
        }))
        .unwrap();
        state.modules.push(ModuleRow {
            repository: 0,
            selected: true,
            values: BTreeMap::from([(key.into(), json!("removed"))]),
            module: CatalogModule {
                path: "theme".into(),
                manifest,
            },
        });
        let mut ui = Ui {
            page: 1,
            ..Ui::default()
        };
        let mut frame = FrameBuffer::new(98, 26);
        ui.focus = ui
            .rows(&state)
            .iter()
            .position(|row| matches!(row.control, Some(Control::ChooseParameter(0, 0))))
            .unwrap();
        assert!(
            ui.rows(&state)[ui.focus]
                .text
                .contains("removed (unavailable value)")
        );
        assert!(
            ui.set_field(&mut state, Field::Parameter(0, 0), "unknown")
                .is_err()
        );
        ui.event(&mut state, Event::Key(13));
        ui.render(&mut frame, &state);
        assert!((0..26).any(|line| frame_text(&frame, line).contains("Use module default (nord)")));
        assert!((0..26).any(|line| frame_text(&frame, line).contains("light")));
        ui.event(&mut state, Event::Key(KEY_DOWN));
        ui.event(&mut state, Event::Key(KEY_DOWN));
        ui.event(&mut state, Event::Key(13));
        assert_eq!(state.modules[0].values[key], json!("light"));

        ui.event(&mut state, Event::Key(13));
        assert!(matches!(&ui.modal, Some(Modal::Pick(picker)) if picker.cursor == 2));
        ui.event(&mut state, Event::Key(27));
        assert_eq!(state.modules[0].values[key], json!("light"));

        ui.event(&mut state, Event::Key(13));
        ui.render(&mut frame, &state);
        let area = ui
            .picker_hits
            .iter()
            .find(|(_, index)| *index == 3)
            .unwrap()
            .0;
        ui.event(
            &mut state,
            Event::Mouse(Mouse {
                x: area.x as u16,
                y: area.y as u16,
                event: MouseEvent::LEFT,
                ..Mouse::default()
            }),
        );
        assert_eq!(state.modules[0].values[key], json!(""));
        ui.event(&mut state, Event::Key(18));
        assert!(!state.modules[0].values.contains_key(key));
        assert!(ui.rows(&state)[ui.focus].text.contains("nord (default)"));
        assert!(
            ui.rows(&state)
                .iter()
                .any(|row| matches!(row.control, Some(Control::Edit(Field::Parameter(0, 1)))))
        );
        ui.set_field(&mut state, Field::Parameter(0, 1), " custom text ")
            .unwrap();
        assert_eq!(
            state.modules[0].values["home.appearance.name"],
            json!(" custom text ")
        );
    }

    #[test]
    fn legacy_package_override_is_preserved_until_reset() {
        let mut state = session();
        let platform = target_platform(state.mode);
        let option = serde_json::from_value(json!({"layer":"home","path":["example","packages"],
            "title":"Packages","description":"Previous package list","type":"packages"}))
        .unwrap();
        let row = ModuleRow {
            repository: 0,
            selected: true,
            values: BTreeMap::from([("home.example.packages".into(), json!(["jq"]))]),
            module: CatalogModule {
                path: "sample".into(),
                manifest: Manifest {
                    schema_version: 1,
                    title: "Sample".into(),
                    description: "Sample".into(),
                    layers: BTreeMap::from([(Layer::Home, vec![platform])]),
                    systems: None,
                    options: vec![option],
                },
            },
        };
        state.modules.push(row);
        let mut ui = Ui {
            page: 1,
            ..Ui::default()
        };
        ui.focus = ui
            .rows(&state)
            .iter()
            .position(|r| matches!(r.control, Some(Control::ResetParameter(..))))
            .unwrap();
        ui.event(&mut state, Event::Key(13));
        assert!(ui.modal.is_none());
        assert!(state.modules[0].values.is_empty());
        assert!(parameter_rows(0, &state.modules[0], &state.target()).is_empty());
    }

    #[test]
    fn package_switch_toggles_visible_layers_together_without_an_editor() {
        let mut state = session();
        let platform = target_platform(state.mode);
        let system_layer = if platform == Platform::Darwin {
            Layer::Darwin
        } else {
            Layer::Nixos
        };
        let mut package: ManifestOption = serde_json::from_value(json!({
            "layer":"home", "path":["components","sample","enabledPackages","daemon"],
            "type":"bool", "title":"Daemon", "description":"Daemon and its service", "default":true
        }))
        .unwrap();
        let mut options = vec![package.clone()];
        package.layer = system_layer;
        options.push(package);
        options.push(
            serde_json::from_value(json!({"layer":"home","path":["signal"],"title":"Signal",
            "description":"Signal", "type":"bool", "default":false}))
            .unwrap(),
        );
        state.modules.push(ModuleRow {
            repository: 0,
            selected: true,
            values: BTreeMap::new(),
            module: CatalogModule {
                path: "sample".into(),
                manifest: Manifest {
                    schema_version: 1,
                    title: "Sample".into(),
                    description: "Sample".into(),
                    layers: BTreeMap::from([
                        (Layer::Home, vec![platform]),
                        (system_layer, vec![platform]),
                    ]),
                    systems: None,
                    options,
                },
            },
        });
        let mut ui = Ui {
            page: 1,
            ..Ui::default()
        };
        let rows = ui.rows(&state);
        assert_eq!(rows.iter().filter(|r| r.text.contains("Daemon")).count(), 1);
        ui.focus = rows
            .iter()
            .position(|r| r.text.contains("[×] Daemon"))
            .unwrap();
        ui.event(&mut state, Event::Text(" ".into()));
        assert!(ui.modal.is_none());
        assert_eq!(state.modules[0].values.len(), 2);
        assert!(
            state.modules[0]
                .values
                .values()
                .all(|v| v == &Value::Bool(false))
        );
        let mut frame = FrameBuffer::new(98, 26);
        ui.render(&mut frame, &state);
        let area = ui.hits.iter().find(|(_, i)| *i == ui.focus).unwrap().0;
        ui.event(
            &mut state,
            Event::Mouse(Mouse {
                x: area.x as u16,
                y: area.y as u16,
                event: MouseEvent::LEFT,
                ..Mouse::default()
            }),
        );
        assert!(state.modules[0].values.is_empty());
        ui.focus = ui
            .rows(&state)
            .iter()
            .position(|r| r.text.contains("[ ] home · Signal"))
            .unwrap();
        ui.event(&mut state, Event::Key(13));
        assert_eq!(state.modules[0].values["home.signal"], true);
        ui.event(&mut state, Event::Key(18));
        assert!(state.modules[0].values.is_empty());
    }

    #[test]
    fn grouped_package_uses_target_defaults_to_display_toggle_and_reset() {
        let json = json!({
            "schemaVersion": 1, "title": "Graphics", "description": "Graphics tools",
            "layers": {"home": ["linux", "darwin"], "darwin": ["darwin"]},
            "options": [
                {"layer": "home", "path": ["components", "graphics", "enabledPackages", "gimp3"],
                 "type": "bool", "title": "GIMP", "description": "Default GIMP setting",
                 "default": true, "defaultsByPlatform": {"darwin": false}},
                {"layer": "darwin", "path": ["components", "graphics", "enabledPackages", "gimp3"],
                 "type": "bool", "title": "GIMP", "description": "Darwin GIMP setting",
                 "default": false}
            ]
        });
        let module = ModuleRow {
            repository: 0,
            selected: true,
            values: BTreeMap::new(),
            module: CatalogModule {
                path: "graphics".into(),
                manifest: serde_json::from_value(json).unwrap(),
            },
        };

        for (platform, initially_enabled, members) in [
            (Platform::Linux, true, vec![0]),
            (Platform::Darwin, false, vec![0, 1]),
        ] {
            let target = Target::new(platform, "aarch64");
            let mut module = module.clone();
            let rows = parameter_rows(0, &module, &target);
            assert_eq!(rows.len(), 1);
            assert_eq!(
                rows[0].text.trim(),
                if initially_enabled {
                    "[×] GIMP"
                } else {
                    "[ ] GIMP"
                }
            );
            assert!(
                matches!(&rows[0].control, Some(Control::ToggleParameter(0, options)) if options == &members)
            );
            module.toggle(&members, platform);
            assert_eq!(module.values.len(), members.len());
            assert_eq!(
                module.values["home.components.graphics.enabledPackages.gimp3"],
                !initially_enabled
            );
            assert_eq!(
                parameter_rows(0, &module, &target)[0].text.trim(),
                if initially_enabled {
                    "[ ] GIMP"
                } else {
                    "[×] GIMP"
                }
            );
            module.toggle(&members, platform);
            assert!(module.values.is_empty());
        }

        let mut state = session();
        let target = state.target();
        state.modules.push(module);
        let mut ui = Ui {
            page: 1,
            ..Ui::default()
        };
        ui.focus = ui
            .rows(&state)
            .iter()
            .position(|row| row.text.contains("] GIMP"))
            .unwrap();
        let before = ui.rows(&state)[ui.focus].text.clone();
        ui.event(&mut state, Event::Key(13));
        assert!(!state.modules[0].values.is_empty());
        ui.event(&mut state, Event::Key(18));
        assert!(state.modules[0].values.is_empty());
        assert_eq!(
            parameter_rows(0, &state.modules[0], &target)[0].text,
            before
        );
    }

    #[test]
    fn unavailable_modules_and_duplicate_package_layers_remain_selectable() {
        let mut state = session();
        let target = state.target();
        let foreign = if target.platform == Platform::Linux {
            "darwin"
        } else {
            "linux"
        };
        let layer = if target.platform == Platform::Linux {
            "darwin"
        } else {
            "nixos"
        };
        let mut modules = vec![];
        for (path, manifest) in [
            (
                "tool",
                json!({
                    "schemaVersion": 1, "title": "Toolbox", "description": "Applications",
                    "layers": {"home": ["linux", "darwin"], layer: [foreign]},
                    "options": [
                        {"layer":"home","path":["components","tool","enabledPackages","curl"],
                            "type":"bool","title":"curl","description":"Available tool","default":true},
                        {"layer":layer,"path":["components","tool","enabledPackages","curl"],
                            "type":"bool","title":"curl","description":"Foreign layer","default":true},
                        {"layer":"home","path":["components","tool","enabledPackages","intel-tool"],
                            "type":"bool","title":"Intel tool","description":"System-specific tool",
                            "systems":[format!("other-{foreign}")],"default":true}
                    ]
                }),
            ),
            (
                "foreign",
                json!({
                    "schemaVersion": 1, "title": "Foreign app", "description": "Other OS",
                    "layers": {layer: [foreign]}
                }),
            ),
        ] {
            modules.push(ModuleRow {
                repository: 0,
                selected: false,
                values: BTreeMap::new(),
                module: CatalogModule {
                    path: path.into(),
                    manifest: serde_json::from_value(manifest).unwrap(),
                },
            });
        }
        state.modules = modules;
        state.modules[0].selected = true;
        let mut ui = Ui {
            page: 1,
            search: "intel-tool".into(),
            ..Ui::default()
        };
        let rows = ui.rows(&state);
        assert!(rows.iter().any(|row| row.text.contains(&format!(
            "[×] Intel tool — unavailable on {}",
            target.system
        ))));
        assert!(!rows.iter().any(|row| row.text.contains("Foreign app")));
        assert_eq!(
            rows.iter().filter(|row| row.text.contains("curl")).count(),
            1
        );
        let active = rows
            .iter()
            .find(|row| row.text.contains("[×] curl") && !row.text.contains("unavailable"))
            .unwrap();
        let foreign_key = format!("{layer}.components.tool.enabledPackages.curl");
        state.modules[0]
            .values
            .insert(foreign_key.clone(), json!(true));
        ui.activate(&mut state, active.control.clone());
        assert_eq!(
            state.modules[0].values["home.components.tool.enabledPackages.curl"],
            false
        );
        assert_eq!(state.modules[0].values[&foreign_key], true);
        ui.search.clear();
        let rows = ui.rows(&state);
        let unavailable = rows
            .iter()
            .find(|row| row.text.starts_with("Unavailable") && row.text.contains("Foreign app"))
            .unwrap();
        ui.activate(&mut state, unavailable.control.clone());
        assert!(state.modules[1].selected);
        let review = ui.review_rows(&state);
        assert_eq!(
            review
                .iter()
                .filter(|row| row.text.contains("curl"))
                .count(),
            1
        );
        assert!(
            review
                .iter()
                .any(|row| row.text.contains("[ ] curl") && !row.text.contains("unavailable"))
        );
        assert!(
            review
                .iter()
                .any(|row| row.text.contains("1 active / 2 saved overrides"))
        );
        assert!(
            review
                .iter()
                .any(|row| row.text.starts_with("Unavailable") && row.text.contains("foreign"))
        );
    }

    #[test]
    fn disk_editor_requires_exact_device_confirmation_before_callback() {
        let mut state = session();
        let mut ui = Ui::default();
        ui.activate(
            &mut state,
            Some(Control::Confirm(Action::EditPartitions("/dev/test".into()))),
        );
        ui.event(&mut state, Event::Text("PARTITION /dev/wrong".into()));
        assert!(matches!(
            ui.event(&mut state, Event::Key(13)),
            Outcome::Continue
        ));
        ui.event(&mut state, Event::Key(21));
        ui.event(&mut state, Event::Text("PARTITION /dev/test".into()));
        assert!(
            matches!(ui.event(&mut state, Event::Key(13)), Outcome::Execute(Action::EditPartitions(path)) if path == "/dev/test")
        );
    }

    #[test]
    fn alongside_layout_requires_uefi_and_resets_efi_when_disk_changes() {
        let mut request = session().install;
        request.layout = DiskLayout::Erase {
            disk: String::new(),
        };
        request.boot_mode = BootMode::Bios;
        change_setting(&mut request, Setting::Layout);
        assert_eq!(request.boot_mode, BootMode::Uefi);
        set_device(&mut request.layout, DeviceSlot::Disk, "/dev/first".into());
        set_device(
            &mut request.layout,
            DeviceSlot::AlongsideEsp,
            "/dev/first1".into(),
        );
        set_device(&mut request.layout, DeviceSlot::Disk, "/dev/second".into());
        assert_eq!(
            request.layout,
            DiskLayout::Alongside {
                disk: "/dev/second".into(),
                esp: None
            }
        );
    }
}
