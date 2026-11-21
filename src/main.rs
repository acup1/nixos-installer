mod catalog;
mod config;
mod install;
mod ui;

use catalog::{CatalogSource, Layer, OptionKind, Platform, SourceMode, SourceSpec};
use config::{Choice, Location};
use install::{BootMode, DiskLayout, FileSystem, InstallRequest};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    env, fs,
    io::{self, IsTerminal},
    path::{Path, PathBuf},
    process::Command,
};
use ui::{Action, Mode, ModuleRow, Session};

type Result<T> = std::result::Result<T, String>;
type Settings = BTreeMap<String, Value>;

#[derive(Clone, Serialize, Deserialize)]
struct Selected {
    source: SourceSpec,
    path: String,
    values: Settings,
}

#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
struct Saved {
    repositories: Vec<SourceSpec>,
    selections: Vec<Selected>,
    flake: String,
    selector: String,
}

fn layer_name(layer: Layer) -> &'static str {
    match layer {
        Layer::Nixos => "nixos",
        Layer::Home => "home",
        Layer::Darwin => "darwin",
    }
}
fn kind_name(kind: OptionKind) -> &'static str {
    match kind {
        OptionKind::Bool => "bool",
        OptionKind::Integer => "integer",
        OptionKind::Packages => "packages",
        OptionKind::String => "string",
        OptionKind::StringList => "string_list",
    }
}

