mod catalog;
mod config;
mod install;
mod read_command;
mod ui;

use catalog::{CatalogSource, Layer, OptionKind, Platform, SourceMode, SourceSpec};
use config::{Choice, Location, Selected, Selector};
use install::{BootMode, DiskLayout, FileSystem, InstallRequest};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    env, fs,
    io::{self, IsTerminal, Write},
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
    flake_override: String,
    selector: String,
}

fn detected_flake(selector: Option<&Selector>, saved: &Saved, path: &str) -> Result<String> {
    Ok(selector
        .map(Selector::flake_dir)
        .transpose()?
        .flatten()
        .filter(|reference| !reference.trim().is_empty())
        .unwrap_or_else(|| {
            if saved.selector == path {
                saved.flake.clone()
            } else {
                String::new()
            }
        }))
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
    sources_unavailable: bool,
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
        let selector = if state.mode == Mode::Configure {
            Some(
                Selector::read(selector_path)?
                    .ok_or_else(|| format!("Selector {} is missing.", selector_path.display()))?,
            )
        } else {
            None
        };
        state.detected_flake = if state.mode == Mode::Configure && state.flake.is_empty() {
            detected_flake(selector.as_ref(), &self.saved, &state.selector)?
        } else {
            String::new()
        };
        let same_selector = self.inspected_host == config::flake_reference(state.host_flake())
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
        let mut repositories = if !same_selector
            && (self.selector.is_some() || (!update && state.repositories.is_empty()))
        {
            selector
                .as_ref()
                .and_then(Selector::repositories)
                .map(<[SourceSpec]>::to_vec)
                .unwrap_or_else(|| state.repositories.clone())
        } else {
            state.repositories.clone()
        };
        let offline = state.mode == Mode::Configure && !update;
        let context = selector
            .as_ref()
            .map(|selector| selector.catalog_context(state.host_flake(), offline))
            .transpose()?
            .unwrap_or_default();
        add_local_sources(&context.sources, &context.originals, &mut repositories);
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
        let mut unavailable = vec![];
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
            let loaded = (|| {
                if let Some(source) = selector
                    .as_ref()
                    .filter(|selector| selector.installed())
                    .map(|_| installed_local_checkout(spec, Path::new("/etc/nixos")))
                    .transpose()?
                    .flatten()
                {
                    Ok(source)
                } else if update {
                    catalog::discover_updated(spec, &cache)
                } else if offline {
                    catalog::discover_cached(spec, &cache)
                } else {
                    catalog::discover(spec, &cache)
                }
            })();
            let source = match loaded {
                Ok(source) => source,
                Err(error) => {
                    unavailable.push(format!("{}: {error}", spec.repository));
                    self.loaded
                        .iter()
                        .position(|old| old == spec)
                        .and_then(|index| self.sources.get(index))
                        .cloned()
                        .unwrap_or_else(|| CatalogSource {
                            spec: spec.clone(),
                            checkout: PathBuf::new(),
                            revision: None,
                            modules: vec![],
                        })
                }
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

        let manual = manual_matches(selector_path, &context.originals, &sources, &rows, offline);
        if selector
            .as_ref()
            .is_some_and(|selector| !selector.current(selector_path))
        {
            return Err(
                "The selector changed during catalog loading. Refresh to use its latest contents."
                    .into(),
            );
        }
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
        self.inspected_host = config::flake_reference(state.host_flake()).to_owned();
        self.manual = manual;
        state.catalog_warning = if unavailable.is_empty() {
            None
        } else {
            Some(format!(
                "Fix or remove unavailable sources on Module sources and refresh before saving.

{}",
                unavailable.join(
                    "

"
                )
            ))
        };
        self.sources_unavailable = !unavailable.is_empty();
        state.modules = rows;
        state.plan = None;
        Ok(format!(
            "Loaded {} modules from {} repositories",
            state.modules.len(),
            self.sources.len()
        ))
    }
    fn choices(&self, state: &Session) -> Result<Vec<Choice>> {
        if self.sources_unavailable {
            return Err(
                "Fix unavailable module sources and refresh before applying changes.".into(),
            );
        }
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
                    SourceMode::Local => {
                        let installed = self
                            .selector
                            .as_ref()
                            .filter(|selector| selector.installed());
                        match installed.and_then(|_| {
                            installed_source_index(&s.checkout, Path::new("/etc/nixos"))
                        }) {
                            Some(index) => Location::TargetCopy(index),
                            None => Location::Local(s.checkout.clone()),
                        }
                    }
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
            flake: s.host_flake().to_owned(),
            flake_override: s.flake.clone(),
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
        if self.inspected_host != config::flake_reference(state.host_flake())
            || !selector.current(path)
        {
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
            let modules = config::selection(&self.locations(false)?, &new)?;
            if selector.installed() && !effective_user_is_root()? {
                selector.save_installed_with_sudo(
                    modules,
                    saved.selections.clone(),
                    saved.repositories.clone(),
                )?;
            } else {
                selector.save(
                    modules,
                    config::manual_changes(&changes)?,
                    masked,
                    saved.selections.clone(),
                    saved.repositories.clone(),
                )?;
            }
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
            saved.flake_override.clear();
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
        Ok(if matches!(state.install.layout, DiskLayout::InPlace) {
            "NixOS boot entry is ready. Reboot manually to enter NixOS and preserve the former root under /old-root.".into()
        } else {
            "Installation completed".into()
        })
    }
    fn check_local_install(&self, choices: &[Choice]) -> Result<()> {
        for (i, source) in self.sources.iter().enumerate() {
            if source.spec.mode != SourceMode::Local || !choices.iter().any(|c| c.repo == i) {
                continue;
            }
            if source.revision.is_none() {
                return Err(format!(
                    "Select a clean Git checkout for installation. {} is a plain catalog directory and can be used in Configure.",
                    source.checkout.display()
                ));
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
            Action::LoadCatalog => self.refresh(state, false),
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
                let target =
                    config::RebuildTarget::resolve(state.host_flake(), Path::new(&state.selector))?;
                self.save(state)?;
                target.rebuild(Path::new(&state.selector))?;
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
    let mut command = Command::new("git");
    command
        .current_dir(directory)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .args(args);
    let output = read_command::output(&mut command).ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8(output.stdout).ok()?.trim().into())
}

fn checkout_origin(root: &Path) -> Option<(String, String)> {
    if !root.join(".git").exists() || !git_output(root, &["rev-parse", "--show-prefix"])?.is_empty()
    {
        return None;
    }
    let remote = git_output(root, &["remote", "get-url", "origin"])?;
    Some((
        remote_identity(&remote)?,
        git_output(root, &["rev-parse", "HEAD"])?,
    ))
}

fn add_local_sources(
    explicit: &[String],
    originals: &[String],
    repositories: &mut Vec<SourceSpec>,
) {
    let mut registered: BTreeSet<PathBuf> = repositories
        .iter()
        .filter(|source| source.mode == SourceMode::Local)
        .filter_map(|source| {
            let path = PathBuf::from(&source.repository);
            let root = path.canonicalize().unwrap_or(path);
            let reference = source.reference.as_str();
            let working_tree = matches!(reference, "" | "HEAD")
                || (root.join(".git").exists()
                    && git_output(&root, &["symbolic-ref", "--quiet", "HEAD"]).is_some_and(
                        |head| {
                            head == reference || head.strip_prefix("refs/heads/") == Some(reference)
                        },
                    ));
            working_tree.then_some(root)
        })
        .collect();
    for repository in explicit {
        let path = PathBuf::from(repository);
        let identity = path.canonicalize().unwrap_or(path);
        if registered.insert(identity) {
            repositories.push(SourceSpec {
                repository: repository.clone(),
                reference: "HEAD".into(),
                mode: SourceMode::Local,
            });
        }
    }

    for original in originals {
        let path = Path::new(original);
        if !path.is_absolute() || path.starts_with("/nix/store") {
            continue;
        }
        let Ok(directory) = path.canonicalize() else {
            continue;
        };
        if !["default.nix", "module.json"]
            .iter()
            .all(|name| fs::symlink_metadata(directory.join(name)).is_ok_and(|file| file.is_file()))
            || registered
                .iter()
                .any(|root| root != &directory && directory.starts_with(root))
        {
            continue;
        }
        let Some(root) = directory
            .ancestors()
            .skip(1)
            .find(|path| path.join(".git").exists())
            .or_else(|| directory.parent())
        else {
            continue;
        };
        let Some(relative) = directory.strip_prefix(root).ok().and_then(Path::to_str) else {
            continue;
        };
        if catalog::validate_module_path(relative).is_ok()
            && let Some(repository) = root.to_str()
            && registered.insert(root.to_path_buf())
        {
            repositories.push(SourceSpec {
                repository: repository.into(),
                reference: "HEAD".into(),
                mode: SourceMode::Local,
            });
        }
    }
}

fn manual_matches(
    selector: &Path,
    originals: &[String],
    sources: &[CatalogSource],
    rows: &[ModuleRow],
    offline: bool,
) -> BTreeMap<(usize, String), String> {
    let mut matches: BTreeMap<(usize, String), Vec<&String>> = BTreeMap::new();
    let mut origins = BTreeMap::new();
    let mut fetched = BTreeMap::new();
    for original in originals {
        let absolute = Path::new(original);
        let mut local = Vec::new();
        let mut inferred = Vec::new();
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
            if root
                .canonicalize()
                .is_ok_and(|path| path == source.checkout)
            {
                local.push((row.repository, row.module.path.clone()));
                continue;
            }
            if source.spec.mode == SourceMode::Pinned
                && let Some(revision) = source.revision.as_deref()
            {
                let same = if root.starts_with("/nix/store") {
                    let fetched_root = fetched.entry(row.repository).or_insert_with(|| {
                        config::remote_root(
                            selector,
                            &Location::Remote {
                                url: source.spec.repository.clone(),
                                rev: revision.into(),
                            },
                            offline,
                        )
                        .ok()
                    });
                    fetched_root
                        .as_ref()
                        .is_some_and(|fetched| Path::new(fetched) == root)
                } else {
                    let actual = origins
                        .entry(root.to_path_buf())
                        .or_insert_with(|| checkout_origin(root));
                    actual.as_ref().is_some_and(|(url, commit)| {
                        commit == revision
                            && remote_identity(&source.spec.repository).as_ref() == Some(url)
                    })
                };
                if same {
                    inferred.push((row.repository, row.module.path.clone()));
                }
            }
        }
        let candidates = if local.is_empty() { inferred } else { local };
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

fn installed_source_index(checkout: &Path, system_directory: &Path) -> Option<usize> {
    let name = checkout
        .strip_prefix(system_directory.join("module-sources"))
        .ok()?
        .to_str()?;
    let index = name.parse::<usize>().ok()?;
    (index.to_string() == name).then_some(index)
}

fn installed_local_checkout(
    spec: &SourceSpec,
    system_directory: &Path,
) -> Result<Option<CatalogSource>> {
    use std::os::unix::fs::MetadataExt;

    if spec.mode != SourceMode::Local || !matches!(spec.reference.as_str(), "" | "HEAD") {
        return Ok(None);
    }
    let sources = system_directory.join("module-sources");
    let checkout = Path::new(&spec.repository);
    if installed_source_index(checkout, system_directory).is_none() {
        return Ok(None);
    }
    let git_directory = checkout.join(".git");
    for directory in [system_directory, &sources, checkout, &git_directory] {
        let stat = fs::symlink_metadata(directory)
            .map_err(|error| format!("Inspecting {}: {error}", directory.display()))?;
        if !stat.is_dir() || stat.uid() != 0 || stat.mode() & 0o022 != 0 {
            return Err(format!(
                "Expected a root-owned installed repository at {}",
                directory.display()
            ));
        }
    }
    let head = git_directory.join("HEAD");
    let stat = fs::symlink_metadata(&head)
        .map_err(|error| format!("Inspecting {}: {error}", head.display()))?;
    if !stat.is_file() || stat.uid() != 0 || stat.mode() & 0o022 != 0 {
        return Err(format!(
            "Invalid installed repository HEAD at {}",
            head.display()
        ));
    }
    let revision = fs::read_to_string(&head)
        .map_err(|error| format!("Reading {}: {error}", head.display()))?;
    let revision = revision.trim();
    if !matches!(revision.len(), 40 | 64) || !revision.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return Err(format!("Expected a detached commit at {}", head.display()));
    }
    Ok(Some(CatalogSource {
        spec: spec.clone(),
        checkout: checkout.to_path_buf(),
        revision: Some(revision.into()),
        modules: catalog::read_checkout(checkout)?,
    }))
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

const HELP: &str = "nixos-installer\n\nOpens the configuration editor when a usable config.nix and host flake are present. Opens the installation wizard on Linux without an existing setup (root required to apply).\n\nUse NIXOS_BASE_CONFIG to select another config.nix. Its flakeDir supplies the host flake.\n  --version    Print the release version\n  --help       Show this help";

struct ExistingSetup {
    path: String,
    selector: Selector,
    flake_override: String,
}

#[cfg(test)]
impl ExistingSetup {
    fn validate(&self, saved: &Saved) -> Result<()> {
        let host = if self.flake_override.is_empty() {
            detected_flake(Some(&self.selector), saved, &self.path)?
        } else {
            self.flake_override.clone()
        };
        self.selector
            .catalog_context(&host, true)
            .map(|_| ())
            .map_err(|error| format!("Existing setup at {} is incomplete: {error}", self.path))
    }
}

fn path_present(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(format!("Inspecting {}: {error}", path.display())),
    }
}

fn existing_setup(
    home: &Path,
    system_config: &Path,
    saved: &Saved,
    selector_override: Option<&Path>,
    flake_override: Option<String>,
    can_install: bool,
) -> Result<Option<ExistingSetup>> {
    let home_selector = home.join(".config/config.nix");
    let installed_selector = system_config.join("config.nix");
    let saved_selector = Path::new(&saved.selector);
    let path = if let Some(path) = selector_override {
        Some(path)
    } else if path_present(&home_selector)? {
        Some(home_selector.as_path())
    } else if !saved.selector.is_empty() && path_present(saved_selector)? {
        Some(saved_selector)
    } else if path_present(&installed_selector)? {
        Some(installed_selector.as_path())
    } else {
        None
    };

    if let Some(path) = path {
        let name = path.to_str().ok_or("Selector path must be UTF-8")?;
        let selector = Selector::read(path)?
            .ok_or_else(|| format!("Selector {} is missing. Restore it or set NIXOS_BASE_CONFIG to an existing selector.", path.display()))?;
        let flake_override = flake_override
            .or_else(|| {
                (saved.selector == name && !saved.flake_override.is_empty())
                    .then(|| saved.flake_override.clone())
            })
            .unwrap_or_default();
        return Ok(Some(ExistingSetup {
            path: name.into(),
            selector,
            flake_override,
        }));
    }

    for marker in [
        home_selector.with_extension("pre-installer.nix"),
        installed_selector.with_extension("pre-installer.nix"),
        system_config.join("nixos-installer-state.json"),
    ] {
        if path_present(&marker)? {
            return Err(format!(
                "Found an existing setup at {}. Restore its config.nix or set NIXOS_BASE_CONFIG to the selector.",
                marker.display()
            ));
        }
    }
    if !saved.selector.is_empty() {
        return Err(format!(
            "Saved selector {} is missing. Restore it or set NIXOS_BASE_CONFIG to an existing selector.",
            saved_selector.display()
        ));
    }
    if flake_override.is_some() || !saved.flake.is_empty() || !saved.flake_override.is_empty() {
        return Err(
            "A host flake was set without a selector. Restore config.nix or set NIXOS_BASE_CONFIG."
                .into(),
        );
    }
    if !can_install {
        return Err("No configuration selector found. Create config.nix or set NIXOS_BASE_CONFIG. The installation wizard requires Linux.".into());
    }
    Ok(None)
}

fn read_saved(path: &Path) -> Result<Option<Saved>> {
    match fs::read(path) {
        Ok(data) => serde_json::from_slice(&data)
            .map(Some)
            .map_err(|error| format!("Invalid saved state at {}: {error}", path.display())),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!(
            "Reading saved state at {}: {error}",
            path.display()
        )),
    }
}

const INSTALL_TOOLS: &str = "NIXOS_INSTALLER_NIX_SHELL";
const SYSTEM_PATH: &str =
    "/run/current-system/sw/bin:/nix/var/nix/profiles/default/bin:/usr/sbin:/usr/bin:/sbin:/bin";

fn effective_user_is_root() -> Result<bool> {
    let id = ["/usr/bin/id", "/bin/id", "/run/current-system/sw/bin/id"]
        .into_iter()
        .find(|path| Path::new(path).is_file())
        .ok_or("Cannot locate the system id program.")?;
    let current = Command::new(id)
        .arg("-u")
        .output()
        .map_err(|error| format!("Reading current user: {error}"))?;
    if !current.status.success() {
        return Err("Cannot determine the current user.".into());
    }
    Ok(current.stdout == b"0\n")
}

fn prepare_install_process() -> Result<bool> {
    let program = env::current_exe()
        .map_err(|error| format!("Cannot locate the installer executable: {error}"))?;
    if !effective_user_is_root()? {
        let sudo = ["/run/wrappers/bin/sudo", "/usr/bin/sudo"]
            .into_iter()
            .find(|path| Path::new(path).is_file())
            .ok_or_else(|| {
                format!(
                    "The wizard needs root. Run {} from a root shell.",
                    program.display()
                )
            })?;
        eprint!(
            "The installation wizard needs root to prepare disks and boot entries. Continue with sudo? [y/N] "
        );
        io::stderr().flush().map_err(|error| error.to_string())?;
        let mut answer = String::new();
        io::stdin()
            .read_line(&mut answer)
            .map_err(|error| error.to_string())?;
        if !matches!(answer.trim(), "y" | "Y" | "yes" | "YES") {
            return Ok(true);
        }
        let mut command = Command::new(sudo);
        command
            .env_clear()
            .env("PATH", SYSTEM_PATH)
            .args(["--"])
            .arg(&program);
        if let Some(term) = env::var_os("TERM") {
            command.env("TERM", term);
        }
        let status = command
            .status()
            .map_err(|error| format!("Starting the installation wizard with sudo: {error}"))?;
        return if status.success() {
            Ok(true)
        } else {
            Err(format!("The elevated installer exited with {status}."))
        };
    }

    if env::var_os(INSTALL_TOOLS).is_some() {
        return Ok(false);
    }
    let live_bin = Path::new("/run/current-system/sw/bin");
    let native = Path::new("/etc/NIXOS").is_file()
        && [
            "nixos-install",
            "nixos-generate-config",
            "nixos-enter",
            "lsblk",
            "git",
        ]
        .iter()
        .all(|tool| live_bin.join(tool).is_file());
    let mut command = if native {
        Command::new(&program)
    } else {
        use std::os::unix::fs::MetadataExt;
        let nix = ["/run/current-system/sw/bin/nix", "/nix/var/nix/profiles/default/bin/nix", "/usr/bin/nix", "/usr/local/bin/nix"]
            .into_iter().find(|path| fs::metadata(path).is_ok_and(|metadata| metadata.is_file() && metadata.uid() == 0 && metadata.mode() & 0o022 == 0))
            .ok_or("Install the official multi-user Nix package manager before starting the Linux installation wizard. The installer uses root-owned Nix tools.")?;
        let mut nix = Command::new(nix);
        nix.args([
            "--extra-experimental-features",
            "nix-command flakes",
            "shell",
        ]);
        for package in [
            "nixos-install-tools",
            "util-linux",
            "dosfstools",
            "e2fsprogs",
            "btrfs-progs",
            "xfsprogs",
            "cryptsetup",
            "mkpasswd",
            "systemd",
            "git",
        ] {
            nix.arg(format!("github:NixOS/nixpkgs/nixos-unstable#{package}"));
        }
        nix.arg("--command").arg(&program);
        nix
    };
    // This process either came from sudo's clean environment or is a direct
    // root invocation. Resolve all tool names from system and Nix shell bins.
    let status = command
        .env("PATH", SYSTEM_PATH)
        .env(INSTALL_TOOLS, "1")
        .status()
        .map_err(|error| format!("Preparing Linux installation tools: {error}"))?;
    if status.success() {
        Ok(true)
    } else {
        Err(format!(
            "The Linux installation wizard exited with {status}."
        ))
    }
}

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
    match args
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
        [config::SAVE_INSTALLED_ARG] => {
            if !effective_user_is_root()? {
                return Err("Saving the installed selector requires administrator access.".into());
            }
            return config::save_installed_from_stdin();
        }
        [] => {}
        _ => return Err(HELP.into()),
    };
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return Err("Open an interactive terminal, or use --help.".into());
    }
    if cfg!(target_os = "linux") && path_present(Path::new("/.nixos-installer-migrate"))? {
        return Err(
            "In-place installation is prepared. Boot the new NixOS entry before configuring it."
                .into(),
        );
    }
    let home = env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or("Missing HOME")?;
    let dir = env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".local/state"))
        .join("nixos-installer");
    let system_config = Path::new("/etc/nixos");
    let saved = match read_saved(&dir.join("state.json"))? {
        Some(saved) => saved,
        None => read_saved(&system_config.join("nixos-installer-state.json"))?.unwrap_or_default(),
    };
    let selector_override = env::var_os("NIXOS_BASE_CONFIG")
        .filter(|path| !path.is_empty())
        .map(PathBuf::from);
    let flake_override = env::var("NH_FLAKE").ok().filter(|path| !path.is_empty());
    let setup = existing_setup(
        &home,
        system_config,
        &saved,
        selector_override.as_deref(),
        flake_override,
        cfg!(target_os = "linux"),
    )?;
    let repositories = setup
        .as_ref()
        .and_then(|setup| setup.selector.repositories())
        .map(<[SourceSpec]>::to_vec)
        .unwrap_or_else(|| saved.repositories.clone());
    let repositories = if setup.is_none() && repositories.is_empty() {
        vec![SourceSpec {
            repository: "https://github.com/acup1/nixos-modules.git".into(),
            reference: "main".into(),
            mode: SourceMode::Pinned,
        }]
    } else {
        repositories
    };
    let (mode, flake, selector) = match setup {
        Some(setup) => (Mode::Configure, setup.flake_override, setup.path),
        None => (Mode::Install, String::new(), String::new()),
    };
    if mode == Mode::Install && prepare_install_process()? {
        return Ok(());
    }
    let mut request = default_request();
    if mode == Mode::Install {
        request.layout = install::default_install_layout();
        if let Some(username) = install::suggested_account() {
            request.username = username;
        }
    }
    let state = Session::new(mode, repositories, request, flake, selector);
    let controller = Controller {
        dir,
        sources: vec![],
        loaded: vec![],
        saved,
        selector: None,
        inspected_host: String::new(),
        manual: BTreeMap::new(),
        sources_unavailable: false,
    };
    ui::run(state, controller, Controller::action)
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
            sources_unavailable: false,
        }
    }

    #[test]
    fn installed_source_indices_preserve_relative_import_and_exclude_other_paths() {
        let system = Path::new("/etc/nixos");
        assert_eq!(
            installed_source_index(Path::new("/etc/nixos/module-sources/15"), system),
            Some(15)
        );
        for path in [
            "/etc/nixos/module-sources/015",
            "/etc/nixos/module-sources/0/child",
            "/etc/nixos/module-sources/../0",
            "/tmp/module-sources/0",
        ] {
            assert_eq!(installed_source_index(Path::new(path), system), None);
        }
    }

    #[test]
    fn startup_guides_only_unconfigured_linux_and_never_falls_back_from_partial_setup() {
        let fixture = Fixture::new();
        let home = fixture.0.join("home");
        let etc = fixture.0.join("etc-nixos");
        fs::create_dir(&home).unwrap();
        fs::create_dir(&etc).unwrap();
        assert!(
            existing_setup(&home, &etc, &Saved::default(), None, None, true)
                .unwrap()
                .is_none()
        );
        assert!(
            existing_setup(&home, &etc, &Saved::default(), None, None, false)
                .err()
                .unwrap()
                .contains("requires Linux")
        );

        let explicit = home.join("missing.nix");
        assert!(
            existing_setup(&home, &etc, &Saved::default(), Some(&explicit), None, true)
                .err()
                .unwrap()
                .contains("missing.nix")
        );
        let saved = Saved {
            selector: explicit.display().to_string(),
            ..Saved::default()
        };
        assert!(
            existing_setup(&home, &etc, &saved, None, None, true)
                .err()
                .unwrap()
                .contains("is missing")
        );

        fs::write(etc.join("configuration.nix"), "{}").unwrap();
        fs::write(etc.join("hardware-configuration.nix"), "{}").unwrap();
        fs::write(etc.join("flake.nix"), "{}").unwrap();
        fs::create_dir_all(home.join("flakes/nixos-base")).unwrap();
        fs::write(home.join("flakes/nixos-base/flake.nix"), "{}").unwrap();
        assert!(
            existing_setup(&home, &etc, &Saved::default(), None, None, true)
                .unwrap()
                .is_none()
        );

        fs::write(etc.join("nixos-installer-state.json"), "{}").unwrap();
        assert!(
            existing_setup(&home, &etc, &Saved::default(), None, None, true)
                .err()
                .unwrap()
                .contains("nixos-installer-state.json")
        );
    }

    #[test]
    fn startup_requires_a_reachable_host_flake_and_evaluable_module_bundles() {
        if !Command::new("nix")
            .arg("--version")
            .output()
            .is_ok_and(|output| output.status.success())
        {
            return;
        }
        let fixture = Fixture::new();
        let home = fixture.0.join("home");
        let etc = fixture.0.join("etc-nixos");
        let host = fixture.0.join("host");
        fs::create_dir_all(home.join(".config")).unwrap();
        fs::create_dir(&etc).unwrap();
        fs::create_dir(&host).unwrap();
        fs::write(
            host.join("flake.nix"),
            r#"{
          outputs = {self}: {
            nixosConfigurations.machine = throw "must not build";
            darwinConfigurations.user-OSX = throw "must not build";
          };
        }"#,
        )
        .unwrap();
        let source = |modules: &str| {
            format!(
                "{{ ... }}: {{ username = \"user\"; hostName = \"machine\"; flakeDir = {}; {modules} }}",
                config::quote(host.to_str().unwrap())
            )
        };
        let installed = etc.join("config.nix");
        fs::write(&installed, source("modules = [];")).unwrap();
        let assess = || {
            let saved = Saved::default();
            let setup = existing_setup(&home, &etc, &saved, None, None, true)?;
            if let Some(setup) = &setup {
                setup.validate(&saved)?;
            }
            Ok::<_, String>(setup)
        };
        assert_eq!(assess().unwrap().unwrap().path, installed.to_str().unwrap());

        let user = home.join(".config/config.nix");
        fs::write(
            &user,
            source("modules = [ (import ./missing-module.nix) ];"),
        )
        .unwrap();
        assert!(assess().err().unwrap().contains("missing-module.nix"));
        fs::write(&user, source("")).unwrap();
        assert!(assess().err().unwrap().contains("must declare modules"));
        fs::write(&user, source("modules = [ {} ];")).unwrap();
        assert_eq!(assess().unwrap().unwrap().path, user.to_str().unwrap());
        let stale = Saved {
            selector: fixture.0.join("outdated.nix").display().to_string(),
            ..Saved::default()
        };
        assert_eq!(
            existing_setup(&home, &etc, &stale, None, None, true)
                .unwrap()
                .unwrap()
                .path,
            user.to_str().unwrap()
        );

        fs::remove_file(host.join("flake.nix")).unwrap();
        assert!(assess().err().unwrap().contains("incomplete"));
    }

    #[test]
    fn selector_flake_dir_takes_priority_over_stale_saved_host() {
        if !Command::new("nix")
            .arg("--version")
            .output()
            .is_ok_and(|output| output.status.success())
        {
            return;
        }
        let fixture = Fixture::new();
        let path = fixture.0.join("config.nix");
        fs::write(fixture.0.join("flake.nix"), "{ outputs = { self }: {}; }").unwrap();
        fs::write(
            &path,
            "{ inputs }: { flakeDir = toString ./.; modules = []; }",
        )
        .unwrap();
        let mut state = Session::new(
            Mode::Configure,
            vec![],
            default_request(),
            String::new(),
            path.display().to_string(),
        );
        let mut editor = controller(&fixture.0.join("state"));
        editor.saved.selector = state.selector.clone();
        editor.saved.flake = "/obsolete-host".into();

        editor.refresh(&mut state, false).unwrap();
        assert_eq!(state.host_flake(), fixture.0.to_str().unwrap());
        state.flake = format!("{}#another", state.host_flake());
        editor.save(&mut state).unwrap();
        assert_eq!(editor.saved.flake_override, state.flake);
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

        let unrelated_path = fixture.0.join("unrelated");
        git_output(
            &fixture.0,
            &[
                "clone",
                "--quiet",
                "--no-hardlinks",
                "--",
                repo.to_str().unwrap(),
                unrelated_path.to_str().unwrap(),
            ],
        )
        .unwrap();
        let unrelated = SourceSpec {
            repository: unrelated_path.canonicalize().unwrap().display().to_string(),
            reference: "HEAD".into(),
            mode: SourceMode::Local,
        };

        let path = fixture.0.join("manual.nix");
        fs::write(fixture.0.join("flake.nix"), "{ outputs = inputs: {}; }").unwrap();
        let manual = format!(
            "{{ inputs }}: {{ flakeDir = {}; modules = [ {{ _componentPath = {}; home.keepThis = true; }} {{ home.legacy = true; }} ]; }}",
            config::quote(fixture.0.to_str().unwrap()),
            config::quote(definition.to_str().unwrap()),
        );
        fs::write(&path, &manual).unwrap();
        let spec = SourceSpec {
            repository: repo.canonicalize().unwrap().display().to_string(),
            reference: "HEAD".into(),
            mode: SourceMode::Local,
        };
        let mut state = Session::new(
            Mode::Configure,
            vec![unrelated.clone()],
            default_request(),
            String::new(),
            path.to_str().unwrap().into(),
        );
        let state_directory = fixture.0.join("state");
        let mut editor = controller(&state_directory);
        editor.refresh(&mut state, false).unwrap();
        assert_eq!(state.repositories, vec![unrelated.clone(), spec.clone()]);
        assert!(!state.modules[0].selected);
        assert!(state.modules[1].selected);
        assert!(state.modules[1].values.is_empty());
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
        let mut remote = CatalogSource {
            spec: SourceSpec {
                repository: "https://example.invalid/owner/catalog.git".into(),
                reference: "main".into(),
                mode: SourceMode::Pinned,
            },
            checkout: fixture.0.join("different-cached-repository"),
            revision: git_output(&repo, &["rev-parse", "HEAD"]),
            modules: vec![state.modules[1].module.clone()],
        };
        let path_names = [definition.display().to_string()];
        let rows = &state.modules;
        assert_eq!(
            manual_matches(&path, &path_names, &[remote.clone()], &rows[..1], true).len(),
            1
        );
        assert!(
            manual_matches(
                &path,
                &path_names,
                &[remote.clone(), editor.sources[1].clone()],
                rows,
                true,
            )
            .contains_key(&(1, "demo".into()))
        );
        remote.spec.reference = "1234567890123456789012345678901234567890".into();
        remote.revision = Some(remote.spec.reference.clone());
        let mut pinned_sources = vec![remote.spec.clone()];
        add_local_sources(&[], &path_names, &mut pinned_sources);
        assert_eq!(pinned_sources, vec![remote.spec.clone(), spec.clone()]);
        let result = manual_matches(
            &path,
            &path_names,
            &[remote, editor.sources[1].clone()],
            rows,
            true,
        );
        assert_eq!(
            result.keys().cloned().collect::<Vec<_>>(),
            vec![(1, "demo".into())]
        );

        state.modules[1].selected = false;
        editor.refresh(&mut state, false).unwrap();
        assert!(!state.modules[1].selected);
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
        assert_eq!(restored.repositories, vec![unrelated, spec]);
        assert!(restored.modules.iter().all(|row| !row.selected));
        reload.save(&mut restored).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), managed);
        assert_eq!(
            fs::read_to_string(path.with_extension("pre-installer.nix")).unwrap(),
            manual
        );
    }
    #[test]
    fn managed_plain_catalogs_restore_mask_and_discover_unselected_platform_modules() {
        if Command::new("nix").arg("--version").output().is_err() {
            return;
        }
        let fixture = Fixture::new();
        let root = fixture.0.canonicalize().unwrap();
        fs::write(root.join("flake.nix"), "{ outputs = inputs: {}; }").unwrap();
        let detected = root.join("auto catalog");
        let explicit = root.join("explicit catalog");
        let primary = detected.join("primary");
        let supported = explicit.join("available");
        let foreign = explicit.join("foreign");
        let native_layer = if cfg!(target_os = "macos") {
            ("darwin", "darwin")
        } else {
            ("nixos", "linux")
        };
        let foreign_layer = if cfg!(target_os = "macos") {
            ("nixos", "linux")
        } else {
            ("darwin", "darwin")
        };
        for (path, (layer, platform)) in [
            (&primary, native_layer),
            (&supported, native_layer),
            (&foreign, foreign_layer),
        ] {
            fs::create_dir_all(path).unwrap();
            fs::write(path.join("default.nix"), format!(
                "{{ _componentPath = builtins.dirOf (toString ./default.nix); {layer} = {{}}; }}"
            )).unwrap();
            fs::write(
                path.join("module.json"),
                serde_json::to_vec(&serde_json::json!({
                    "schemaVersion": 1,
                    "title": path.file_name().unwrap().to_str().unwrap(),
                    "description": "Catalog fixture",
                    "layers": {layer: [platform]}
                }))
                .unwrap(),
            )
            .unwrap();
        }
        let path = root.join("config.nix");
        fs::write(&path, format!(
            "{{ inputs }}: {{ flakeDir = {}; moduleSources = [ {} ]; modules = [ (import (builtins.toPath {})) ]; }}",
            config::quote(root.to_str().unwrap()),
            config::quote(explicit.to_str().unwrap()),
            config::quote(primary.join("default.nix").to_str().unwrap()),
        )).unwrap();
        let manual = Selector::read(&path).unwrap().unwrap();
        // A v2 selector which never recorded this private catalog as a repository.
        let mask = BTreeSet::from([primary.to_str().unwrap().into()]);
        manual
            .save("[]".into(), "[]".into(), mask.clone(), vec![], vec![])
            .unwrap();
        let start = || {
            Session::new(
                Mode::Configure,
                vec![],
                default_request(),
                String::new(),
                path.to_str().unwrap().into(),
            )
        };
        let mut state = start();
        let mut editor = controller(&root.join("state"));
        editor.refresh(&mut state, false).unwrap();
        assert_eq!(state.repositories.len(), 2);
        assert!(
            editor
                .sources
                .iter()
                .all(|source| source.revision.is_none())
        );
        assert_eq!(
            state
                .modules
                .iter()
                .map(|row| (row.module.path.as_str(), row.selected))
                .collect::<Vec<_>>(),
            [("available", false), ("primary", false)]
        );
        state.modules[0].selected = true;
        editor.refresh(&mut state, true).unwrap();
        assert!(
            editor
                .check_local_install(&editor.choices(&state).unwrap())
                .unwrap_err()
                .contains("plain catalog")
        );
        editor.save(&mut state).unwrap();
        drop(editor);

        let mut reopened = start();
        let mut editor = controller(&root.join("state"));
        editor.refresh(&mut reopened, false).unwrap();
        assert!(reopened.modules[0].selected);
        assert!(!reopened.modules[1].selected);
        assert_eq!(Selector::read(&path).unwrap().unwrap().masked(), mask);
        let selector = fs::read_to_string(&path).unwrap();
        assert!(selector.contains(&config::quote(explicit.to_str().unwrap())));
        assert!(!selector.contains("builtins.fetchGit"));
    }

    #[test]
    fn local_sources_reuse_the_checked_out_branch_and_add_head_for_other_refs() {
        let fixture = Fixture::new();
        let root = fixture.0.canonicalize().unwrap();
        let module = root.join("demo");
        fs::create_dir(&module).unwrap();
        fs::write(module.join("default.nix"), "{}").unwrap();
        fs::write(module.join("module.json"), "{}").unwrap();
        git_output(
            &root,
            &["init", "--quiet", "--template=", "--initial-branch=current"],
        )
        .unwrap();
        let source = |reference: &str| SourceSpec {
            repository: root.to_str().unwrap().into(),
            reference: reference.into(),
            mode: SourceMode::Local,
        };
        let original = [module.to_str().unwrap().to_owned()];
        let mut sources = vec![source("current")];
        add_local_sources(&[], &original, &mut sources);
        assert_eq!(sources, [source("current")]);
        let mut sources = vec![source("other")];
        add_local_sources(&[], &original, &mut sources);
        assert_eq!(sources, [source("other"), source("HEAD")]);
    }

    #[test]
    fn uncached_remote_leaves_local_catalog_browsable_and_blocks_writes() {
        if Command::new("nix").arg("--version").output().is_err() {
            return;
        }
        let fixture = Fixture::new();
        let root = fixture.0.canonicalize().unwrap();
        fs::write(root.join("flake.nix"), "{ outputs = inputs: {}; }").unwrap();
        let catalog = root.join("plain");
        let module = catalog.join("demo");
        fs::create_dir_all(&module).unwrap();
        fs::write(module.join("default.nix"), "{ home = {}; }").unwrap();
        fs::write(
            module.join("module.json"),
            r#"{
            "schemaVersion": 1,
            "title": "Demo",
            "description": "Local catalog",
            "layers": { "home": ["linux", "darwin"] }
        }"#,
        )
        .unwrap();
        let selector = root.join("config.nix");
        let original = format!(
            "{{ inputs }}: {{ flakeDir = {}; moduleSources = [ {} ]; modules = []; }}",
            config::quote(root.to_str().unwrap()),
            config::quote(catalog.to_str().unwrap())
        );
        fs::write(&selector, &original).unwrap();
        let missing = SourceSpec {
            repository: "https://no-network.invalid/catalog.git".into(),
            reference: "1234567890123456789012345678901234567890".into(),
            mode: SourceMode::Pinned,
        };
        let mut state = Session::new(
            Mode::Configure,
            vec![missing],
            default_request(),
            String::new(),
            selector.to_str().unwrap().into(),
        );
        let mut editor = controller(&root.join("state"));
        editor.refresh(&mut state, false).unwrap();
        assert!(
            state
                .catalog_warning
                .as_deref()
                .unwrap()
                .contains("Refresh catalog")
        );
        assert_eq!(state.modules.len(), 1);
        assert_eq!(state.modules[0].repository, 1);
        assert!(editor.save(&mut state).unwrap_err().contains("unavailable"));
        assert_eq!(fs::read_to_string(selector).unwrap(), original);
    }
}
