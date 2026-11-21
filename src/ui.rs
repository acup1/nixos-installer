use crate::catalog::{
    CatalogModule, Layer, ManifestOption, OptionKind, Platform, SourceMode, SourceSpec,
};
use crate::install::{
    BlockDevice, BootMode, DiskLayout, FileSystem, InstallPlan, InstallRequest, confirmation_token,
};
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

pub struct Session {
    pub mode: Mode,
    pub repositories: Vec<SourceSpec>,
    pub modules: Vec<ModuleRow>,
    pub flake: String,
    pub selector: String,
    pub install: InstallRequest,
    pub disks: Vec<BlockDevice>,
    pub plan: Option<InstallPlan>,
    pub message: Option<(String, bool)>,
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
            selector,
            install,
            disks: Vec::new(),
            plan: None,
            message: None,
        }
    }
}

#[derive(Clone, Debug)]
pub enum Action {
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
            Self::Modules => "Toggle components and edit their Nixpkgs package lists.",
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
const CONFIG_PAGES: &[Page] = &[Page::Sources, Page::Modules, Page::Review];

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
    BootDisk,
    PartitionEditor,
}

#[derive(Clone)]
enum Control {
    Edit(Field),
    SelectModule(usize),
    AddRepository,
    ToggleSource(usize),
    RemoveRepository(usize),
    Change(Setting),
    Choose(DeviceSlot),
    Page(usize),
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
    value: String,
    label: String,
}

struct Picker {
    slot: DeviceSlot,
    choices: Vec<Choice>,
    cursor: usize,
    offset: usize,
}