struct Controller {
    dir: PathBuf,
    sources: Vec<CatalogSource>,
    loaded: Vec<SourceSpec>,
    saved: Saved,
}
impl Controller {
    fn selections(&self, state: &Session) -> Vec<Selected> {
        state
            .modules
            .iter()
            .filter(|row| row.selected)
            .filter_map(|row| {
                Some(Selected {
                    source: self.loaded.get(row.repository)?.clone(),
                    path: row.module.path.clone(),
                    values: row.values.clone(),
                })
            })
            .collect()
    }
    fn refresh(&mut self, state: &mut Session) -> Result<String> {
        let previous = if self.loaded.is_empty() {
            self.saved.selections.clone()
        } else {
            self.selections(state)
        };
        let mut sources = vec![];
        let mut rows = vec![];
        let platform = if state.mode == Mode::Install || cfg!(target_os = "linux") {
            Platform::Linux
        } else {
            Platform::Darwin
        };
        for (i, spec) in state.repositories.iter().enumerate() {
            if state.repositories[..i].contains(spec) {
                return Err(format!("Duplicate repository {}", spec.repository));
            }
            let source = catalog::discover(spec, &self.dir.join("repositories"))?;
            for module in source
                .modules
                .iter()
                .filter(|m| m.manifest.supports(platform))
            {
                let prior = previous
                    .iter()
                    .find(|s| &s.source == spec && s.path == module.path);
                rows.push(ModuleRow {
                    repository: i,
                    module: module.clone(),
                    selected: prior.is_some(),
                    values: prior.map(|s| s.values.clone()).unwrap_or_default(),
                });
            }
            sources.push(source);
        }
        self.loaded = state.repositories.clone();
        self.sources = sources;
        state.modules = rows;
        state.plan = None;
        Ok(format!(
            "Loaded {} modules from {} repositories",
            state.modules.len(),
            self.sources.len()
        ))
    }
    fn choices(&self, state: &Session) -> Result<Vec<Choice>> {
        if self.loaded != state.repositories {
            return Err("Refresh the catalog after changing repositories.".into());
        }
        let mut choices = vec![];
        for row in state.modules.iter().filter(|row| row.selected) {
            let source = self
                .sources
                .get(row.repository)
                .ok_or("Stale module repository")?;
            let found =
                catalog::validate_selection(source, std::slice::from_ref(&row.module.path))?;
            let manifest = &found[0].manifest;
            let mut settings = vec![];
            for (key, v) in &row.values {
                let opt = manifest
                    .options
                    .iter()
                    .find(|o| format!("{}.{}", layer_name(o.layer), o.path.join(".")) == *key)
                    .ok_or_else(|| format!("Unknown option {key}"))?;
                if !opt.kind.accepts(v) {
                    return Err(format!("Invalid value for {key}"));
                }
                settings.push((
                    layer_name(opt.layer).into(),
                    opt.path.clone(),
                    kind_name(opt.kind).into(),
                    v.clone(),
                ));
            }
            choices.push(Choice {
                repo: row.repository,
                path: row.module.path.clone(),
                settings,
            });
        }
        Ok(choices)
    }
    fn locations(&self, target_copy: bool) -> Result<Vec<Location>> {
        self.sources
            .iter()
            .enumerate()
            .map(|(i, s)| {
                Ok(match s.spec.mode {
                    SourceMode::Local if target_copy => Location::TargetCopy(i),
                    SourceMode::Local => Location::Local(s.checkout.clone()),
                    SourceMode::Pinned => Location::Remote {
                        url: s.spec.repository.clone(),
                        rev: s.revision.clone().ok_or("Missing source revision")?,
                    },
                })
            })
            .collect()
    }
    fn snapshot(&self, s: &Session) -> Saved {
        Saved {
            repositories: s.repositories.clone(),
            selections: self.selections(s),
            selector: s.selector.clone(),
            flake: s.flake.clone(),
        }
    }
    fn save(&mut self, state: &Session) -> Result<String> {
        let modules = config::selection(&self.locations(false)?, &self.choices(state)?)?;
        config::save_selector(Path::new(&state.selector), &modules, None)?;
        self.saved = self.snapshot(state);
        config::atomic(
            &self.dir.join("state.json"),
            &serde_json::to_vec_pretty(&self.saved).map_err(|e| e.to_string())?,
        )?;
        Ok(format!("Saved {}", state.selector))
    }
    fn apply(&self, state: &Session) -> Result<String> {
        let choices = self.choices(state)?;
        let modules = config::selection(&self.locations(true)?, &choices)?;
        let plan = state
            .plan
            .as_ref()
            .ok_or("Review the installation plan first")?;
        if plan.request != state.install {
            return Err("Review your updated installation settings first".into());
        }
        let system = config::architecture()?;
        self.check_local_install(&choices)?;
        install::apply_plan(plan, |root, req| {
            let dir = root.join("etc/nixos");
            for (i, source) in self.sources.iter().enumerate() {
                if source.spec.mode == SourceMode::Local && choices.iter().any(|c| c.repo == i) {
                    copy_commit(source, &dir.join(format!("module-sources/{i}")))?;
                }
            }
            config::save_selector(
                &dir.join("config.nix"),
                &modules,
                Some((&req.username, &req.hostname, &system)),
            )?;
            config::fresh_flake(&dir, &req.hostname)?;
            let mut saved = self.snapshot(state);
            saved.flake = format!("/etc/nixos#{}", req.hostname);
            saved.selector = "/etc/nixos/config.nix".into();
            for (index, src) in self.sources.iter().enumerate() {
                if src.spec.mode == SourceMode::Local && choices.iter().any(|c| c.repo == index) {
                    let target = SourceSpec {
                        repository: format!("/etc/nixos/module-sources/{index}"),
                        reference: "HEAD".into(),
                        mode: SourceMode::Local,
                    };
                    for item in &mut saved.selections {
                        if item.source == saved.repositories[index] {
                            item.source = target.clone();
                        }
                    }
                    saved.repositories[index] = target;
                }
            }
            let state_file = dir.join("nixos-installer-state.json");
            config::atomic(
                &state_file,
                &serde_json::to_vec_pretty(&saved).map_err(|e| e.to_string())?,
            )?;
            for file in [&state_file, &dir.join("config.nix"), &dir.join("flake.nix")] {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(file, fs::Permissions::from_mode(0o644))
                    .map_err(|e| e.to_string())?;
            }
            Ok(())
        })?;
        Ok("Installation completed".into())
    }
    fn check_local_install(&self, choices: &[Choice]) -> Result<()> {
        for (i, source) in self.sources.iter().enumerate() {
            if source.spec.mode != SourceMode::Local || !choices.iter().any(|c| c.repo == i) {
                continue;
            }
            let output = Command::new("git")
                .current_dir(&source.checkout)
                .args(["status", "--porcelain"])
                .output()
                .map_err(|e| e.to_string())?;
            if !output.status.success() || !output.stdout.is_empty() {
                return Err(format!(
                    "Commit changes in {} before copying it to the target",
                    source.checkout.display()
                ));
            }
        }
        Ok(())
    }
    fn action(&mut self, action: Action, state: &mut Session) -> Result<String> {
        match action {
            Action::Calamares => {
                let status = Command::new("calamares")
                    .status()
                    .map_err(|e| format!("calamares: {e}"))?;
                if !status.success() {
                    return Err(format!("Calamares exited with {status}"));
                }
                state.plan = None;
                Ok("Calamares closed".into())
            }
            Action::RefreshCatalog => self.refresh(state),
            Action::RefreshDisks => {
                state.disks = install::probe()?;
                state.plan = None;
                Ok("Disk inventory refreshed".into())
            }
            Action::EditPartitions(disk) => {
                state.plan = None;
                install::edit_partitions(&disk)?;
                state.disks = install::probe()?;
                Ok("Disk inventory refreshed".into())
            }
            Action::ReviewInstall => {
                let choices = self.choices(state)?;
                self.check_local_install(&choices)?;
                let mut plan = install::build_plan(state.install.clone())?;
                for s in &self.sources {
                    plan.steps.push(format!(
                        "Module source {} @ {}",
                        s.spec.repository,
                        s.revision.as_deref().unwrap_or("working tree")
                    ));
                }
                for c in &choices {
                    plan.steps.push(format!(
                        "Enable {} ({} overrides)",
                        c.path,
                        c.settings.len()
                    ));
                }
                state.plan = Some(plan);
                Ok("Installation plan ready".into())
            }
            Action::ApplyInstall => self.apply(state),
            Action::Save => self.save(state),
            Action::Rebuild => {
                self.save(state)?;
                config::rebuild(&state.flake, &state.selector)?;
                Ok("System rebuilt".into())
            }
        }
    }
}

