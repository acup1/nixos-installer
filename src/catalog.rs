use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::hash_map::DefaultHasher;
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::fs::{self, File};
use std::hash::{Hash, Hasher};
use std::io::Read;
use std::net::Ipv6Addr;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceMode {
    Local,
    Pinned,
}

#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SourceSpec {
    pub repository: String,
    pub reference: String,
    pub mode: SourceMode,
}

#[derive(Clone, Debug)]
pub struct CatalogSource {
    pub spec: SourceSpec,
    pub checkout: PathBuf,
    pub revision: Option<String>,
    pub modules: Vec<CatalogModule>,
}

#[derive(Clone, Debug)]
pub struct CatalogModule {
    pub path: String,
    pub manifest: Manifest,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Layer {
    Nixos,
    Darwin,
    Home,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Platform {
    Linux,
    Darwin,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OptionKind {
    Bool,
    Integer,
    String,
    StringList,
    Packages,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ManifestOption {
    pub layer: Layer,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub platforms: Option<Vec<Platform>>,
    pub path: Vec<String>,
    pub title: String,
    pub description: String,
    #[serde(rename = "type")]
    pub kind: OptionKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<Value>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Manifest {
    pub schema_version: u32,
    pub title: String,
    pub description: String,
    pub layers: BTreeMap<Layer, Vec<Platform>>,
    #[serde(default)]
    pub options: Vec<ManifestOption>,
}

#[derive(Deserialize, Serialize)]
struct CacheRecord {
    spec: SourceSpec,
    revision: String,
}

const MAX_JSON_SIZE: u64 = 256 * 1024;
static NEXT_CHECKOUT: AtomicU64 = AtomicU64::new(0);

impl OptionKind {
    pub fn accepts(self, value: &Value) -> bool {
        match self {
            Self::Bool => value.is_boolean(),
            Self::Integer => value.as_i64().is_some(),
            Self::String => value.is_string(),
            Self::StringList | Self::Packages => value.as_array().is_some_and(|entries| {
                entries.iter().all(|entry| {
                    entry.as_str().is_some_and(|text| {
                        self == Self::StringList || text.split('.').all(valid_package_segment)
                    })
                })
            }),
        }
    }
}

impl Manifest {
    pub fn supports(&self, platform: Platform) -> bool {
        self.layers
            .values()
            .any(|platforms| platforms.contains(&platform))
    }

    fn validate(&self) -> Result<(), String> {
        if self.schema_version != 1 {
            return Err(format!(
                "Версия схемы {} не поддерживается",
                self.schema_version
            ));
        }
        validate_label(&self.title, "title")?;
        validate_label(&self.description, "description")?;
        if self.layers.is_empty() {
            return Err("Укажите хотя бы один слой".into());
        }
        for (layer, platforms) in &self.layers {
            if platforms.is_empty()
                || platforms.iter().collect::<BTreeSet<_>>().len() != platforms.len()
            {
                return Err(format!("Неверные платформы слоя {layer:?}"));
            }
            match layer {
                Layer::Nixos if platforms.as_slice() != [Platform::Linux] => {
                    return Err("Слой nixos поддерживает только Linux".into());
                }
                Layer::Darwin if platforms.as_slice() != [Platform::Darwin] => {
                    return Err("Слой darwin поддерживает только macOS".into());
                }
                _ => {}
            }
        }

        let mut paths: BTreeSet<(Layer, &[String])> = BTreeSet::new();
        let mut keys = BTreeSet::new();
        for option in &self.options {
            validate_label(&option.title, "options.title")?;
            validate_label(&option.description, "options.description")?;
            if !self.layers.contains_key(&option.layer)
                || option.path.is_empty()
                || !option.path.iter().all(|segment| valid_segment(segment))
            {
                return Err(format!("Неверный путь параметра {:?}", option.path));
            }
            if let Some(platforms) = &option.platforms
                && (platforms.is_empty()
                    || platforms.iter().collect::<BTreeSet<_>>().len() != platforms.len()
                    || !platforms
                        .iter()
                        .all(|p| self.layers[&option.layer].contains(p)))
            {
                return Err(format!("Неверные платформы параметра {:?}", option.path));
            }
            if !paths.insert((option.layer, &option.path))
                || !keys.insert((option.layer, option.path.join(".")))
            {
                return Err(format!("Параметр {:?} указан дважды", option.path));
            }
            if option
                .default
                .as_ref()
                .is_some_and(|value| !option.kind.accepts(value) || !safe_default(value))
            {
                return Err(format!(
                    "Значение по умолчанию не соответствует типу {:?}",
                    option.path
                ));
            }
        }
        let mut paths = paths.iter();
        if let Some(mut previous) = paths.next() {
            for path in paths {
                if previous.0 == path.0 && path.1.starts_with(previous.1) {
                    return Err(format!(
                        "Пути параметров {:?} и {:?} пересекаются",
                        previous.1, path.1
                    ));
                }
                previous = path;
            }
        }
        Ok(())
    }
}

fn validate_label(text: &str, field: &str) -> Result<(), String> {
    if text.trim().is_empty() || text.chars().any(char::is_control) {
        return Err(format!(
            "Поле {field} должно содержать непустой текст без управляющих символов"
        ));
    }
    Ok(())
}

fn safe_default(value: &Value) -> bool {
    match value {
        Value::String(text) => !text.chars().any(char::is_control),
        Value::Array(items) => items.iter().all(safe_default),
        _ => true,
    }
}

fn valid_package_segment(segment: &str) -> bool {
    !segment.is_empty()
        && segment
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'+'))
}

fn valid_segment(segment: &str) -> bool {
    let mut bytes = segment.bytes();
    bytes
        .next()
        .is_some_and(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

pub fn validate_module_path(path: &str) -> Result<(), String> {
    if path.split('/').all(valid_segment) {
        Ok(())
    } else {
        Err(format!("Неверный относительный путь модуля: {path:?}"))
    }
}

pub fn validate_selection<'a>(
    source: &'a CatalogSource,
    paths: &[String],
) -> Result<Vec<&'a CatalogModule>, String> {
    let mut seen = BTreeSet::new();
    paths
        .iter()
        .map(|path| {
            validate_module_path(path)?;
            if !seen.insert(path) {
                return Err(format!("Модуль {path} выбран дважды"));
            }
            source
                .modules
                .iter()
                .find(|module| &module.path == path)
                .ok_or_else(|| format!("Модуль {path} отсутствует в каталоге"))
        })
        .collect()
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<T, String> {
    let metadata =
        fs::symlink_metadata(path).map_err(|error| format!("{}: {error}", path.display()))?;
    if !metadata.is_file() || metadata.len() > MAX_JSON_SIZE {
        return Err(format!(
            "Ожидался обычный JSON-файл размером до 256 КиБ: {}",
            path.display()
        ));
    }
    let mut json = Vec::new();
    File::open(path)
        .and_then(|file| file.take(MAX_JSON_SIZE + 1).read_to_end(&mut json))
        .map_err(|error| format!("{}: {error}", path.display()))?;
    if json.len() as u64 > MAX_JSON_SIZE {
        return Err(format!("Слишком большой JSON-файл: {}", path.display()));
    }
    serde_json::from_slice(&json).map_err(|error| format!("{}: {error}", path.display()))
}

fn visit_directory(
    root: &Path,
    directory: &Path,
    modules: &mut Vec<CatalogModule>,
) -> Result<(), String> {
    for entry in
        fs::read_dir(directory).map_err(|error| format!("{}: {error}", directory.display()))?
    {
        let entry = entry.map_err(|error| format!("{}: {error}", directory.display()))?;
        let file_type = entry
            .file_type()
            .map_err(|error| format!("{}: {error}", entry.path().display()))?;
        let path = entry.path();
        if entry.file_name() == "module.json" {
            let relative = directory
                .strip_prefix(root)
                .ok()
                .and_then(Path::to_str)
                .ok_or_else(|| format!("Неверный каталог модуля: {}", directory.display()))?;
            validate_module_path(relative)?;
            let definition = directory.join("default.nix");
            let metadata = fs::symlink_metadata(&definition)
                .map_err(|error| format!("{}: {error}", definition.display()))?;
            if !metadata.is_file() {
                return Err(format!("Ожидался обычный файл: {}", definition.display()));
            }
            let manifest: Manifest = read_json(&path)?;
            manifest
                .validate()
                .map_err(|error| format!("{}: {error}", path.display()))?;
            modules.push(CatalogModule {
                path: relative.into(),
                manifest,
            });
        } else if file_type.is_dir() && !entry.file_name().to_string_lossy().starts_with('.') {
            visit_directory(root, &path, modules)?;
        }
    }
    Ok(())
}

pub fn read_checkout(checkout: &Path) -> Result<Vec<CatalogModule>, String> {
    ensure_directory(checkout)?;
    let mut modules = Vec::new();
    visit_directory(checkout, checkout, &mut modules)?;
    modules.sort_unstable_by(|left, right| left.path.cmp(&right.path));
    Ok(modules)
}

fn run_git<I, S>(directory: &Path, arguments: I, local_transport: bool) -> Result<String, String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let mut arguments = arguments.into_iter().peekable();
    let mut command = Command::new("git");
    // Fetch may use a configured credential helper. A checkout needs no
    // system or global configuration and must avoid running smudge filters.
    if !arguments
        .peek()
        .is_some_and(|argument| matches!(argument.as_ref().to_str(), Some("fetch" | "ls-remote")))
    {
        command
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null");
    }
    command
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .env_remove("GIT_COMMON_DIR")
        .env_remove("GIT_OBJECT_DIRECTORY")
        .env_remove("GIT_ALTERNATE_OBJECT_DIRECTORIES")
        .env_remove("GIT_CONFIG_PARAMETERS")
        .env_remove("GIT_CONFIG_COUNT")
        .env("GIT_TERMINAL_PROMPT", "0")
        .arg("-C")
        .arg(directory)
        .args([
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "core.fsmonitor=false",
            "-c",
            "protocol.allow=never",
            "-c",
            "protocol.https.allow=always",
            "-c",
            "protocol.ssh.allow=always",
            "-c",
            if local_transport {
                "protocol.file.allow=always"
            } else {
                "protocol.file.allow=never"
            },
        ])
        .args(arguments);

    let output = command
        .output()
        .map_err(|error| format!("Не удалось запустить Git: {error}"))?;
    if !output.status.success() {
        let detail = String::from_utf8_lossy(&output.stderr);
        return Err(format!("Git завершился с ошибкой: {}", detail.trim()));
    }
    String::from_utf8(output.stdout).map_err(|_| "Git вернул данные вне UTF-8".into())
}

fn is_commit_id(reference: &str) -> bool {
    matches!(reference.len(), 40 | 64) && reference.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn reference_selector(reference: &str, directory: &Path) -> Result<String, String> {
    if reference.is_empty() || reference == "HEAD" || is_commit_id(reference) {
        return Ok(if reference.is_empty() {
            "HEAD".into()
        } else {
            reference.into()
        });
    }
    if reference.starts_with('-')
        || reference.starts_with("refs/")
            && !reference.starts_with("refs/heads/")
            && !reference.starts_with("refs/tags/")
    {
        return Err(format!("Неверная ветка или ссылка Git: {reference:?}"));
    }
    let selector = if reference.starts_with("refs/") {
        reference.into()
    } else {
        format!("refs/heads/{reference}")
    };
    run_git(directory, ["check-ref-format", &selector], false)
        .map_err(|_| format!("Неверная ветка или ссылка Git: {reference:?}"))?;
    Ok(selector)
}

fn commit_at(checkout: &Path, reference: &str) -> Result<String, String> {
    let result = run_git(
        checkout,
        [
            "rev-parse",
            "--verify",
            "--end-of-options",
            &format!("{reference}^{{commit}}"),
        ],
        false,
    )?;
    let revision = result.trim();
    if !is_commit_id(revision) {
        return Err(format!(
            "Git не вернул идентификатор коммита для {reference}"
        ));
    }
    Ok(revision.into())
}

fn valid_host(host: &str) -> bool {
    valid_segment(host) && !host.starts_with('-')
}

fn valid_authority(authority: &str) -> bool {
    let host_port = if let Some((user, tail)) = authority.rsplit_once('@') {
        if !valid_segment(user) || user.starts_with('-') {
            return false;
        }
        tail
    } else {
        authority
    };

    let port = if let Some(ipv6) = host_port.strip_prefix('[') {
        let Some((host, suffix)) = ipv6.split_once(']') else {
            return false;
        };
        if host.parse::<Ipv6Addr>().is_err() {
            return false;
        }
        suffix
    } else {
        let host = host_port.split(':').next().unwrap_or_default();
        if !valid_host(host) {
            return false;
        }
        &host_port[host.len()..]
    };
    port.is_empty()
        || port
            .strip_prefix(':')
            .is_some_and(|port| port.parse::<u16>().is_ok_and(|port| port > 0))
}

fn remote_url(repository: &str) -> Result<String, String> {
    if repository
        .chars()
        .any(|character| character.is_control() || character.is_whitespace())
    {
        return Err("URL репозитория содержит пробельный или управляющий символ".into());
    }
    if let Some(url) = repository
        .strip_prefix("https://")
        .or_else(|| repository.strip_prefix("ssh://"))
    {
        if url
            .split_once('/')
            .is_some_and(|(authority, path)| !path.is_empty() && valid_authority(authority))
        {
            return Ok(repository.into());
        }
    } else if let Some((host, path)) = repository.split_once(':')
        && host.split_once('@').is_some_and(|(user, host)| {
            valid_segment(user) && !user.starts_with('-') && valid_host(host)
        })
        && !path.trim_matches('/').is_empty()
    {
        return Ok(format!("ssh://{host}/{}", path.trim_start_matches('/')));
    }
    Err("Укажите локальный Git-репозиторий или URL HTTPS/SSH".into())
}

fn checkout_path(path: &Path) -> Result<PathBuf, String> {
    let canonical = path
        .canonicalize()
        .map_err(|error| format!("{}: {error}", path.display()))?;
    let prefix = run_git(&canonical, ["rev-parse", "--show-prefix"], false)?;
    let worktree = run_git(&canonical, ["rev-parse", "--is-inside-work-tree"], false)?;
    if !prefix.trim().is_empty() || worktree.trim() != "true" {
        return Err(format!(
            "Укажите корень Git-репозитория: {}",
            path.display()
        ));
    }
    Ok(canonical)
}

fn cache_id(spec: &SourceSpec, local_revision: Option<&str>) -> String {
    let mut hasher = DefaultHasher::new();
    (spec, local_revision).hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

fn ensure_directory(path: &Path) -> Result<(), String> {
    if !fs::symlink_metadata(path).is_ok_and(|metadata| metadata.is_dir()) {
        return Err(format!("Ожидался обычный каталог: {}", path.display()));
    }
    Ok(())
}

fn prepare_cache(
    staging: &Path,
    spec: &SourceSpec,
    selector: &str,
    repository: &str,
    local: bool,
) -> Result<(), String> {
    let checkout = staging.join("checkout");
    fs::create_dir(&checkout).map_err(|error| format!("{}: {error}", checkout.display()))?;
    run_git(&checkout, ["init", "--quiet", "--template="], false)?;
    run_git(
        &checkout,
        [
            "fetch",
            "--quiet",
            "--depth=1",
            "--no-tags",
            "--no-recurse-submodules",
            "--",
            repository,
            selector,
        ],
        local,
    )?;
    let revision = commit_at(&checkout, "FETCH_HEAD")?;
    run_git(
        &checkout,
        ["checkout", "--quiet", "--detach", &revision],
        false,
    )?;
    let record = CacheRecord {
        spec: spec.clone(),
        revision,
    };
    let json = serde_json::to_vec(&record).map_err(|error| error.to_string())?;
    let record_path = staging.join("source.json");
    fs::write(&record_path, json).map_err(|error| format!("{}: {error}", record_path.display()))
}

fn cached_checkout(
    spec: &SourceSpec,
    cache: &Path,
    selector: &str,
    repository: &str,
    local_revision: Option<&str>,
) -> Result<(PathBuf, String), String> {
    fs::create_dir_all(cache).map_err(|error| format!("{}: {error}", cache.display()))?;
    ensure_directory(cache)?;
    let cache = cache
        .canonicalize()
        .map_err(|error| format!("{}: {error}", cache.display()))?;
    let entry = cache.join(cache_id(spec, local_revision));
    let exists = match entry.symlink_metadata() {
        Ok(_) => true,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => return Err(format!("{}: {error}", entry.display())),
    };
    if !exists {
        let sequence = NEXT_CHECKOUT.fetch_add(1, Ordering::Relaxed);
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let staging = cache.join(format!(
            ".checkout-{}-{timestamp}-{sequence}",
            std::process::id()
        ));
        fs::create_dir(&staging).map_err(|error| format!("{}: {error}", staging.display()))?;
        let prepared = prepare_cache(
            &staging,
            spec,
            selector,
            repository,
            local_revision.is_some(),
        );
        let result = prepared.and_then(|()| match fs::rename(&staging, &entry) {
            Ok(()) => Ok(()),
            Err(_) if entry.is_dir() => Ok(()),
            Err(error) => Err(format!("{}: {error}", entry.display())),
        });
        if staging.is_dir() {
            let _ = fs::remove_dir_all(&staging);
        }
        result?;
    }
    ensure_directory(&entry)?;
    let record: CacheRecord = read_json(&entry.join("source.json"))?;
    if &record.spec != spec || !is_commit_id(&record.revision) {
        return Err(format!("Запись кеша повреждена: {}", entry.display()));
    }
    let checkout = entry.join("checkout");
    ensure_directory(&checkout)?;
    if spec.mode == SourceMode::Pinned {
        let status = run_git(
            &checkout,
            [
                "status",
                "--porcelain",
                "--untracked-files=all",
                "--ignored=matching",
                "--ignore-submodules=all",
            ],
            false,
        )?;
        if !status.is_empty() || commit_at(&checkout, "HEAD")? != record.revision {
            return Err(format!(
                "Закреплённая копия содержит локальные изменения: {}",
                checkout.display()
            ));
        }
    }
    Ok((checkout, record.revision))
}

/// Resolve moving remote references on an explicit refresh. Existing cached
/// checkouts stay at their original commits for running configurations.
pub fn discover_updated(spec: &SourceSpec, cache: &Path) -> Result<CatalogSource, String> {
    if Path::new(&spec.repository).exists() || is_commit_id(&spec.reference) {
        return discover(spec, cache);
    }
    let current = std::env::current_dir().map_err(|e| e.to_string())?;
    let selector = reference_selector(&spec.reference, &current)?;
    let url = remote_url(&spec.repository)?;
    let peeled = format!("{selector}^{{}}");
    let output = run_git(
        &current,
        ["ls-remote", "--exit-code", "--", &url, &selector, &peeled],
        false,
    )?;
    let refs: BTreeMap<_, _> = output
        .lines()
        .filter_map(|line| line.split_once('\t').map(|(rev, name)| (name, rev)))
        .collect();
    let rev = refs
        .get(peeled.as_str())
        .or_else(|| refs.get(selector.as_str()))
        .filter(|rev| is_commit_id(rev))
        .ok_or_else(|| format!("Git reference {selector} was not found"))?;
    let pinned = SourceSpec {
        repository: url,
        reference: (*rev).to_string(),
        mode: spec.mode,
    };
    let mut source = discover(&pinned, cache)?;
    source.spec.reference.clone_from(&spec.reference);
    Ok(source)
}

pub fn discover(spec: &SourceSpec, cache: &Path) -> Result<CatalogSource, String> {
    let mut spec = spec.clone();
    let current = std::env::current_dir().map_err(|error| error.to_string())?;
    let selector = reference_selector(&spec.reference, &current)?;
    let candidate = Path::new(&spec.repository);

    let (checkout, revision) = if candidate.exists() {
        if spec.mode == SourceMode::Pinned {
            return Err("Для закреплённого источника необходим удалённый URL Git".into());
        }
        let checkout = checkout_path(candidate)?;
        spec.repository = checkout
            .to_str()
            .ok_or("Путь к Git-репозиторию должен быть в UTF-8")?
            .into();
        let head =
            run_git(&checkout, ["symbolic-ref", "--quiet", "HEAD"], false).unwrap_or_default();
        let revision = commit_at(&checkout, &selector)?;
        if selector == "HEAD" || head.trim() == selector {
            (checkout, revision)
        } else {
            cached_checkout(&spec, cache, &revision, &spec.repository, Some(&revision))?
        }
    } else {
        spec.repository = remote_url(&spec.repository)?;
        cached_checkout(&spec, cache, &selector, &spec.repository, None)?
    };

    let modules = read_checkout(&checkout)?;
    Ok(CatalogSource {
        spec,
        checkout,
        revision: Some(revision),
        modules,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture(PathBuf);

    impl Fixture {
        fn new() -> Self {
            let timestamp = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let suffix = NEXT_CHECKOUT.fetch_add(1, Ordering::Relaxed);
            let root = std::env::temp_dir().join(format!(
                "nixos-catalog-{}-{timestamp}-{suffix}",
                std::process::id()
            ));
            fs::create_dir(&root).unwrap();
            Self(root)
        }

        fn repository(&self, name: &str) -> PathBuf {
            let repository = self.0.join(name);
            fs::create_dir(&repository).unwrap();
            run_git(
                &repository,
                ["init", "--quiet", "--template=", "--initial-branch=main"],
                false,
            )
            .unwrap();
            repository
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    const MANIFEST: &str = r#"{
        "schemaVersion": 1,
        "title": "Оконный менеджер",
        "description": "Графическая сессия",
        "layers": {"nixos": ["linux"], "home": ["linux", "darwin"]},
        "options": [{
            "layer": "nixos", "path": ["components", "wm", "packages"],
            "title": "Пакеты", "description": "Список пакетов",
            "type": "packages", "default": ["nerd-fonts.jetbrains-mono"]
        }]
    }"#;

    fn write_module(repository: &Path, module: &str, text: &str) {
        let component = repository.join(module);
        fs::create_dir_all(&component).unwrap();
        fs::write(component.join("module.json"), text).unwrap();
        fs::write(
            component.join("default.nix"),
            r#"throw "catalog must not evaluate this""#,
        )
        .unwrap();
    }

    fn commit(repository: &Path) -> String {
        run_git(repository, ["add", "--all"], false).unwrap();
        run_git(
            repository,
            [
                "-c",
                "user.name=Catalog Test",
                "-c",
                "user.email=catalog@example.invalid",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "--quiet",
                "--message",
                "Fixture",
            ],
            false,
        )
        .unwrap();
        commit_at(repository, "HEAD").unwrap()
    }

    fn local_source(repository: &Path, reference: &str) -> SourceSpec {
        SourceSpec {
            repository: repository.to_str().unwrap().into(),
            reference: reference.into(),
            mode: SourceMode::Local,
        }
    }

    #[test]
    fn discovers_multiple_checkouts_without_evaluating_nix() {
        let fixture = Fixture::new();
        let a = fixture.repository("a");
        let b = fixture.repository("b");
        write_module(&a, "WM-DE/drift", MANIFEST);
        write_module(&b, "other", MANIFEST);
        let first_revision = commit(&a);
        let second_revision = commit(&b);
        let cache = fixture.0.join("cache");
        let first = discover(&local_source(&a, "main"), &cache).unwrap();
        let second = discover(&local_source(&b, "HEAD"), &cache).unwrap();
        assert_eq!(first.modules[0].path, "WM-DE/drift");
        assert_eq!(second.modules[0].path, "other");
        assert_eq!(first.revision.as_deref(), Some(first_revision.as_str()));
        assert_eq!(second.revision.as_deref(), Some(second_revision.as_str()));
        assert!(first.modules[0].manifest.supports(Platform::Darwin));
        assert_eq!(
            validate_selection(&first, &["WM-DE/drift".into()])
                .unwrap()
                .len(),
            1
        );
        assert!(validate_selection(&first, &["WM-DE/drift".into(), "WM-DE/drift".into()]).is_err());
        assert!(validate_selection(&first, &["other".into()]).is_err());
        assert!(validate_selection(&first, &["../other".into()]).is_err());
    }

    #[test]
    fn uses_dirty_current_checkout_but_pins_other_refs() {
        let fixture = Fixture::new();
        let repository = fixture.repository("repo");
        let cache = fixture.0.join("cache");
        write_module(&repository, "original", MANIFEST);
        let original = commit(&repository);
        run_git(&repository, ["branch", "other"], false).unwrap();
        write_module(&repository, "dirty", MANIFEST);

        let current = discover(&local_source(&repository, "main"), &cache).unwrap();
        assert_eq!(current.modules.len(), 2);
        assert!(!cache.exists());

        for reference in ["other", original.as_str()] {
            let snapshot = discover(&local_source(&repository, reference), &cache).unwrap();
            assert_ne!(snapshot.checkout, current.checkout);
            assert_eq!(snapshot.revision.as_deref(), Some(original.as_str()));
            assert_eq!(snapshot.modules.len(), 1);
            let reused = discover(&local_source(&repository, reference), &cache).unwrap();
            assert_eq!(reused.checkout, snapshot.checkout);
        }

        assert!(discover(&local_source(&repository, "missing"), &cache).is_err());
        assert!(discover(&local_source(&repository, "main~1"), &cache).is_err());
        assert!(discover(&local_source(&repository, "--config"), &cache).is_err());
    }

    #[test]
    fn rejects_dirty_pin_and_detects_missing_fetched_ref() {
        let fixture = Fixture::new();
        let repository = fixture.repository("repo");
        let cache = fixture.0.join("cache");
        write_module(&repository, "original", MANIFEST);
        let revision = commit(&repository);
        let spec = SourceSpec {
            repository: "https://example.invalid/modules.git".into(),
            reference: revision.clone(),
            mode: SourceMode::Pinned,
        };
        let transport = repository.to_str().unwrap();
        assert!(
            cached_checkout(
                &spec,
                &cache,
                "refs/heads/missing",
                transport,
                Some(&revision)
            )
            .is_err()
        );
        assert_eq!(fs::read_dir(&cache).unwrap().count(), 0);

        // Exercise the same checkout flow without a network dependency.
        let (checkout, pinned) =
            cached_checkout(&spec, &cache, &revision, transport, Some(&revision)).unwrap();
        assert_eq!(pinned, revision);
        write_module(&checkout, "dirty", MANIFEST);
        assert!(cached_checkout(&spec, &cache, &revision, transport, Some(&revision)).is_err());

        let mut unsupported = local_source(&repository, "HEAD");
        unsupported.mode = SourceMode::Pinned;
        assert!(discover(&unsupported, &cache).is_err());
    }

    #[test]
    fn validates_manifest_and_transport() {
        let fixture = Fixture::new();
        for (valid, manifest) in [
            (true, MANIFEST.into()),
            (
                false,
                MANIFEST.replace(r#""schemaVersion": 1"#, r#""schemaVersion": 3"#),
            ),
            (
                false,
                MANIFEST.replace(
                    r#""layers": {"nixos": ["linux"]"#,
                    r#""layers": {"nixos": ["darwin"]"#,
                ),
            ),
            (
                false,
                MANIFEST.replace(
                    r#""default": ["nerd-fonts.jetbrains-mono"]"#,
                    r#""default": ["../package"]"#,
                ),
            ),
            (false, MANIFEST.replace(r#""packages"],"#, r#"".."],"#)),
            (
                false,
                MANIFEST.replace(r#""type": "packages""#, r#""type": "script""#),
            ),
        ] {
            write_module(&fixture.0, "wm", &manifest);
            assert_eq!(read_checkout(&fixture.0).is_ok(), valid);
        }
        assert_eq!(
            remote_url("git@example.org:owner/repo").unwrap(),
            "ssh://git@example.org/owner/repo"
        );
        assert!(remote_url("ssh://git@[::1]:2222/repo").is_ok());
        assert!(remote_url("https://example.org/owner/repo").is_ok());
        for invalid in [
            "ext::helper",
            "file:///etc",
            "-x",
            "ssh://user@-oProxyCommand=whoami/repo",
            "ssh://example.org:0/repo",
        ] {
            assert!(remote_url(invalid).is_err());
        }
        for invalid in ["", "/abc", "ab//cd", "ab/..", "abc/.", "abc/hello world"] {
            assert!(validate_module_path(invalid).is_err());
        }
    }

    #[cfg(unix)]
    #[test]
    fn refuses_symlinked_module_files_and_ignores_symlinked_directories() {
        use std::os::unix::fs::symlink;
        let fixture = Fixture::new();
        let repository = fixture.repository("repo");
        write_module(&fixture.0, "outside", MANIFEST);
        symlink(fixture.0.join("outside"), repository.join("outside")).unwrap();
        assert!(read_checkout(&repository).unwrap().is_empty());

        write_module(&repository, "inside", MANIFEST);
        let definition = repository.join("inside/default.nix");
        fs::remove_file(&definition).unwrap();
        symlink(fixture.0.join("outside/default.nix"), &definition).unwrap();
        assert!(read_checkout(&repository).is_err());

        fs::remove_file(&definition).unwrap();
        fs::write(&definition, "{}").unwrap();
        let manifest = repository.join("inside/module.json");
        fs::remove_file(&manifest).unwrap();
        symlink(fixture.0.join("outside/module.json"), &manifest).unwrap();
        assert!(read_checkout(&repository).is_err());
    }
}