enum Modal {
    Edit(Editor),
    Pick(Picker),
    Notice { text: String, offset: usize },
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

pub fn run(
    state: &mut Session,
    mut handle: impl FnMut(Action, &mut Session) -> Result<String, String>,
) -> Result<(), String> {
    let error = |err: io::Error| format!("Terminal error: {err}");
    let mut terminal = open_terminal().map_err(error)?;
    let mut frame = FrameBuffer::default();
    let message = state.message.take();
    let modal = message
        .as_ref()
        .filter(|(_, success)| !*success)
        .map(|(text, _)| Modal::Notice {
            text: text.clone(),
            offset: 0,
        });
    let mut ui = Ui {
        message,
        modal,
        calamares: state.mode == Mode::Install && executable_in_path("calamares"),
        ..Ui::default()
    };
    let mut redraw = true;
    loop {
        let (height, width) = terminal.getsize();
        if redraw || height != frame.height() || width != frame.width() {
            frame.resize(width, height);
            ui.render(&mut frame, state);
            frame.present(&mut terminal.stdout).map_err(error)?;
            redraw = false;
        }
        let event = match read_event(&mut terminal) {
            Ok(Event::Idle) => continue,
            Ok(event) => event,
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            Err(err) => return Err(error(err)),
        };
        redraw = true;
        match ui.event(state, event) {
            Outcome::Continue => {}
            Outcome::Quit => return terminal.restore().map_err(error),
            Outcome::Execute(action) => {
                terminal.restore().map_err(error)?;
                drop(terminal);
                if matches!(action, Action::EditPartitions(_) | Action::Calamares) {
                    state.plan = None;
                }
                let applied = matches!(action, Action::ApplyInstall);
                let review = matches!(action, Action::ReviewInstall);
                let refresh = matches!(action, Action::RefreshCatalog);
                match handle(action, state) {
                    Ok(message) => {
                        ui.applied |= applied;
                        ui.catalog_dirty &= !refresh;
                        ui.message = Some((message, true));
                    }
                    Err(message) => {
                        if review || applied {
                            state.plan = None;
                        }
                        ui.modal = Some(Modal::Notice {
                            text: message.clone(),
                            offset: 0,
                        });
                        ui.message = Some((message, false));
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
                    rows.push(Row::field("Host flake", &state.flake, Field::Flake));
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
                let platform = target_platform(state.mode);
                if self.catalog_dirty {
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
                    {
                        continue;
                    }
                    found = true;
                    let checked = if row.selected { "[×]" } else { "[ ]" };
                    let title = format!("{checked} {} · {}", module.path, module.manifest.title);
                    if !module.manifest.supports(platform) {
                        rows.push(Row::note(format!("{title} · unavailable on this platform")));
                        continue;
                    }
                    rows.push(Row::action(title, Control::SelectModule(index)));
                    if row.selected {
                        for (parameter, option) in module.manifest.options.iter().enumerate() {
                            if !module.manifest.layers[&option.layer].contains(&platform) {
                                continue;
                            }
                            let saved = row.values.get(&option_key(option));
                            let value = saved
                                .or(option.default.as_ref())
                                .map(|value| option_text(option.kind, value));
                            let display = match (saved, value) {
                                (Some(_), Some(value)) => value,
                                (None, Some(value)) => format!("{value} (default)"),
                                _ => "Default".into(),
                            };
                            rows.push(Row::field(
                                &format!("  {} · {}", layer_name(option.layer), option.title),
                                &display,
                                Field::Parameter(index, parameter),
                            ));
                        }
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
            DiskLayout::Partitions { .. } => "Use existing partitions",
            DiskLayout::Mounted { .. } => "Use a mounted target",
        };
        let mut rows = vec![Row::action(
            format!("Layout             {layout}"),
            Control::Change(Setting::Layout),
        )];
        match &request.layout {
            DiskLayout::Erase { disk } => rows.push(Row::action(
                format!("Target disk        {}", partition_name(disk)),
                Control::Choose(DeviceSlot::Disk),
            )),
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
        rows.push(Row::action(
            format!("Boot mode          {}", boot_name(&request.boot_mode)),
            Control::Change(Setting::Boot),
        ));
        if !matches!(request.layout, DiskLayout::Mounted { .. }) {
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
            rows.push(Row::action(
                "Refresh disk list",
                Control::Send(Action::RefreshDisks),
            ));
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
        rows.push(Row::note(format!(
            "{} selected components · {} repositories",
            active.len(),
            state.repositories.len()
        )));
        for module in active {
            rows.push(Row::note(format!(
                "  {} · {} overrides",
                module.module.path,
                module.values.len()
            )));
            for option in &module.module.manifest.options {
                if let Some(value) = module.values.get(&option_key(option)) {
                    rows.push(Row::note(format!(
                        "    {} · {} = {}",
                        layer_name(option.layer),
                        option.title,
                        option_text(option.kind, value)
                    )));
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
                    if selector_valid && !state.flake.trim().is_empty() {
                        Row::action(
                            "Save and run nh rebuild…",
                            Control::Confirm(Action::Rebuild),
                        )
                    } else {
                        Row::note("Rebuild unavailable · set host flake and selector paths")
                    },
                );
            }
            Mode::Install => {
                rows.push(Row::note(format!(
                    "{}@{} · {} · {}",
                    state.install.username,
                    state.install.hostname,
                    boot_name(&state.install.boot_mode),
                    filesystem_name(&state.install.filesystem)
                )));
                if self.applied {
                    rows.insert(0, Row::note("Installation completed."));
                } else if let Some(plan) = &state.plan {
                    rows.insert(
                        0,
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

    fn event(&mut self, state: &mut Session, event: Event) -> Outcome {
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
                let choices = device_choices(&state.disks, slot);
                if choices.is_empty() {
                    self.message = Some((
                        "No devices match this selection. Refresh the disk list.".into(),
                        false,
                    ));
                } else {
                    self.modal = Some(Modal::Pick(Picker {
                        slot,
                        choices,
                        cursor: 0,
                        offset: 0,
                    }));
                }
            }
            Some(Control::Page(page)) => {
                self.change_page(page);
            }
            Some(Control::Confirm(action)) => {
                let word = match &action {
                    Action::ApplyInstall => match &state.plan {
                        Some(plan) => confirmation_token(plan),
                        None => return Outcome::Continue,
                    },
                    Action::EditPartitions(disk) => format!("PARTITION {disk}"),
                    Action::Calamares => "CALAMARES".into(),
                    Action::Rebuild => "REBUILD".into(),
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
                        if let EditTarget::Field(Field::Parameter(row, parameter)) = editor.target {
                            let module = &mut state.modules[row];
                            let key = option_key(&module.module.manifest.options[parameter]);
                            module.values.remove(&key);
                            state.plan = None;
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
                    if matches!(picker.slot, DeviceSlot::PartitionEditor) {
                        return self.activate(
                            state,
                            Some(Control::Confirm(Action::EditPartitions(choice.value))),
                        );
                    }
                    set_device(&mut state.install.layout, picker.slot, choice.value);
                    state.plan = None;
                } else {
                    self.modal = Some(Modal::Pick(picker));
                }
            }
            Modal::Notice { text, mut offset } => {
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
                self.modal = Some(Modal::Notice { text, offset });
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
                    .or(option.default.as_ref())
                    .map(|value| option_text(option.kind, value))
                    .unwrap_or_default()
            }
            Field::Flake => state.flake.clone(),
            Field::Selector => state.selector.clone(),
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
                Field::Source(_)
                    | Field::Hostname
                    | Field::Username
                    | Field::Timezone
                    | Field::Locale
                    | Field::Keymap
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
                state.selector = value.into();
            }
            Field::Hostname => state.install.hostname = value.into(),
            Field::Username => state.install.username = value.into(),
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
                DiskLayout::Erase { .. } => DiskLayout::Partitions {
                    root: String::new(),
                    esp: None,
                    boot_disk: None,
                    format_root: false,
                    format_esp: false,
                },
                DiskLayout::Partitions { .. } => DiskLayout::Mounted {
                    target: PathBuf::from("/mnt"),
                },
                DiskLayout::Mounted { .. } => DiskLayout::Erase {
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

fn device_choices(devices: &[BlockDevice], slot: DeviceSlot) -> Vec<Choice> {
    fn visit(devices: &[BlockDevice], slot: DeviceSlot, choices: &mut Vec<Choice>) {
        for device in devices {
            let target = match slot {
                DeviceSlot::Disk | DeviceSlot::BootDisk | DeviceSlot::PartitionEditor => {
                    device.kind == "disk"
                }
                DeviceSlot::Root => ["part", "crypt", "lvm"].contains(&device.kind.as_str()),
                DeviceSlot::Esp => device.kind == "part",
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
                    value: device.path.clone(),
                    label,
                });
            }
            visit(&device.children, slot, choices);
        }
    }
    let mut choices = Vec::new();
    visit(devices, slot, &mut choices);
    choices
}

fn set_device(layout: &mut DiskLayout, slot: DeviceSlot, value: String) {
    match (layout, slot) {
        (DiskLayout::Erase { disk }, DeviceSlot::Disk) => *disk = value,
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
        line(
            frame,
            top,
            content.x + 2,
            content.w - 4,
            page.subtitle(),
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
        let bottom = if let Some((message, success)) = &self.message {
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
                Modal::Notice { text, offset } => {
                    render_notice(frame, area, text, offset);
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
    let title = match picker.slot {
        DeviceSlot::Disk => "Choose installation disk",
        DeviceSlot::Root => "Choose root partition",
        DeviceSlot::Esp => "Choose EFI System Partition",
        DeviceSlot::BootDisk => "Choose boot disk",
        DeviceSlot::PartitionEditor => "Select disk for cfdisk",
    };
    box_panel(frame, area, title);
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
        "Enter selects · Esc cancels",
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
        Field::Flake => "Host flake reference",
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
        OptionKind::Bool => "true or false",
        OptionKind::Integer => "Whole number",
        OptionKind::String => "Text",
        OptionKind::StringList => "JSON string list",
        OptionKind::Packages => "Package names separated by commas",
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
        OptionKind::Bool => Value::Bool(input.parse().map_err(|_| error())?),
        OptionKind::Integer => Value::from(input.parse::<i64>().map_err(|_| error())?),
        OptionKind::String => Value::String(input.to_owned()),
        OptionKind::StringList => serde_json::from_str(input).map_err(|_| error())?,
        OptionKind::Packages => Value::Array(
            input
                .split(|ch: char| ch == ',' || ch.is_whitespace())
                .filter(|name| !name.is_empty())
                .map(|name| Value::String(name.to_owned()))
                .collect(),
        ),
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

fn render_notice(frame: &mut FrameBuffer, area: Area, text: &str, offset: &mut usize) {
    box_panel(frame, area, "Action failed");
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
            if index == 0 { RED } else { TEXT },
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
        assert!(frame_text(&frame, 5).contains("Step 2 of 3"));
        assert!(frame_text(&frame, 8).contains("Filter modules"));
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
                    options: vec![ManifestOption {
                        layer: Layer::Home,
                        path: vec!["example".into(), "packages".into()],
                        title: "Packages".into(),
                        description: "Packages".into(),
                        kind: OptionKind::Packages,
                        default: Some(json!(["jq"])),
                    }],
                },
            },
        });
        let mut ui = Ui::default();
        assert!(
            ui.set_field(&mut state, Field::Parameter(0, 0), "hello; abort")
                .is_err()
        );
        ui.set_field(
            &mut state,
            Field::Parameter(0, 0),
            "ripgrep, nerd-fonts.jetbrains-mono",
        )
        .unwrap();
        assert_eq!(
            state.modules[0].values["home.example.packages"],
            json!(["ripgrep", "nerd-fonts.jetbrains-mono"])
        );
        ui.activate(&mut state, Some(Control::Edit(Field::Parameter(0, 0))));
        ui.event(&mut state, Event::Key(18));
        assert!(state.modules[0].values.is_empty());
        assert_eq!(ui.field(&state, Field::Parameter(0, 0)), "jq");
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
}
