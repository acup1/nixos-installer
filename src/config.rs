//! Generate data-only Nix selections. User and catalog text is always quoted.
use crate::catalog::SourceSpec;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, OpenOptions, Permissions},
    io::{self, Write},
    os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt, chown},
    path::{Path, PathBuf},
    process::Command,
    sync::atomic::{AtomicU64, Ordering},
};

pub type Result<T> = std::result::Result<T, String>;
#[derive(Clone)]
pub struct Choice {
    pub repo: usize,
    pub path: String,
    pub settings: Vec<(String, Vec<String>, String, Value)>,
}
pub enum Location {
    Local(PathBuf),
    TargetCopy(usize),
    Remote { url: String, rev: String },
}

pub fn quote(value: &str) -> String {
    let mut out = String::from("\"");
    let mut chars = value.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '$' if chars.peek() == Some(&'{') => out.push_str("\\$"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn nix_value(kind: &str, v: &Value) -> Result<String> {
    let fail = || format!("Invalid {kind} option value");
    Ok(match kind {
        "bool" => v.as_bool().ok_or_else(fail)?.to_string(),
        "integer" => v.as_i64().ok_or_else(fail)?.to_string(),
        "string" => quote(v.as_str().ok_or_else(fail)?),
        "string_list" | "packages" => {
            let mut items = vec![];
            for item in v.as_array().ok_or_else(fail)? {
                let name = item.as_str().ok_or_else(fail)?;
                if kind == "packages" {
                    let parts: Vec<_> = name.split('.').collect();
                    if parts.iter().any(|p| {
                        p.is_empty()
                            || !p
                                .bytes()
                                .all(|b| b.is_ascii_alphanumeric() || b"_-+".contains(&b))
                    }) {
                        return Err(format!("Invalid nixpkgs attribute {name:?}"));
                    }
                    items.push(format!(
                        "(lib.attrByPath [ {} ] null pkgs)",
                        parts.into_iter().map(quote).collect::<Vec<_>>().join(" ")
                    ));
                } else {
                    items.push(quote(name));
                }
            }
            let list = format!("[ {} ]", items.join(" "));
            if kind == "packages" {
                format!(
                    "(builtins.filter (p: p != null && lib.meta.availableOn pkgs.stdenv.hostPlatform p) {list})"
                )
            } else {
                list
            }
        }
        _ => return Err(format!("Unsupported option type {kind}")),
    })
}

fn repository_expression(location: &Location) -> Result<String> {
    Ok(match location {
        Location::TargetCopy(index) => format!("(./. + \"/module-sources/{index}\")"),
        Location::Local(p) => {
            if !p.is_absolute() {
                return Err("Expected absolute repository path".into());
            }
            format!(
                "builtins.toPath {}",
                quote(p.to_str().ok_or("Repository path is not UTF-8")?)
            )
        }
        Location::Remote { url, rev } => {
            if rev.len() != 40 || !rev.bytes().all(|b| b.is_ascii_hexdigit()) {
                return Err("Remote repository needs a pinned Git commit".into());
            }
            format!(
                "(builtins.fetchGit {{ url = {}; rev = {}; allRefs = true; }}).outPath",
                quote(url),
                quote(rev)
            )
        }
    })
}

pub fn selection(locations: &[Location], choices: &[Choice]) -> Result<String> {
    let mut out = String::from("let\n");
    for (i, loc) in locations.iter().enumerate() {
        let expr = repository_expression(loc)?;
        out.push_str(&format!("  repo{i} = {expr};\n"));
    }
    out.push_str("in [\n");
    let mut seen = BTreeSet::new();
    for c in choices {
        if c.repo >= locations.len()
            || c.path
                .split('/')
                .any(|p| p.is_empty() || p == "." || p == "..")
            || !seen.insert((c.repo, &c.path))
        {
            return Err("Invalid module selection".into());
        }
        let import = format!(
            "(import (repo{} + {}))",
            c.repo,
            quote(&format!("/{}", c.path))
        );
        if c.settings.is_empty() {
            out.push_str(&format!("  {import}\n"));
            continue;
        }
        let mut layers = BTreeMap::<&str, Vec<String>>::new();
        for (layer, path, kind, value) in &c.settings {
            if !matches!(layer.as_str(), "nixos" | "home" | "darwin")
                || path.is_empty()
                || path.iter().any(String::is_empty)
            {
                return Err("Invalid module option path".into());
            }
            let key = path.iter().map(|s| quote(s)).collect::<Vec<_>>().join(".");
            layers.entry(layer).or_default().push(format!(
                "      {key} = lib.mkForce {};\n",
                nix_value(kind, value)?
            ));
        }
        out.push_str(&format!("  ({import}.withConfig {{\n"));
        for (layer, opts) in layers {
            out.push_str(&format!(
                "    {layer} = {{ lib, pkgs, ... }}: {{\n{}    }};\n",
                opts.concat()
            ));
        }
        out.push_str("  })\n");
    }
    out.push(']');
    Ok(out)
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Selected {
    pub source: SourceSpec,
    pub path: String,
    pub values: BTreeMap<String, Value>,
}

#[derive(Clone, Serialize, Deserialize)]
enum Base {
    Original,
    Fresh {
        username: String,
        hostname: String,
        system: String,
    },
}

#[derive(Clone, Serialize, Deserialize)]
struct Record {
    base: Base,
    modules: String,
    changes: String,
    masked: BTreeSet<String>,
    selected: Vec<Selected>,
    repositories: Vec<SourceSpec>,
}

const MARKER: &str = "# Managed by nixos-installer v2\n";
const STATE_PREFIX: &str = "# Selection: ";
const PATH_OF: &str = r#"bundle:
    let path = if builtins.isAttrs bundle then bundle._componentPath or null else null;
    in if builtins.isString path || builtins.isPath path then toString path else null"#;

#[derive(Clone, Debug, PartialEq, Eq)]
struct FileState {
    bytes: Vec<u8>,
    mode: u32,
    owner: u32,
    group: u32,
}

impl FileState {
    fn read(path: &Path) -> Result<Option<Self>> {
        let metadata = match fs::symlink_metadata(path) {
            Ok(metadata) if metadata.is_file() => metadata,
            Ok(_) => return Err(format!("Expected a regular file at {}", path.display())),
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(format!("{}: {error}", path.display())),
        };
        Ok(Some(Self {
            bytes: fs::read(path).map_err(|error| format!("{}: {error}", path.display()))?,
            mode: metadata.mode() & 0o777,
            owner: metadata.uid(),
            group: metadata.gid(),
        }))
    }

    fn matches(&self, path: &Path) -> Result<()> {
        if Self::read(path)?.as_ref() != Some(self) {
            return Err(format!(
                "{} changed after the catalog was loaded. Reload it before saving.",
                path.display()
            ));
        }
        Ok(())
    }
}

static TEMP_ID: AtomicU64 = AtomicU64::new(0);

fn write_atomic(
    path: &Path,
    bytes: &[u8],
    mode: u32,
    original: Option<&FileState>,
    create: bool,
) -> Result<()> {
    let parent = path.parent().ok_or("Path needs a directory")?;
    fs::create_dir_all(parent).map_err(|error| format!("{}: {error}", parent.display()))?;
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or("Invalid file name")?;
    let number = TEMP_ID.fetch_add(1, Ordering::Relaxed);
    let tmp = parent.join(format!(".{name}.{}.{number}.tmp", std::process::id()));
    let mut file = OpenOptions::new()
        .mode(0o600)
        .write(true)
        .create_new(true)
        .open(&tmp)
        .map_err(|error| format!("{}: {error}", tmp.display()))?;
    let result = (|| {
        file.write_all(bytes).map_err(|error| error.to_string())?;
        if let Some(original) = original {
            let metadata = file.metadata().map_err(|error| error.to_string())?;
            if (metadata.uid(), metadata.gid()) != (original.owner, original.group) {
                chown(&tmp, Some(original.owner), Some(original.group))
                    .map_err(|error| error.to_string())?;
            }
        }
        file.set_permissions(Permissions::from_mode(mode))
            .map_err(|error| error.to_string())?;
        file.sync_all().map_err(|error| error.to_string())?;
        if create {
            // Publish the complete file without replacing an existing path.
            fs::hard_link(&tmp, path).map_err(|error| error.to_string())?;
        } else {
            if let Some(original) = original {
                original.matches(path)?;
            }
            fs::rename(&tmp, path).map_err(|error| error.to_string())?;
        }
        fs::File::open(parent)
            .and_then(|directory| directory.sync_all())
            .map_err(|error| error.to_string())
    })();
    let _ = fs::remove_file(&tmp);
    result.map_err(|error| format!("{}: {error}", path.display()))
}

pub fn atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let original = FileState::read(path)?;
    write_atomic(
        path,
        bytes,
        original.as_ref().map_or(0o600, |file| file.mode),
        original.as_ref(),
        original.is_none(),
    )
}

fn backup_path(path: &Path) -> PathBuf {
    path.with_extension("pre-installer.nix")
}

fn render(path: &Path, record: &Record) -> Result<String> {
    let base = match &record.base {
        Base::Original => {
            let backup = backup_path(path);
            let name = backup
                .file_name()
                .and_then(|name| name.to_str())
                .ok_or("Invalid backup file name")?;
            format!("(import (./. + {})) args", quote(&format!("/{name}")))
        }
        Base::Fresh {
            username,
            hostname,
            system,
        } => format!(
            "{{ username = {}; hostName = {}; system = {}; flakeDir = \"/etc/nixos\"; modules = []; }}",
            quote(username),
            quote(hostname),
            quote(system),
        ),
    };
    let state = serde_json::to_string(record).map_err(|error| error.to_string())?;
    let masked = record
        .masked
        .iter()
        .map(|path| quote(path))
        .collect::<Vec<_>>()
        .join(" ");
    Ok(format!(
        "{MARKER}{STATE_PREFIX}{state}\nargs: let\n  base = {base};\n  selected = {};\n  masked = [ {masked} ];\n  changes = {};\n  pathOf = {PATH_OF};\n  keep = bundle: !(builtins.elem (pathOf bundle) masked);\n  modify = bundle: builtins.foldl' (current: change:\n    if pathOf bundle == change.path then builtins.mapAttrs (layer: original:\n      if builtins.hasAttr layer change.layers then {{ imports = [ original change.layers.${{layer}} ]; }}\n      else original) current\n    else current) bundle changes;\nin base // {{ modules = map modify (builtins.filter keep (base.modules or [])) ++ selected; }}\n",
        record.modules, record.changes,
    ))
}

pub struct Selector {
    path: PathBuf,
    file: FileState,
    backup: Option<FileState>,
    record: Option<Record>,
}

impl Selector {
    pub fn read(path: &Path) -> Result<Option<Self>> {
        if !path.is_absolute() {
            return Err("Selector path must be absolute".into());
        }
        let Some(file) = FileState::read(path)? else {
            return Ok(None);
        };
        let text = std::str::from_utf8(&file.bytes).map_err(|_| "Selector must be UTF-8")?;
        let record = if let Some(body) = text.strip_prefix(MARKER) {
            let contents = body
                .lines()
                .next()
                .and_then(|line| line.strip_prefix(STATE_PREFIX))
                .ok_or("Managed selector is missing its selection metadata")?;
            let record: Record = serde_json::from_str(contents)
                .map_err(|error| format!("Invalid selector metadata: {error}"))?;
            if render(path, &record)? != text {
                return Err(format!(
                    "{} was edited outside the installer. Its changes were preserved.",
                    path.display()
                ));
            }
            Some(record)
        } else if text.starts_with("# Managed by nixos-installer ") {
            return Err(format!(
                "Unrecognized managed selector at {}. The file and its backup were preserved.",
                path.display()
            ));
        } else {
            None
        };
        let backup = if record
            .as_ref()
            .is_some_and(|record| matches!(record.base, Base::Original))
        {
            let backup = backup_path(path);
            Some(
                FileState::read(&backup)?
                    .ok_or_else(|| format!("Original selector is missing: {}", backup.display()))?,
            )
        } else {
            None
        };
        Ok(Some(Self {
            path: path.into(),
            file,
            backup,
            record,
        }))
    }

    pub fn current(&self, path: &Path) -> bool {
        self.path == path
            && self.file.matches(path).is_ok()
            && self
                .backup
                .as_ref()
                .is_none_or(|file| file.matches(&backup_path(path)).is_ok())
    }

    pub fn selections(&self) -> &[Selected] {
        self.record
            .as_ref()
            .map_or(&[], |record| record.selected.as_slice())
    }

    pub fn repositories(&self) -> Option<&[SourceSpec]> {
        self.record
            .as_ref()
            .map(|record| record.repositories.as_slice())
    }

    pub fn managed(&self) -> bool {
        self.record.is_some()
    }

    pub fn masked(&self) -> BTreeSet<String> {
        self.record
            .as_ref()
            .map_or_else(BTreeSet::new, |record| record.masked.clone())
    }

    pub fn original_paths(&self, host: &str) -> Result<Vec<String>> {
        let original = if self.backup.is_some() {
            backup_path(&self.path)
        } else if self.record.is_some() {
            return Ok(Vec::new());
        } else {
            self.path.clone()
        };
        let host = host.split_once('#').map_or(host, |(path, _)| path);
        let inputs = if host.is_empty() {
            "{}".into()
        } else {
            let host = Path::new(host)
                .canonicalize()
                .ok()
                .and_then(|path| path.to_str().map(str::to_string))
                .unwrap_or_else(|| host.into());
            format!(
                r#"(let flake = builtins.getFlake {}; in flake.inputs // {{ self = flake; }})"#,
                quote(&host)
            )
        };
        let path = original.to_str().ok_or("Selector path must be UTF-8")?;
        nix_json(
            &self.path,
            &format!(
                "let cfg = import (builtins.toPath {}) {{ inputs = {inputs}; }};\n pathOf = {PATH_OF};\n in builtins.filter (path: path != null) (map pathOf (cfg.modules or []))",
                quote(path),
            ),
        )
    }

    pub fn save(
        &self,
        modules: String,
        changes: String,
        masked: BTreeSet<String>,
        selected: Vec<Selected>,
        repositories: Vec<SourceSpec>,
    ) -> Result<()> {
        self.file.matches(&self.path)?;
        let record = Record {
            base: self
                .record
                .as_ref()
                .map_or(Base::Original, |record| record.base.clone()),
            modules,
            changes,
            masked,
            selected,
            repositories,
        };
        let text = render(&self.path, &record)?;
        if self.file.bytes == text.as_bytes() {
            return Ok(());
        }
        if matches!(record.base, Base::Original) {
            let backup = backup_path(&self.path);
            if let Some(original) = &self.backup {
                original.matches(&backup)?;
            } else {
                match FileState::read(&backup)? {
                    Some(original) if original == self.file => {}
                    Some(_) => {
                        return Err(format!(
                            "The original at {} differs from the selector. Neither file was changed.",
                            backup.display()
                        ));
                    }
                    None => write_atomic(
                        &backup,
                        &self.file.bytes,
                        self.file.mode,
                        Some(&self.file),
                        true,
                    )?,
                }
            }
        }
        write_atomic(
            &self.path,
            text.as_bytes(),
            self.file.mode,
            Some(&self.file),
            false,
        )
    }
}

pub fn create_selector(
    path: &Path,
    modules: String,
    selected: Vec<Selected>,
    repositories: Vec<SourceSpec>,
    username: &str,
    hostname: &str,
    system: &str,
) -> Result<()> {
    let record = Record {
        base: Base::Fresh {
            username: username.into(),
            hostname: hostname.into(),
            system: system.into(),
        },
        modules,
        changes: "[]".into(),
        masked: BTreeSet::new(),
        selected,
        repositories,
    };
    let text = render(path, &record)?;
    write_atomic(path, text.as_bytes(), 0o644, None, true)
}

pub fn manual_changes(changes: &[(&str, &Choice)]) -> Result<String> {
    let mut out = String::from("[\n");
    for (path, choice) in changes {
        out.push_str(&format!("  {{ path = {}; layers = {{\n", quote(path)));
        let mut layers = BTreeMap::<&str, Vec<String>>::new();
        for (layer, key, kind, value) in &choice.settings {
            let key = key
                .iter()
                .map(|segment| quote(segment))
                .collect::<Vec<_>>()
                .join(".");
            // Override an existing mkForce while keeping the rest of the manual module.
            layers.entry(layer).or_default().push(format!(
                "      {key} = lib.mkOverride 49 {};\n",
                nix_value(kind, value)?
            ));
        }
        for (layer, lines) in layers {
            out.push_str(&format!(
                "    {layer} = {{lib, pkgs, ...}}: {{\n{}    }};\n",
                lines.concat()
            ));
        }
        out.push_str("  }; }\n");
    }
    out.push(']');
    Ok(out)
}

pub fn remote_root(selector: &Path, location: &Location) -> Result<String> {
    nix_json(
        selector,
        &format!("toString ({})", repository_expression(location)?),
    )
}

fn nix_json<T: for<'de> Deserialize<'de>>(selector: &Path, expression: &str) -> Result<T> {
    let output = Command::new("nix")
        .args([
            "--extra-experimental-features",
            "nix-command flakes",
            "eval",
            "--impure",
            "--read-only",
            "--json",
            "--option",
            "allow-import-from-derivation",
            "false",
            "--expr",
            expression,
        ])
        .env("NIXOS_BASE_CONFIG", selector)
        .output()
        .map_err(|error| format!("Inspecting selector with nix: {error}"))?;
    if !output.status.success() {
        let message = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "Inspecting {}: {}",
            selector.display(),
            message.trim()
        ));
    }
    serde_json::from_slice(&output.stdout)
        .map_err(|error| format!("Invalid selector inspection output: {error}"))
}

