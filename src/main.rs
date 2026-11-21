mod catalog;
mod config;
mod install;
mod ui;

use catalog::{CatalogSource, Layer, OptionKind, Platform, SourceMode, SourceSpec};
use config::{Choice, Location, Selected, Selector};
use install::{BootMode, DiskLayout, FileSystem, InstallRequest};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    env, fs,
    io::{self, IsTerminal},
    path::{Path, PathBuf},
    process::Command,
};
use ui::{Action, Mode, ModuleRow, Session};

type Result<T> = std::result::Result<T, String>;
type Settings = BTreeMap<String, Value>;

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
    selector: Option<Selector>,
    inspected_host: String,
    manual: BTreeMap<(usize, String), String>,
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
    fn refresh(&mut self, state: &mut Session, update: bool) -> Result<String> {
        let selector_path = Path::new(&state.selector);
        let same_selector = self.inspected_host == state.flake
            && self
                .selector
                .as_ref()
                .is_some_and(|selector| selector.current(selector_path));
        let previous: HashMap<_, _> = if same_selector || state.mode == Mode::Install {
            state
                .modules
                .iter()
                .filter_map(|row| {
                    Some((
                        (
                            self.loaded.get(row.repository)?.clone(),
                            row.module.path.clone(),
                        ),
                        (row.selected, row.values.clone()),
                    ))
                })
                .collect()
        } else {
            HashMap::new()
        };
        let selector = if state.mode == Mode::Configure {
            Selector::read(selector_path)?
        } else {
            None
        };
        let repositories = if !same_selector {
            selector
                .as_ref()
                .and_then(Selector::repositories)
                .map(<[SourceSpec]>::to_vec)
                .unwrap_or_else(|| state.repositories.clone())
        } else {
            state.repositories.clone()
        };
        let originals = selector
            .as_ref()
            .map(|selector| selector.original_paths(&state.flake))
            .transpose()?
            .unwrap_or_default();
        let persisted = selector
            .as_ref()
            .map(Selector::selections)
            .unwrap_or_else(|| {
                if state.mode == Mode::Install {
                    &self.saved.selections
                } else {
                    &[]
                }
            });
        let masked = selector
            .as_ref()
            .map_or_else(BTreeSet::new, Selector::masked);

        let mut sources = vec![];
        let mut rows = vec![];
        let platform = if state.mode == Mode::Install || cfg!(target_os = "linux") {
            Platform::Linux
        } else {
            Platform::Darwin
        };
        for (i, spec) in repositories.iter().enumerate() {
            if repositories[..i].contains(spec) {
                return Err(format!("Duplicate repository {}", spec.repository));
            }
            let cache = self.dir.join("repositories");
            let source = if update {
                catalog::discover_updated(spec, &cache)?
            } else {
                catalog::discover(spec, &cache)?
            };
            for module in source
                .modules
                .iter()
                .filter(|m| m.manifest.supports(platform))
            {
                rows.push(ModuleRow {
                    repository: i,
                    module: module.clone(),
                    selected: false,
                    values: Settings::new(),
                });
            }
            sources.push(source);
        }

        let manual = manual_matches(selector_path, &originals, &sources, &rows);
        for row in &mut rows {
            let spec = &repositories[row.repository];
            let path = &row.module.path;
            let key = (spec.clone(), path.clone());
            if let Some((selected, values)) = previous.get(&key) {
                row.selected = *selected;
                row.values = values.clone();
            } else if let Some(setting) = persisted
                .iter()
                .find(|setting| &setting.source == spec && &setting.path == path)
            {
                row.selected = true;
                row.values = setting.values.clone();
            } else {
                row.selected = manual
                    .get(&(row.repository, path.clone()))
                    .is_some_and(|manual_path| !masked.contains(manual_path));
            }
        }
        self.loaded = repositories.clone();
        state.repositories = repositories;
        self.sources = sources;
        self.selector = selector;
        self.inspected_host = state.flake.clone();
        self.manual = manual;
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
    fn synchronized_state(&self, state: &Session, target: bool) -> Saved {
        let mut saved = self.snapshot(state);
        let used: BTreeSet<_> = state
            .modules
            .iter()
            .filter(|row| row.selected)
            .map(|row| row.repository)
            .collect();
        let mut repositories = Vec::new();
        for (index, source) in self.sources.iter().enumerate() {
            if target && !used.contains(&index) {
                continue;
            }
            let old = &saved.repositories[index];
            let effective = match source.spec.mode {
                SourceMode::Local if target => SourceSpec {
                    repository: format!("/etc/nixos/module-sources/{index}"),
                    reference: "HEAD".into(),
                    mode: SourceMode::Local,
                },
                SourceMode::Pinned if used.contains(&index) => SourceSpec {
                    reference: source
                        .revision
                        .clone()
                        .unwrap_or_else(|| old.reference.clone()),
                    ..source.spec.clone()
                },
                _ => old.clone(),
            };
            for selected in &mut saved.selections {
                if &selected.source == old {
                    selected.source = effective.clone();
                }
            }
            repositories.push(effective);
        }
        saved.repositories = repositories;
        saved
    }

    fn save(&mut self, state: &mut Session) -> Result<String> {
        let path = Path::new(&state.selector);
        let selector = self
            .selector
            .as_ref()
            .ok_or("Load an existing selector before saving.")?;
        if self.inspected_host != state.flake || !selector.current(path) {
            return Err("The host or selector changed. Refresh the catalog before saving.".into());
        }
        let choices = self.choices(state)?;
        for previous in selector.selections() {
            if let Some(index) = self
                .loaded
                .iter()
                .position(|source| source == &previous.source)
                && !state
                    .modules
                    .iter()
                    .any(|row| row.repository == index && row.module.path == previous.path)
            {
                return Err(format!(
                    "Module {} is unavailable in this catalog. Its selector was preserved.",
                    previous.path
                ));
            }
        }
        let mut masked = selector.masked();
        for row in &state.modules {
            if let Some(path) = self.manual.get(&(row.repository, row.module.path.clone())) {
                if row.selected {
                    masked.remove(path);
                } else {
                    masked.insert(path.clone());
                }
            }
        }
        let mut new = Vec::new();
        let mut changes = Vec::new();
        for choice in &choices {
            if let Some(path) = self.manual.get(&(choice.repo, choice.path.clone())) {
                if !choice.settings.is_empty() {
                    changes.push((path.as_str(), choice));
                }
            } else {
                new.push(choice.clone());
            }
        }

        let saved = self.synchronized_state(state, false);
        if selector.managed() || !new.is_empty() || !changes.is_empty() || !masked.is_empty() {
            selector.save(
                config::selection(&self.locations(false)?, &new)?,
                config::manual_changes(&changes)?,
                masked,
                saved.selections.clone(),
                saved.repositories.clone(),
            )?;
            self.selector = Selector::read(path)?;
        }
        state.repositories = saved.repositories.clone();
        self.loaded = saved.repositories.clone();
        self.saved = saved;
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
            let mut saved = self.synchronized_state(state, true);
            saved.flake = format!("/etc/nixos#{}", req.hostname);
            saved.selector = "/etc/nixos/config.nix".into();
            config::create_selector(
                &dir.join("config.nix"),
                modules,
                saved.selections.clone(),
                saved.repositories.clone(),
                &req.username,
                &req.hostname,
                &system,
            )?;
            config::fresh_flake(&dir, &req.hostname)?;
            let state_file = dir.join("nixos-installer-state.json");
            config::atomic(
                &state_file,
                &serde_json::to_vec_pretty(&saved).map_err(|e| e.to_string())?,
            )?;
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&state_file, fs::Permissions::from_mode(0o644))
                .map_err(|e| e.to_string())?;
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
            let current = git_output(&source.checkout, &["rev-parse", "HEAD"]);
            if !output.status.success() || !output.stdout.is_empty() || current != source.revision {
                return Err(format!(
                    "Refresh the clean repository {} before copying it to the target",
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
            Action::RefreshCatalog => self.refresh(state, true),
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

fn remote_identity(url: &str) -> Option<String> {
    let (host, path) = if let Some(url) = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("ssh://"))
    {
        url.split_once('/')?
    } else {
        let (host, path) = url.split_once(':')?;
        if !host.contains('@') {
            return None;
        }
        (host, path.trim_start_matches('/'))
    };
    let name = host.rsplit_once('@').map_or(host, |(_, host)| host);
    let path = path.trim_end_matches('/');
    Some(format!(
        "{}/{}",
        name.to_ascii_lowercase(),
        path.strip_suffix(".git").unwrap_or(path)
    ))
}

fn git_output(directory: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .current_dir(directory)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .args(args)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8(output.stdout).ok()?.trim().into())
}

fn checkout_origin(root: &Path) -> Option<(String, String)> {
    if !git_output(root, &["rev-parse", "--show-prefix"])?.is_empty() {
        return None;
    }
    let remote = git_output(root, &["remote", "get-url", "origin"])?;
    Some((
        remote_identity(&remote)?,
        git_output(root, &["rev-parse", "HEAD"])?,
    ))
}

fn manual_matches(
    selector: &Path,
    originals: &[String],
    sources: &[CatalogSource],
    rows: &[ModuleRow],
) -> BTreeMap<(usize, String), String> {
    let mut matches: BTreeMap<(usize, String), Vec<&String>> = BTreeMap::new();
    let mut origins = BTreeMap::new();
    let mut fetched = BTreeMap::new();
    for original in originals {
        let absolute = Path::new(original);
        let mut candidates = Vec::new();
        for row in rows {
            let relative = Path::new(&row.module.path);
            if !absolute.ends_with(relative) {
                continue;
            }
            let depth = relative.components().count();
            let Some(root) = absolute.ancestors().nth(depth) else {
                continue;
            };
            let source = &sources[row.repository];
            let mut same = root
                .canonicalize()
                .is_ok_and(|path| path == source.checkout);
            if !same
                && source.spec.mode == SourceMode::Pinned
                && let Some(revision) = source.revision.as_deref()
            {
                if root.starts_with("/nix/store") {
                    let fetched_root = fetched.entry(row.repository).or_insert_with(|| {
                        config::remote_root(
                            selector,
                            &Location::Remote {
                                url: source.spec.repository.clone(),
                                rev: revision.into(),
                            },
                        )
                        .ok()
                    });
                    same = fetched_root
                        .as_ref()
                        .is_some_and(|fetched| Path::new(fetched) == root);
                } else {
                    let actual = origins
                        .entry(root.to_path_buf())
                        .or_insert_with(|| checkout_origin(root));
                    same = actual.as_ref().is_some_and(|(url, commit)| {
                        commit == revision
                            && remote_identity(&source.spec.repository).as_ref() == Some(url)
                    });
                }
            }
            if same {
                candidates.push((row.repository, row.module.path.clone()));
            }
        }
        if let [key] = candidates.as_slice() {
            matches.entry(key.clone()).or_default().push(original);
        }
    }
    // A module with multiple manual imports may have different overrides per
    // import. Keep those original entries in the selector verbatim.
    matches
        .into_iter()
        .filter_map(|(key, paths)| {
            let [path] = paths.as_slice() else {
                return None;
            };
            Some((key, (*path).clone()))
        })
        .collect()
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
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
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
fn default_request() -> InstallRequest {
    InstallRequest {
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
    }
}

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
    let selector = env::var("NIXOS_BASE_CONFIG")
        .ok()
        .filter(|path| !path.is_empty())
        .or_else(|| (!saved.selector.is_empty()).then(|| saved.selector.clone()))
        .unwrap_or_else(|| home.join(".config/config.nix").display().to_string());
    let flake = env::var("NH_FLAKE")
        .ok()
        .filter(|path| !path.is_empty())
        .or_else(|| (!saved.flake.is_empty()).then(|| saved.flake.clone()))
        .unwrap_or_else(|| "/etc/nixos".into());
    let managed = if mode == Mode::Configure {
        Selector::read(Path::new(&selector)).ok().flatten()
    } else {
        None
    };
    let repositories = managed
        .as_ref()
        .and_then(Selector::repositories)
        .map(<[SourceSpec]>::to_vec)
        .unwrap_or_else(|| saved.repositories.clone());
    let repositories = if repositories.is_empty() {
        vec![SourceSpec {
            repository: "https://github.com/acup1/nixos-modules.git".into(),
            reference: "main".into(),
            mode: SourceMode::Pinned,
        }]
    } else {
        repositories
    };
    let request = default_request();
    let mut state = Session::new(mode, repositories, request, flake, selector);
    let mut controller = Controller {
        dir,
        sources: vec![],
        loaded: vec![],
        saved,
        selector: None,
        inspected_host: String::new(),
        manual: BTreeMap::new(),
    };
    if let Err(e) = controller.refresh(&mut state, false) {
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

#[cfg(test)]
mod controller_tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    struct Fixture(PathBuf);

    impl Fixture {
        fn new() -> Self {
            let time = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path =
                env::temp_dir().join(format!("nixos-controller-{}-{time}", std::process::id()));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn controller(directory: &Path) -> Controller {
        Controller {
            dir: directory.to_path_buf(),
            sources: vec![],
            loaded: vec![],
            saved: Saved::default(),
            selector: None,
            inspected_host: String::new(),
            manual: BTreeMap::new(),
        }
    }

    #[test]
    fn existing_manual_import_is_selected_and_stays_disabled_after_restart() {
        if !Command::new("nix")
            .arg("--version")
            .output()
            .is_ok_and(|output| output.status.success())
        {
            return;
        }
        let fixture = Fixture::new();
        let repo = fixture.0.join("catalog");
        let definition = repo.join("demo");
        fs::create_dir_all(&definition).unwrap();
        fs::write(definition.join("default.nix"), "{}").unwrap();
        fs::write(
            definition.join("module.json"),
            r#"{
            "schemaVersion": 1,
            "title": "Fixture",
            "description": "Test module",
            "layers": {"home": ["linux", "darwin"]}
        }"#,
        )
        .unwrap();
        git_output(&repo, &["init", "--quiet", "--template="]).unwrap();
        git_output(&repo, &["add", "--all"]).unwrap();
        git_output(
            &repo,
            &[
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.invalid",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "--quiet",
                "--message",
                "catalog fixture",
            ],
        )
        .unwrap();

        let path = fixture.0.join("manual.nix");
        let manual = format!(
            "{{ inputs }}: {{ modules = [ {{ _componentPath = {}; home.keepThis = true; }} {{ home.legacy = true; }} ]; }}",
            config::quote(definition.to_str().unwrap()),
        );
        fs::write(&path, &manual).unwrap();
        let spec = SourceSpec {
            repository: repo.to_str().unwrap().into(),
            reference: "HEAD".into(),
            mode: SourceMode::Local,
        };
        let mut state = Session::new(
            Mode::Configure,
            vec![spec.clone()],
            default_request(),
            String::new(),
            path.to_str().unwrap().into(),
        );
        let state_directory = fixture.0.join("state");
        let mut editor = controller(&state_directory);
        editor.refresh(&mut state, false).unwrap();
        assert!(state.modules[0].selected);
        assert!(state.modules[0].values.is_empty());
        editor.save(&mut state).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), manual);
        assert!(!path.with_extension("pre-installer.nix").exists());

        // Comparing the Git identity also finds a matching local manual
        // checkout when the corresponding catalog was resolved from HTTPS.
        git_output(
            &repo,
            &[
                "remote",
                "add",
                "origin",
                "git@example.invalid:owner/catalog.git",
            ],
        )
        .unwrap();
        let remote = CatalogSource {
            spec: SourceSpec {
                repository: "https://example.invalid/owner/catalog.git".into(),
                reference: "main".into(),
                mode: SourceMode::Pinned,
            },
            checkout: fixture.0.join("different-cached-repository"),
            revision: git_output(&repo, &["rev-parse", "HEAD"]),
            modules: vec![state.modules[0].module.clone()],
        };
        assert_eq!(
            manual_matches(
                &path,
                &[definition.display().to_string()],
                &[remote],
                &state.modules
            )
            .len(),
            1
        );

        state.modules[0].selected = false;
        editor.refresh(&mut state, false).unwrap();
        assert!(!state.modules[0].selected);
        editor.save(&mut state).unwrap();
        let managed = fs::read_to_string(&path).unwrap();

        let mut restored = Session::new(
            Mode::Configure,
            vec![],
            default_request(),
            String::new(),
            path.to_str().unwrap().into(),
        );
        let mut reload = controller(&state_directory);
        reload.refresh(&mut restored, false).unwrap();
        assert_eq!(restored.repositories, vec![spec]);
        assert!(!restored.modules[0].selected);
        reload.save(&mut restored).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), managed);
        assert_eq!(
            fs::read_to_string(path.with_extension("pre-installer.nix")).unwrap(),
            manual
        );
    }
}
