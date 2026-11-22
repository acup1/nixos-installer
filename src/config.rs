//! Generate data-only Nix selections. User and catalog text is always quoted.
use crate::catalog::SourceSpec;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, OpenOptions, Permissions},
    io::{self, Read, Write},
    os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt, chown},
    path::{Path, PathBuf},
    process::{Command, Stdio},
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

#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct Selected {
    pub source: SourceSpec,
    pub path: String,
    pub values: BTreeMap<String, Value>,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
enum Base {
    Original,
    Fresh {
        username: String,
        hostname: String,
        system: String,
    },
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Identity {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hostname: Option<String>,
}

impl Identity {
    pub fn is_empty(&self) -> bool {
        self.username.is_none() && self.hostname.is_none()
    }

    fn validate(&self) -> Result<()> {
        if let Some(username) = &self.username {
            validate_username(username, HostPlatform::current() == HostPlatform::Darwin)?;
        }
        if let Some(hostname) = &self.hostname {
            validate_hostname(hostname)?;
        }
        Ok(())
    }

    fn definitions(&self) -> String {
        let mut definitions = String::new();
        if let Some(username) = &self.username {
            definitions.push_str(&format!(" username = {};", quote(username)));
        }
        if let Some(hostname) = &self.hostname {
            definitions.push_str(&format!(" hostName = {};", quote(hostname)));
        }
        definitions
    }
}

pub fn validate_hostname(hostname: &str) -> Result<()> {
    let name = hostname.as_bytes();
    if name.len() <= 63
        && name.first().is_some_and(u8::is_ascii_alphanumeric)
        && name.last().is_some_and(u8::is_ascii_alphanumeric)
        && name.iter().all(|c| c.is_ascii_alphanumeric() || *c == b'-')
    {
        Ok(())
    } else {
        Err("Hostname must be a DNS label of 1–63 ASCII letters, digits or hyphens.".into())
    }
}

pub fn validate_username(username: &str, darwin: bool) -> Result<()> {
    let letter = |c: u8| c.is_ascii_lowercase() || (darwin && c.is_ascii_uppercase());
    let name = username.as_bytes();
    if name.len() <= 32
        && name.first().is_some_and(|&c| letter(c) || c == b'_')
        && name.iter().all(|&c| {
            letter(c) || c.is_ascii_digit() || matches!(c, b'_' | b'-') || (darwin && c == b'.')
        })
        && !matches!(
            username.to_ascii_lowercase().as_str(),
            "root" | "nobody" | "nixbld"
        )
    {
        Ok(())
    } else {
        Err(if darwin {
            "Use a normal short account name with ASCII letters, digits, underscores, periods or hyphens."
        } else {
            "Choose a normal account name with lowercase ASCII letters, digits, underscores or hyphens."
        }.into())
    }
}

#[derive(Clone, PartialEq, Serialize, Deserialize)]
struct Record {
    base: Base,
    modules: String,
    changes: String,
    masked: BTreeSet<String>,
    selected: Vec<Selected>,
    repositories: Vec<SourceSpec>,
    #[serde(default, skip_serializing_if = "Identity::is_empty")]
    identity: Identity,
}

const MARKER: &str = "# Managed by nixos-installer v2";
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

// Ignore whitespace between syntax elements, keeping comments, string contents,
// and whitespace-sensitive boundaries such as the end of a Nix path literal.
fn same_nix_structure(left: rnix::SyntaxNode, right: rnix::SyntaxNode) -> bool {
    use rnix::{NodeOrToken, SyntaxKind::TOKEN_WHITESPACE};
    let children = |node: rnix::SyntaxNode| {
        node.children_with_tokens()
            .filter(|child| child.kind() != TOKEN_WHITESPACE)
    };
    let mut other = children(right);
    children(left).all(|child| match (child, other.next()) {
        (NodeOrToken::Node(a), Some(NodeOrToken::Node(b))) => {
            a.kind() == b.kind() && same_nix_structure(a, b)
        }
        (NodeOrToken::Token(a), Some(NodeOrToken::Token(b))) => {
            a.kind() == b.kind()
                && (a.text() == b.text()
                    || (a.kind() == rnix::SyntaxKind::TOKEN_COMMENT
                        && a.text().starts_with('#')
                        && a.text().trim_end_matches('\r') == b.text().trim_end_matches('\r')))
        }
        _ => false,
    }) && other.next().is_none()
}

fn matches_managed_text(actual: &str, expected: &str) -> bool {
    if actual == expected {
        return true;
    }
    let actual = rnix::Root::parse(actual);
    let expected = rnix::Root::parse(expected);
    actual.errors().is_empty()
        && expected.errors().is_empty()
        && same_nix_structure(actual.syntax(), expected.syntax())
}

fn render(path: &Path, record: &Record) -> Result<String> {
    render_with_original(path, record, None)
}

fn render_with_original(path: &Path, record: &Record, original: Option<&Path>) -> Result<String> {
    let base = match &record.base {
        Base::Original if original.is_some() => {
            let original = original
                .and_then(Path::to_str)
                .ok_or("Selector path must be UTF-8")?;
            format!("(import (builtins.toPath {})) args", quote(original))
        }
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
    let system_change = if matches!(record.base, Base::Fresh { .. }) {
        let mut options = String::new();
        if let Some(hostname) = &record.identity.hostname {
            options.push_str(&format!(
                " networking.hostName = lib.mkOverride 90 {};",
                quote(hostname)
            ));
        }
        if let Some(username) = &record.identity.username {
            options.push_str(&format!(
                " users.users.{}.isNormalUser = lib.mkDefault true;",
                quote(username)
            ));
        }
        if options.is_empty() {
            String::new()
        } else {
            format!(" ++ [ {{ nixos = {{ lib, ... }}: {{{options} }}; }} ]")
        }
    } else {
        String::new()
    };
    let identity = record.identity.definitions();
    let state = serde_json::to_string(record).map_err(|error| error.to_string())?;
    let masked = record
        .masked
        .iter()
        .map(|path| quote(path))
        .collect::<Vec<_>>()
        .join(" ");
    Ok(format!(
        "{MARKER}\n{STATE_PREFIX}{state}\nargs: let\n  base = {base};\n  selected = {};\n  masked = [ {masked} ];\n  changes = {};\n  pathOf = {PATH_OF};\n  keep = bundle: !(builtins.elem (pathOf bundle) masked);\n  modify = bundle: builtins.foldl' (current: change:\n    if pathOf bundle == change.path then builtins.mapAttrs (layer: original:\n      if builtins.hasAttr layer change.layers then {{ imports = [ original change.layers.${{layer}} ]; }}\n      else original) current\n    else current) bundle changes;\nin base // {{{identity} modules = map modify (builtins.filter keep (base.modules or [])) ++ selected{system_change}; }}\n",
        record.modules, record.changes,
    ))
}

#[derive(Default, Deserialize)]
pub struct CatalogContext {
    pub originals: Vec<String>,
    pub sources: Vec<String>,
    pub identity: Identity,
}

pub struct Selector {
    path: PathBuf,
    file: FileState,
    backup: Option<FileState>,
    record: Option<Record>,
}

pub const INSTALLED_SELECTOR: &str = "/etc/nixos/config.nix";
pub const SAVE_INSTALLED_ARG: &str = "--save-installed-selector";
const MAX_INSTALLED_EDIT: u64 = 16 * 1024 * 1024;

#[derive(Serialize, Deserialize)]
struct InstalledEdit {
    expected: String,
    record: Record,
}

impl InstalledEdit {
    fn write(&self, selector: &Selector) -> Result<()> {
        if selector.file.bytes != self.expected.as_bytes() {
            return Err(format!(
                "{} changed after the catalog was loaded. Refresh it before saving.",
                selector.path.display()
            ));
        }
        if !matches!(self.record.base, Base::Fresh { .. })
            || selector.record.as_ref().map(|record| &record.base) != Some(&self.record.base)
            || self.record.changes != "[]"
            || !self.record.masked.is_empty()
        {
            return Err("The installed selector's base or manual imports changed.".into());
        }
        self.record.identity.validate()?;
        if selector.record.as_ref() == Some(&self.record) {
            return Ok(());
        }
        let text = render(&selector.path, &self.record)?;
        write_atomic(
            &selector.path,
            text.as_bytes(),
            selector.file.mode,
            Some(&selector.file),
            false,
        )
    }
}

pub fn save_installed_from_stdin() -> Result<()> {
    let mut bytes = Vec::new();
    io::stdin()
        .take(MAX_INSTALLED_EDIT + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("Reading installed selector update: {error}"))?;
    if bytes.len() as u64 > MAX_INSTALLED_EDIT {
        return Err("Installed selector update is too large.".into());
    }
    let edit: InstalledEdit = serde_json::from_slice(&bytes)
        .map_err(|error| format!("Invalid installed selector update: {error}"))?;
    let selector = Selector::read(Path::new(INSTALLED_SELECTOR))?
        .filter(Selector::installed)
        .ok_or("Expected a root-owned, installer-managed /etc/nixos/config.nix.")?;
    edit.write(&selector)
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
        let mut lines = text.lines();
        let record = if lines.next() == Some(MARKER) {
            let contents = lines
                .next()
                .and_then(|line| line.strip_prefix(STATE_PREFIX))
                .ok_or("Managed selector is missing its selection metadata")?;
            let record: Record = serde_json::from_str(contents)
                .map_err(|error| format!("Invalid selector metadata: {error}"))?;
            if !matches_managed_text(text, &render(path, &record)?) {
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

    pub fn flake_dir(&self) -> Result<Option<String>> {
        let path = self.path.to_str().ok_or("Selector path must be UTF-8")?;
        nix_json_with_network(
            &self.path,
            false,
            &format!(
                "let cfg = import (builtins.toPath {}) {{ inputs = {{}}; }}; in if (cfg.flakeDir or null) == null then null else toString cfg.flakeDir",
                quote(path),
            ),
        )
    }

    pub fn catalog_context(&self, host: &str, offline: bool) -> Result<CatalogContext> {
        let original = if self.backup.is_some() {
            backup_path(&self.path)
        } else {
            self.path.clone()
        };
        let host = flake_reference(host);
        if host.trim().is_empty() || host.starts_with('-') {
            return Err("Set flakeDir in config.nix or enter a host flake override.".into());
        }
        let definition = Path::new(host).join("flake.nix");
        let local = Path::new(host).is_absolute() || host.starts_with('.');
        if local && !definition.is_file() {
            return Err(format!("Host flake is missing: {}", definition.display()));
        }
        let host = nix_flake_reference(host);
        let inputs = format!(
            "(let flake = builtins.getFlake {}; in flake.inputs // {{ self = flake; }})",
            quote(&host)
        );
        let path = original.to_str().ok_or("Selector path must be UTF-8")?;
        let modules = if self.managed() && self.backup.is_none() {
            // The installer generated this list from saved choices. Forcing it
            // here can fetch every pinned source and is unnecessary to load metadata.
            "[]"
        } else {
            "(cfg.modules or null)"
        };
        let valid_host = if local {
            let definition = definition.to_str().ok_or("Host flake path must be UTF-8")?;
            format!(
                "builtins.isFunction ((import (builtins.toPath {})).outputs or null)",
                quote(definition)
            )
        } else {
            // Loading a remote host's outputs is deferred until Rebuild.
            "true".into()
        };
        let identity = match self.record.as_ref().map(|record| &record.base) {
            Some(Base::Fresh {
                username, hostname, ..
            }) => format!(
                "{{ username = {}; hostname = {}; }}",
                quote(username),
                quote(hostname)
            ),
            _ => "{ username = cfg.username or null; hostname = cfg.hostName or null; }".into(),
        };
        nix_json_with_network(
            &self.path,
            !offline,
            &format!(
                r#"let
                    cfg = import (builtins.toPath {}) {{ inputs = {inputs}; }};
                    pathOf = {PATH_OF};
                    modules = {modules};
                    sources = cfg.moduleSources or [];
                    in if !({valid_host}) then throw "The host flake must define an outputs function"
                    else if !(builtins.isList modules && builtins.all builtins.isAttrs modules)
                    then throw "config.nix must declare modules as a list of module bundles"
                    else if !(builtins.isList sources && builtins.all
                      (source: (builtins.isString source || builtins.isPath source)
                        && builtins.substring 0 1 (toString source) == "/") sources)
                    then throw "moduleSources must be a list of absolute catalog paths"
                    else {{
                      originals = builtins.filter (path: path != null) (map pathOf modules);
                      sources = map toString sources;
                      identity = {identity};
                    }}"#,
                quote(path),
            ),
        )
    }

    pub fn identity(&self) -> Identity {
        self.record
            .as_ref()
            .map_or_else(Identity::default, |record| record.identity.clone())
    }

    pub fn preview_identity<T>(
        &self,
        dir: &Path,
        identity: &Identity,
        use_selector: impl FnOnce(&Path) -> Result<T>,
    ) -> Result<T> {
        if &self.identity() == identity {
            return use_selector(&self.path);
        }
        identity.validate()?;
        let mut record = self.record.clone().unwrap_or_else(|| Record {
            base: Base::Original,
            modules: "[]".into(),
            changes: "[]".into(),
            masked: BTreeSet::new(),
            selected: vec![],
            repositories: vec![],
            identity: Identity::default(),
        });
        record.identity = identity.clone();
        let original = if self.backup.is_some() {
            backup_path(&self.path)
        } else {
            self.path.clone()
        };
        let original = original
            .canonicalize()
            .map_err(|error| format!("{}: {error}", original.display()))?;
        let text = render_with_original(&self.path, &record, Some(&original))?;
        let path = dir.join(format!(
            ".selector-preview-{}-{}.nix",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_err(|error| error.to_string())?
                .as_nanos()
        ));
        write_atomic(&path, text.as_bytes(), 0o600, None, true)?;
        let result = path
            .canonicalize()
            .map_err(|error| format!("{}: {error}", path.display()))
            .and_then(|path| use_selector(&path));
        let _ = fs::remove_file(path);
        result
    }

    pub fn installed(&self) -> bool {
        cfg!(target_os = "linux")
            && self.path == Path::new(INSTALLED_SELECTOR)
            && self.file.owner == 0
            && self.file.mode & 0o022 == 0
            && self.path.parent().is_some_and(|directory| {
                fs::symlink_metadata(directory)
                    .is_ok_and(|stat| stat.is_dir() && stat.uid() == 0 && stat.mode() & 0o022 == 0)
            })
            && self
                .record
                .as_ref()
                .is_some_and(|record| matches!(record.base, Base::Fresh { .. }))
    }

    pub fn save_installed_with_sudo(
        &self,
        modules: String,
        selected: Vec<Selected>,
        repositories: Vec<SourceSpec>,
        identity: Identity,
    ) -> Result<()> {
        if !self.installed() {
            return Err(
                "Only the installer-managed /etc/nixos/config.nix supports this operation.".into(),
            );
        }
        self.file.matches(&self.path)?;
        let base = self
            .record
            .as_ref()
            .ok_or("Missing installed selector metadata.")?
            .base
            .clone();
        let record = Record {
            base,
            modules,
            selected,
            repositories,
            changes: "[]".into(),
            masked: BTreeSet::new(),
            identity,
        };
        record.identity.validate()?;
        if self.record.as_ref() == Some(&record) {
            return Ok(());
        }
        let expected =
            String::from_utf8(self.file.bytes.clone()).map_err(|error| error.to_string())?;
        let edit = serde_json::to_vec(&InstalledEdit { expected, record })
            .map_err(|error| error.to_string())?;
        if edit.len() as u64 > MAX_INSTALLED_EDIT {
            return Err("Installed selector update is too large.".into());
        }
        let executable =
            std::env::current_exe().map_err(|error| format!("Locating the installer: {error}"))?;
        let sudo = ["/run/wrappers/bin/sudo", "/usr/bin/sudo"]
            .into_iter()
            .find(|path| Path::new(path).is_file())
            .ok_or("Saving /etc/nixos/config.nix requires administrator access through sudo.")?;
        let mut child = Command::new(sudo)
            .env_clear()
            .arg("--")
            .arg(executable)
            .arg(SAVE_INSTALLED_ARG)
            .stdin(Stdio::piped())
            .spawn()
            .map_err(|error| format!("Starting sudo to save the installed selector: {error}"))?;
        let write = child
            .stdin
            .take()
            .ok_or("Cannot send the selector update to sudo.")?
            .write_all(&edit);
        let status = child
            .wait()
            .map_err(|error| format!("Waiting for the selector update: {error}"))?;
        if !status.success() {
            return Err(format!(
                "The administrator save of /etc/nixos/config.nix exited with {status}."
            ));
        }
        write.map_err(|error| format!("Sending the installed selector update: {error}"))
    }

    pub fn save(
        &self,
        modules: String,
        changes: String,
        masked: BTreeSet<String>,
        selected: Vec<Selected>,
        repositories: Vec<SourceSpec>,
        identity: Identity,
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
            identity,
        };
        record.identity.validate()?;
        if self.record.as_ref() == Some(&record) {
            return Ok(());
        }
        let text = render(&self.path, &record)?;
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
        identity: Identity::default(),
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

pub fn remote_root(selector: &Path, location: &Location, offline: bool) -> Result<String> {
    nix_json_with_network(
        selector,
        !offline,
        &format!("toString ({})", repository_expression(location)?),
    )
}

fn nix_json<T: for<'de> Deserialize<'de>>(selector: &Path, expression: &str) -> Result<T> {
    nix_json_with_network(selector, true, expression)
}

fn nix_json_with_network<T: for<'de> Deserialize<'de>>(
    selector: &Path,
    allow_network: bool,
    expression: &str,
) -> Result<T> {
    let mut command = Command::new("nix");
    if !allow_network {
        command.arg("--offline");
    }
    command
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
        .env("NIXOS_BASE_CONFIG", selector);
    let output = crate::read_command::output(&mut command)
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

#[derive(Clone, Copy, PartialEq, Eq)]
enum HostPlatform {
    Linux,
    Darwin,
}

impl HostPlatform {
    fn current() -> Self {
        if cfg!(target_os = "macos") {
            Self::Darwin
        } else {
            Self::Linux
        }
    }

    fn outputs(self) -> &'static str {
        match self {
            Self::Linux => "nixosConfigurations",
            Self::Darwin => "darwinConfigurations",
        }
    }

    fn nh_command(self) -> &'static str {
        match self {
            Self::Linux => "os",
            Self::Darwin => "darwin",
        }
    }
}

#[derive(Deserialize)]
struct FlakeOutputs {
    names: Vec<String>,
    username: Option<String>,
    hostname: Option<String>,
}

pub fn flake_reference(installable: &str) -> &str {
    installable
        .split_once('#')
        .map_or(installable, |(reference, _)| reference)
}

fn nix_flake_reference(reference: &str) -> String {
    use std::fmt::Write as _;

    let Ok(path) = Path::new(reference).canonicalize() else {
        return reference.into();
    };
    let Some(absolute) = path.to_str() else {
        return reference.into();
    };
    let url_safe = |byte: u8| byte.is_ascii_alphanumeric() || b"/-._~".contains(&byte);
    if absolute.bytes().all(url_safe) {
        return absolute.into();
    }

    // Older Nix accepts a percent-encoded local URI for paths with spaces.
    let mut uri = String::from(if path.join(".git").exists() {
        "git+file://"
    } else {
        "path:"
    });
    for byte in absolute.bytes() {
        if url_safe(byte) {
            uri.push(char::from(byte));
        } else {
            write!(&mut uri, "%{byte:02X}").expect("writing to String cannot fail");
        }
    }
    uri
}

pub struct RebuildTarget {
    platform: HostPlatform,
    flake: String,
    name: String,
}

impl RebuildTarget {
    pub fn resolve(flake: &str, selector: &Path) -> Result<Self> {
        Self::resolve_for(flake, selector, HostPlatform::current())
    }

    fn resolve_for(flake: &str, selector: &Path, platform: HostPlatform) -> Result<Self> {
        Self::inspect(flake, selector, platform)
    }

    fn inspect(flake: &str, selector: &Path, platform: HostPlatform) -> Result<Self> {
        let (reference, explicit) = flake
            .split_once('#')
            .map_or((flake, None), |(reference, name)| (reference, Some(name)));
        if reference.trim().is_empty() || reference.starts_with('-') {
            return Err("Set flakeDir in the selector or enter a host flake override.".into());
        }
        let reference = nix_flake_reference(reference);
        let path = selector.to_str().ok_or("Selector path must be UTF-8")?;
        let outputs = platform.outputs();
        let data: FlakeOutputs = nix_json(
            selector,
            &format!(
                "let flake = builtins.getFlake {}; cfg = import (builtins.toPath {}) {{ inputs = flake.inputs // {{ self = flake; }}; }}; in {{ names = builtins.attrNames (flake.{outputs} or {{}}); username = cfg.username or null; hostname = cfg.hostName or null; }}",
                quote(&reference),
                quote(path),
            ),
        )?;
        let name = if let Some(explicit) = explicit {
            data.names
                .iter()
                .find(|name| name.as_str() == explicit)
                .ok_or_else(|| format!("{reference} does not export {outputs}.{explicit}."))?
        } else {
            let preferred = match platform {
                HostPlatform::Darwin => data.username.map(|name| format!("{name}-OSX")),
                HostPlatform::Linux => data.hostname.clone(),
            };
            let fallback = match platform {
                HostPlatform::Darwin => data.hostname.as_deref(),
                HostPlatform::Linux => Some("nixos"),
            };
            [preferred.as_deref(), fallback].into_iter().flatten()
                .find_map(|candidate| data.names.iter().find(|name| name.as_str() == candidate))
                .or(match data.names.as_slice() { [name] => Some(name), _ => None })
                .ok_or_else(|| {
                    if data.names.is_empty() {
                        format!("{reference} exports no {outputs}.")
                    } else {
                        format!("Choose a host override as {reference}#HOST. Available configurations: {}.", data.names.join(", "))
                    }
                })?
        };
        Ok(Self {
            platform,
            flake: reference,
            name: name.clone(),
        })
    }

    fn command(&self, selector: &Path) -> Command {
        let mut command = Command::new("nh");
        command
            .args([
                self.platform.nh_command(),
                "switch",
                "--hostname",
                &self.name,
                &self.flake,
                "--",
                "--impure",
            ])
            .env("NIXOS_BASE_CONFIG", selector);
        command
    }

    pub fn rebuild(&self, selector: &Path) -> Result<()> {
        let status = self
            .command(selector)
            .status()
            .map_err(|e| format!("nh: {e}"))?;
        if status.success() {
            Ok(())
        } else {
            Err(format!("nh exited with {status}"))
        }
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
        old.save(
            "[]".into(),
            changes.into(),
            masked,
            vec![],
            vec![],
            Identity::default(),
        )
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
    fn discovers_flake_and_targets_only_exported_systems() {
        if !nix_available() {
            return;
        }
        let fixture = Fixture::new();
        let host = fixture.0.join("host flake");
        fs::create_dir(&host).unwrap();
        fs::write(
            host.join("flake.nix"),
            r#"{
              outputs = { self }: {
                darwinConfigurations.local-OSX = throw "must not build system";
                darwinConfigurations.other = {};
                nixosConfigurations.nixos = throw "must not build system";
                nixosConfigurations.machine = {};
              };
            }"#,
        )
        .unwrap();
        let selector = fixture.0.join("custom.nix");
        fs::write(
            &selector,
            r#"{ inputs }: let root = ./. + "/host flake"; in {
            flakeDir = root;
            username = "local";
            hostName = "unrelated-hostname";
            modules = throw "must not force modules";
        }"#,
        )
        .unwrap();
        let source = Selector::read(&selector).unwrap().unwrap();
        let flake = source.flake_dir().unwrap().unwrap();
        assert_eq!(
            Path::new(&flake).canonicalize().unwrap(),
            host.canonicalize().unwrap()
        );
        let darwin = RebuildTarget::resolve_for(&flake, &selector, HostPlatform::Darwin).unwrap();
        assert!(darwin.flake.starts_with("path:") && darwin.flake.ends_with("/host%20flake"));
        let command = darwin.command(&selector);
        assert_eq!(
            command.get_args().collect::<Vec<_>>(),
            [
                "darwin",
                "switch",
                "--hostname",
                "local-OSX",
                &darwin.flake,
                "--",
                "--impure"
            ]
        );
        assert!(
            command
                .get_envs()
                .any(|(name, path)| name == "NIXOS_BASE_CONFIG"
                    && path == Some(selector.as_os_str()))
        );
        let linux = RebuildTarget::resolve_for(&flake, &selector, HostPlatform::Linux).unwrap();
        assert_eq!(linux.name, "nixos");
        let explicit =
            RebuildTarget::resolve_for(&format!("{flake}#other"), &selector, HostPlatform::Darwin)
                .unwrap();
        assert_eq!(explicit.name, "other");
        assert!(
            RebuildTarget::resolve_for(
                &format!("{flake}#missing"),
                &selector,
                HostPlatform::Darwin
            )
            .is_err()
        );

        fs::write(&selector, "{...}: { hostName = \"machine\"; }").unwrap();
        let fresh = RebuildTarget::resolve_for(&flake, &selector, HostPlatform::Linux).unwrap();
        assert_eq!(fresh.name, "machine");
    }

    #[test]
    fn scoped_installed_edit_preserves_host_permissions_and_rejects_stale_input() {
        let fixture = Fixture::new();
        let path = fixture.0.join("config.nix");
        create_selector(
            &path,
            "[]".into(),
            vec![],
            vec![],
            "alice",
            "machine",
            "x86_64-linux",
        )
        .unwrap();
        fs::set_permissions(&path, Permissions::from_mode(0o640)).unwrap();
        let fresh = Selector::read(&path).unwrap().unwrap();
        let expected = fs::read_to_string(&path).unwrap();
        let mut record = fresh.record.as_ref().unwrap().clone();
        record.modules = "[ { home.test = true; } ]".into();

        let mut wrong_record = record.clone();
        wrong_record.base = Base::Fresh {
            username: "other-user".into(),
            hostname: "machine".into(),
            system: "x86_64-linux".into(),
        };
        assert!(
            InstalledEdit {
                expected: expected.clone(),
                record: wrong_record
            }
            .write(&fresh)
            .is_err()
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), expected);

        let update = InstalledEdit { expected, record };
        update.write(&fresh).unwrap();
        let saved = Selector::read(&path).unwrap().unwrap();
        assert_eq!(
            saved.record.as_ref().unwrap().modules,
            "[ { home.test = true; } ]"
        );
        assert_eq!(saved.file.mode, 0o640);
        assert_eq!(saved.file.owner, fresh.file.owner);
        assert_eq!(saved.file.group, fresh.file.group);
        assert!(update.write(&saved).is_err());
        assert!(!backup_path(&path).exists());
        assert!(
            !saved.installed(),
            "only the real system path permits elevation"
        );
    }

    #[test]
    fn formatted_selector_reopens_and_noop_save_preserves_its_bytes() {
        let fixture = Fixture::new();
        let path = fixture.0.join("config.nix");
        fs::write(&path, ORIGINAL).unwrap();
        let original = Selector::read(&path).unwrap().unwrap();
        configure(&path, &original, BTreeSet::new(), "[]");
        let canonical = fs::read_to_string(&path).unwrap();
        let formatted = canonical
            .replace("args: let", "args:\nlet")
            .replace("in base // { modules", "in\n  base // {modules")
            .replace("selected = [];", "selected = [\n  ];")
            .replace('\n', "\r\n");
        fs::write(&path, &formatted).unwrap();
        let reopened = Selector::read(&path).unwrap().unwrap();
        configure(&path, &reopened, BTreeSet::new(), "[]");
        assert_eq!(fs::read_to_string(&path).unwrap(), formatted);

        let edited = formatted.replace("selected = [", "selected = [ {} ");
        fs::write(&path, &edited).unwrap();
        assert!(Selector::read(&path).is_err());
        assert!(
            reopened
                .save(
                    "[]".into(),
                    "[]".into(),
                    BTreeSet::new(),
                    vec![],
                    vec![],
                    Identity::default()
                )
                .is_err()
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), edited);
        assert_eq!(fs::read_to_string(backup_path(&path)).unwrap(), ORIGINAL);
    }

    #[test]
    fn selector_formatting_preserves_literal_contents_and_token_boundaries() {
        assert!(matches_managed_text("{ a = \"x y\"; }", "{a=\"x y\";}"));
        for (original, edit) in [
            ("\"x y\"", "\"xy\""),
            ("''x  y''", "''x y''"),
            ("let a = 1; in a", "leta = 1; in a"),
            ("[ ./a ./b ]", "[ ./a./b ]"),
            ("{ a = true; }", "{ a = false; }"),
            ("{}", "{} # keep this"),
        ] {
            assert!(!matches_managed_text(original, edit), "{edit}");
        }
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
            old.save(
                "[]".into(),
                "[]".into(),
                BTreeSet::new(),
                vec![],
                vec![],
                Identity::default()
            )
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
                .save(
                    "[]".into(),
                    "[]".into(),
                    BTreeSet::new(),
                    vec![],
                    vec![],
                    Identity::default()
                )
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
        fs::write(fixture.0.join("flake.nix"), "{ outputs = inputs: {}; }").unwrap();
        let host = fixture.0.to_str().unwrap();
        let old = Selector::read(&config).unwrap().unwrap();
        assert_eq!(
            old.catalog_context(host, true).unwrap().originals,
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
            managed.catalog_context(host, true).unwrap().originals,
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
        fs::write(&alternate, "{inputs}: { username = \"bob\"; hostName = \"alternate\"; system = \"aarch64-linux\"; flakeDir = \"/etc/nixos\"; modules = []; }").unwrap();
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
            configs = (flake.outputs fake).nixosConfigurations;
          in builtins.mapAttrs (name: args: args // {{ inherit name; }}) configs"#,
            quote(fixture.0.join("flake.nix").to_str().unwrap()),
        );
        let default: Value = nix_json(Path::new(""), &expression).unwrap();
        assert_eq!(
            default,
            serde_json::json!({"machine": {"name":"machine", "system":"x86_64-linux", "username":"alice"}})
        );
        let explicit: Value = nix_json(&alternate, &expression).unwrap();
        assert_eq!(
            explicit,
            serde_json::json!({"alternate": {"name":"alternate", "system":"aarch64-linux", "username":"bob"}})
        );
        fs::write(&alternate, r#"{inputs}: { username = "bob"; system = "aarch64-linux"; flakeDir = "/etc/nixos"; modules = []; }"#).unwrap();
        let legacy: Value = nix_json(&alternate, &expression).unwrap();
        assert_eq!(
            legacy,
            serde_json::json!({"machine": {"name":"machine", "system":"aarch64-linux", "username":"bob"}})
        );
    }
    #[test]
    fn prospective_identity_reads_relative_original_and_resolves_platform_outputs_before_save() {
        if !nix_available() {
            return;
        }
        let fixture = Fixture::new();
        let config = fixture.0.join("config.nix");
        let data = r#"{ username = "alice"; hostName = "workstation"; extra = 42; }"#;
        fs::write(fixture.0.join("identity-data.nix"), data).unwrap();
        let original =
            "{inputs}: import ./identity-data.nix // { modules = []; flakeDir = toString ./.; }";
        fs::write(&config, original).unwrap();
        fs::write(
            fixture.0.join("flake.nix"),
            r#"{
          outputs = {self}: let
            cfg = import (builtins.toPath (builtins.getEnv "NIXOS_BASE_CONFIG")) { inputs = {}; };
          in {
            darwinConfigurations."${cfg.username}-OSX" = {};
            nixosConfigurations.${cfg.hostName} = {};
          };
        }"#,
        )
        .unwrap();
        let host = fixture.0.to_str().unwrap();
        let selector = Selector::read(&config).unwrap().unwrap();
        let identity = Identity {
            username: Some("bob".into()),
            hostname: Some("laptop".into()),
        };
        let preview_dir = fixture.0.join("different parent");
        selector
            .preview_identity(&preview_dir, &identity, |preview| {
                let darwin = RebuildTarget::resolve_for(host, preview, HostPlatform::Darwin)?;
                let linux = RebuildTarget::resolve_for(host, preview, HostPlatform::Linux)?;
                assert_eq!(darwin.name, "bob-OSX");
                assert_eq!(linux.name, "laptop");
                assert!(
                    RebuildTarget::resolve_for(
                        &format!("{host}#alice-OSX"),
                        preview,
                        HostPlatform::Darwin
                    )
                    .is_err()
                );
                assert_eq!(
                    RebuildTarget::resolve_for(
                        &format!("{host}#bob-OSX"),
                        preview,
                        HostPlatform::Darwin
                    )?
                    .name,
                    "bob-OSX"
                );
                Ok(())
            })
            .unwrap();
        assert_eq!(fs::read_to_string(&config).unwrap(), original);
        assert_eq!(fs::read_dir(&preview_dir).unwrap().count(), 0);
        selector
            .save(
                "[]".into(),
                "[]".into(),
                BTreeSet::from(["/unrelated".into()]),
                vec![],
                vec![],
                identity.clone(),
            )
            .unwrap();

        let selector = Selector::read(&config).unwrap().unwrap();
        assert_eq!(selector.identity(), identity);
        assert_eq!(
            selector
                .catalog_context(host, true)
                .unwrap()
                .identity
                .username
                .as_deref(),
            Some("alice")
        );
        assert_eq!(fs::read_to_string(backup_path(&config)).unwrap(), original);
        selector
            .preview_identity(&preview_dir, &Identity::default(), |preview| {
                assert_eq!(
                    RebuildTarget::resolve_for(host, preview, HostPlatform::Darwin)?.name,
                    "alice-OSX"
                );
                Ok(())
            })
            .unwrap();
        selector
            .save(
                "[]".into(),
                "[]".into(),
                selector.masked(),
                vec![],
                vec![],
                Identity::default(),
            )
            .unwrap();
        let reset = Selector::read(&config).unwrap().unwrap();
        assert!(reset.identity().is_empty());
        assert_eq!(reset.masked(), BTreeSet::from(["/unrelated".into()]));
        let extra: u32 = nix_json(
            &config,
            &format!(
                "(import (builtins.toPath {}) {{ inputs = {{}}; }}).extra",
                quote(config.to_str().unwrap())
            ),
        )
        .unwrap();
        assert_eq!(extra, 42);
    }

    #[test]
    fn installed_identity_edit_preserves_base_and_generates_scoped_linux_settings() {
        let fixture = Fixture::new();
        let config = fixture.0.join("config.nix");
        create_selector(
            &config,
            "[]".into(),
            vec![],
            vec![],
            "owner",
            "machine",
            "x86_64-linux",
        )
        .unwrap();
        let selector = Selector::read(&config).unwrap().unwrap();
        let mut record = selector.record.clone().unwrap();
        let original = record.base.clone();
        record.identity = Identity {
            username: Some("new-owner".into()),
            hostname: Some("new-node".into()),
        };
        let valid = InstalledEdit {
            expected: fs::read_to_string(&config).unwrap(),
            record,
        };
        valid.write(&selector).unwrap();
        assert_eq!(fs::metadata(&config).unwrap().mode() & 0o777, 0o644);
        let loaded = Selector::read(&config).unwrap().unwrap();
        assert!(loaded.record.as_ref().unwrap().base == original);
        assert_eq!(loaded.identity(), valid.record.identity);
        let mut invalid_record = valid.record.clone();
        invalid_record.identity.username = Some("root".into());
        let copy = fs::read_to_string(&config).unwrap();
        assert!(
            InstalledEdit {
                expected: copy.clone(),
                record: invalid_record
            }
            .write(&loaded)
            .is_err()
        );
        assert_eq!(fs::read_to_string(&config).unwrap(), copy);

        if !nix_available() {
            return;
        }
        let effect: Value = nix_json(
            &config,
            &format!(
                r#"
            let cfg = import (builtins.toPath {}) {{ inputs = {{}}; }};
                options = (builtins.head cfg.modules).nixos {{
                    lib.mkOverride = priority: value: {{ inherit priority value; }};
                    lib.mkDefault = x: x;
                }};
            in {{ inherit (cfg) username hostName; inherit options; }}
        "#,
                quote(config.to_str().unwrap())
            ),
        )
        .unwrap();
        assert_eq!(
            effect,
            serde_json::json!({
                "username": "new-owner", "hostName": "new-node",
                "options": {
                    "networking": {"hostName": {"value": "new-node", "priority": 90}},
                    "users": {"users": {"new-owner": {"isNormalUser": true}}}
                }
            })
        );
    }
}