fn copy_commit(source: &CatalogSource, dest: &Path) -> Result<()> {
    let rev = source.revision.as_deref().ok_or("Missing local commit")?;
    fs::create_dir_all(dest.parent().ok_or("Invalid destination")?).map_err(|e| e.to_string())?;
    fs::create_dir(dest).map_err(|e| format!("{}: {e}", dest.display()))?;
    for args in [
        vec!["init", "--quiet", "--template="],
        vec![
            "fetch",
            "--quiet",
            "--no-tags",
            "--depth=1",
            "--",
            source.checkout.to_str().ok_or("Invalid repository path")?,
            rev,
        ],
        vec!["checkout", "--quiet", "--detach", rev],
    ] {
        let status = Command::new("git")
            .arg("-c")
            .arg("core.hooksPath=/dev/null")
            .current_dir(dest)
            .args(args)
            .status()
            .map_err(|e| e.to_string())?;
        if !status.success() {
            return Err(format!(
                "Could not copy {} at {rev}",
                source.checkout.display()
            ));
        }
    }
    Ok(())
}

const HELP: &str = "nixos-installer [configure|install]\n\nconfigure  Select module repositories, edit parameters and rebuild with nh.\ninstall    Install NixOS on a Linux machine (root required to apply).\n\nUse the repository and path controls to select your host flake and config.nix.\n  --version    Print the release version\n  --help       Show this help";
fn run() -> Result<()> {
    let args = env::args().skip(1).collect::<Vec<_>>();
    let mode = match args
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .as_slice()
    {
        ["--version"] | ["-V"] => {
            println!("nixos-installer {}", env!("CARGO_PKG_VERSION"));
            return Ok(());
        }
        ["--help"] | ["-h"] => {
            println!("{HELP}");
            return Ok(());
        }
        [] | ["configure"] => Mode::Configure,
        ["install"] if cfg!(target_os = "linux") => Mode::Install,
        ["install"] => return Err("Installation requires a Linux live system".into()),
        _ => return Err(HELP.into()),
    };
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return Err("Open an interactive terminal, or use --help.".into());
    }
    let home = env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or("Missing HOME")?;
    let dir = env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".local/state"))
        .join("nixos-installer");
    let mut saved_path = dir.join("state.json");
    if mode == Mode::Configure
        && !saved_path.exists()
        && Path::new("/etc/nixos/nixos-installer-state.json").is_file()
    {
        saved_path = "/etc/nixos/nixos-installer-state.json".into();
    }
    let saved: Saved = match fs::read(saved_path) {
        Ok(data) => {
            serde_json::from_slice(&data).map_err(|e| format!("Invalid saved state: {e}"))?
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => Saved::default(),
        Err(e) => return Err(e.to_string()),
    };
    let repositories = if saved.repositories.is_empty() {
        vec![SourceSpec {
            repository: "https://github.com/acup1/nixos-modules.git".into(),
            reference: "main".into(),
            mode: SourceMode::Pinned,
        }]
    } else {
        saved.repositories.clone()
    };
    let selector = if !saved.selector.is_empty() {
        saved.selector.clone()
    } else {
        env::var("NIXOS_BASE_CONFIG")
            .unwrap_or_else(|_| home.join(".config/config.nix").display().to_string())
    };
    let flake = if !saved.flake.is_empty() {
        saved.flake.clone()
    } else {
        env::var("NH_FLAKE").unwrap_or_else(|_| "/etc/nixos".into())
    };
    let request = InstallRequest {
        layout: DiskLayout::Mounted {
            target: "/mnt".into(),
        },
        boot_mode: if Path::new("/sys/firmware/efi").is_dir() {
            BootMode::Uefi
        } else {
            BootMode::Bios
        },
        filesystem: FileSystem::Ext4,
        encrypt: false,
        swap_gib: 0,
        hostname: "nixos".into(),
        username: "nixos".into(),
        timezone: "UTC".into(),
        locale: "en_US.UTF-8".into(),
        keymap: "us".into(),
        desktop: None,
        network_manager: true,
    };
    let mut state = Session::new(mode, repositories, request, flake, selector);
    let mut controller = Controller {
        dir,
        sources: vec![],
        loaded: vec![],
        saved,
    };
    if let Err(e) = controller.refresh(&mut state) {
        state.message = Some((
            format!("Catalog: {e}. Edit repositories and refresh to retry."),
            false,
        ));
    }
    ui::run(&mut state, |action, state| controller.action(action, state))
}
fn main() {
    if let Err(e) = run() {
        eprintln!("{e}");
        std::process::exit(1);
    }
}