pub fn fresh_flake(dir: &Path, hostname: &str) -> Result<()> {
    let path = dir.join("flake.nix");
    let template = include_str!("../templates/flake.nix");
    write_atomic(
        &path,
        template.replace("@HOST@", &quote(hostname)).as_bytes(),
        0o644,
        None,
        true,
    )
}

pub fn architecture() -> Result<String> {
    let arch = match std::env::consts::ARCH {
        "x86_64" => "x86_64",
        "aarch64" => "aarch64",
        v => return Err(format!("Unsupported architecture {v}")),
    };
    let os = match std::env::consts::OS {
        "linux" => "linux",
        "macos" => "darwin",
        v => return Err(format!("Unsupported OS {v}")),
    };
    Ok(format!("{arch}-{os}"))
}

pub fn rebuild(flake: &str, selector: &str) -> Result<()> {
    if flake.trim().is_empty() || flake.starts_with('-') {
        return Err("Set the host flake path first".into());
    }
    let system = if cfg!(target_os = "macos") {
        "darwin"
    } else {
        "os"
    };
    let status = Command::new("nh")
        .args([system, "switch", flake, "--", "--impure"])
        .env("NIXOS_BASE_CONFIG", selector)
        .status()
        .map_err(|e| format!("nh: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("nh exited with {status}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn nix_interpolation_is_literal() {
        assert_eq!(quote("a${x}\"\\"), "\"a\\${x}\\\"\\\\\"");
    }
    #[test]
    fn no_code_in_package_names() {
        assert!(nix_value("packages", &serde_json::json!(["pkgs; abort"])).is_err());
    }
    #[test]
    fn reject_relative_pin() {
        assert!(
            selection(
                &[Location::Remote {
                    url: "https://example.com/r".into(),
                    rev: "main".into()
                }],
                &[]
            )
            .is_err()
        );
    }
}

#[cfg(test)]
mod selector_tests {
    use super::*;

    struct Fixture(PathBuf);

    impl Fixture {
        fn new() -> Self {
            let id = TEMP_ID.fetch_add(1, Ordering::Relaxed);
            let directory =
                std::env::temp_dir().join(format!("nixos-selector-{}-{id}", std::process::id()));
            fs::create_dir(&directory).unwrap();
            Self(directory)
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    const ORIGINAL: &str = r#"{ inputs, ... }: {
      username = "local";
      system = "original";
      modules = [
        { _componentPath = "/catalog/first"; nixos.value = "first"; }
        { _componentPath = "/catalog/second"; nixos = { port = 8000; handWritten = true; }; }
        { home.legacy = "legacy"; }
      ];
    }"#;

    fn configure(path: &Path, old: &Selector, masked: BTreeSet<String>, changes: &str) -> Selector {
        old.save("[]".into(), changes.into(), masked, vec![], vec![])
            .unwrap();
        Selector::read(path).unwrap().unwrap()
    }

    fn nix_available() -> bool {
        Command::new("nix")
            .arg("--version")
            .output()
            .is_ok_and(|output| output.status.success())
    }

    #[test]
    fn conflict_preserves_manual_file_and_unrelated_backup() {
        let fixture = Fixture::new();
        let config = fixture.0.join("config.nix");
        fs::write(&config, ORIGINAL).unwrap();
        fs::set_permissions(&config, Permissions::from_mode(0o640)).unwrap();
        let backup = backup_path(&config);
        fs::write(&backup, "different original").unwrap();

        let old = Selector::read(&config).unwrap().unwrap();
        assert!(
            old.save("[]".into(), "[]".into(), BTreeSet::new(), vec![], vec![])
                .is_err()
        );
        assert_eq!(fs::read_to_string(&config).unwrap(), ORIGINAL);
        assert_eq!(fs::read_to_string(&backup).unwrap(), "different original");

        fs::remove_file(&backup).unwrap();
        let saved = configure(
            &config,
            &old,
            BTreeSet::from(["/catalog/first".into()]),
            "[]",
        );
        assert_eq!(fs::read_to_string(&backup).unwrap(), ORIGINAL);
        assert_eq!(fs::metadata(&config).unwrap().mode() & 0o777, 0o640);
        assert_eq!(fs::metadata(&backup).unwrap().mode() & 0o777, 0o640);
        assert!(
            !fs::metadata(&config)
                .unwrap()
                .ino()
                .eq(&fs::metadata(&backup).unwrap().ino())
        );

        let edited_managed = format!("{}# custom edit\n", fs::read_to_string(&config).unwrap());
        fs::write(&config, &edited_managed).unwrap();
        assert!(Selector::read(&config).is_err());
        assert!(
            saved
                .save("[]".into(), "[]".into(), BTreeSet::new(), vec![], vec![])
                .is_err()
        );
        assert_eq!(fs::read_to_string(&config).unwrap(), edited_managed);
        assert_eq!(fs::read_to_string(&backup).unwrap(), ORIGINAL);
    }

    #[test]
    fn saved_mask_survives_reload_and_keeps_manual_and_legacy_values() {
        if !nix_available() {
            return;
        }
        let fixture = Fixture::new();
        let config = fixture.0.join("config.nix");
        fs::write(&config, ORIGINAL).unwrap();
        let old = Selector::read(&config).unwrap().unwrap();
        assert_eq!(
            old.original_paths("").unwrap(),
            ["/catalog/first", "/catalog/second"]
        );

        let masks = BTreeSet::from(["/catalog/first".into()]);
        let change = Choice {
            repo: 0,
            path: "second".into(),
            settings: vec![(
                "nixos".into(),
                vec!["port".into()],
                "integer".into(),
                9000.into(),
            )],
        };
        let changes = manual_changes(&[("/catalog/second", &change)]).unwrap();
        let managed = configure(&config, &old, masks.clone(), &changes);
        assert_eq!(managed.masked(), masks);
        assert_eq!(
            managed.original_paths("").unwrap(),
            ["/catalog/first", "/catalog/second"]
        );
        let again = configure(&config, &managed, managed.masked(), &changes);
        assert_eq!(again.masked(), masks);

        let actual: Value = nix_json(
            &config,
            &format!(
                r#"
            let
              cfg = import (builtins.toPath {}) {{ inputs = {{}}; }};
              current = builtins.elemAt cfg.modules 0;
              old = builtins.elemAt current.nixos.imports 0;
              update = (builtins.elemAt current.nixos.imports 1) {{
                lib.mkOverride = priority: value: {{ inherit priority value; }};
                pkgs = {{}};
              }};
            in {{
              inherit (cfg) username system;
              inherit (old) handWritten;
              inherit update;
              count = builtins.length cfg.modules;
              inherit (current) _componentPath;
              legacy = (builtins.elemAt cfg.modules 1).home.legacy;
            }}"#,
                quote(config.to_str().unwrap())
            ),
        )
        .unwrap();
        assert_eq!(
            actual,
            serde_json::json!({
                "username": "local", "system": "original", "handWritten": true,
                "update": {"port": {"priority": 49, "value": 9000}},
                "_componentPath": "/catalog/second", "count": 2, "legacy": "legacy"
            })
        );
    }

    #[test]
    fn installed_state_and_flake_use_default_and_explicit_selector() {
        let fixture = Fixture::new();
        let config = fixture.0.join("config.nix");
        let source = SourceSpec {
            repository: "/etc/nixos/module-sources/0".into(),
            reference: "HEAD".into(),
            mode: crate::catalog::SourceMode::Local,
        };
        let selected = vec![Selected {
            source: source.clone(),
            path: "base".into(),
            values: BTreeMap::new(),
        }];
        create_selector(
            &config,
            "[]".into(),
            selected,
            vec![source.clone()],
            "alice",
            "machine",
            "x86_64-linux",
        )
        .unwrap();
        fresh_flake(&fixture.0, "machine").unwrap();
        assert!(fresh_flake(&fixture.0, "wrong-name").is_err());
        assert!(
            create_selector(
                &config,
                "[]".into(),
                vec![],
                vec![],
                "wrong-user",
                "machine",
                "x86_64-linux"
            )
            .is_err()
        );
        assert_eq!(fs::metadata(&config).unwrap().mode() & 0o777, 0o644);
        assert!(!backup_path(&config).exists());
        let state = Selector::read(&config).unwrap().unwrap();
        assert_eq!(state.repositories(), Some(&[source][..]));
        assert_eq!(state.selections()[0].path, "base");

        if !nix_available() {
            return;
        }
        let alternate = fixture.0.join("alternate.nix");
        fs::write(&alternate, "{inputs}: { username = \"bob\"; system = \"aarch64-linux\"; flakeDir = \"/etc/nixos\"; modules = []; }").unwrap();
        let expression = format!(
            r#"
          let
            fake = {{
              self = {{}};
              nixpkgs.lib.nixosSystem = args: {{
                inherit (args) system;
                inherit (args.specialArgs) username;
              }};
              home-manager.nixosModules.home-manager = {{}};
            }};
            flake = import (builtins.toPath {});
          in (flake.outputs fake).nixosConfigurations.machine"#,
            quote(fixture.0.join("flake.nix").to_str().unwrap()),
        );
        let default: Value = nix_json(Path::new(""), &expression).unwrap();
        assert_eq!(
            default,
            serde_json::json!({"system":"x86_64-linux", "username":"alice"})
        );
        let explicit: Value = nix_json(&alternate, &expression).unwrap();
        assert_eq!(
            explicit,
            serde_json::json!({"system":"aarch64-linux", "username":"bob"})
        );
    }
}
