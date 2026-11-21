//! Generate data-only Nix selections. User and catalog text is always quoted.
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, OpenOptions},
    io::Write,
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    process::Command,
};

pub type Result<T> = std::result::Result<T, String>;
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

pub fn selection(locations: &[Location], choices: &[Choice]) -> Result<String> {
    let mut out = String::from("let\n");
    for (i, loc) in locations.iter().enumerate() {
        let expr = match loc {
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
        };
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
    out.push_str("]");
    Ok(out)
}

pub fn atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().ok_or("Path needs a directory")?;
    fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    let name = path
        .file_name()
        .and_then(|s| s.to_str())
        .ok_or("Invalid file name")?;
    let tmp = parent.join(format!(".{name}.{}.tmp", std::process::id()));
    let mut f = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&tmp)
        .map_err(|e| format!("{}: {e}", tmp.display()))?;
    let result = (|| {
        f.write_all(bytes)?;
        f.sync_all()?;
        fs::rename(&tmp, path)?;
        fs::File::open(parent)?.sync_all()
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result.map_err(|e: std::io::Error| format!("{}: {e}", path.display()))
}

const MARKER: &str = "# Managed by nixos-installer v1\n";
pub fn save_selector(path: &Path, modules: &str, fresh: Option<(&str, &str, &str)>) -> Result<()> {
    if !path.is_absolute() {
        return Err("Selector path must be absolute".into());
    }
    let backup = path.with_extension("pre-installer.nix");
    let existing = match fs::symlink_metadata(path) {
        Ok(m) if !m.file_type().is_file() => {
            return Err(format!(
                "Expected a regular selector file at {}",
                path.display()
            ));
        }
        Ok(_) => Some(fs::read_to_string(path).map_err(|e| e.to_string())?),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e.to_string()),
    };
    if fresh.is_some() && existing.is_some() {
        return Err(format!("{} already exists", path.display()));
    }
    if let Some(text) = &existing {
        if !text.starts_with(MARKER) {
            fs::hard_link(path, &backup)
                .map_err(|e| format!("Saving original selector to {}: {e}", backup.display()))?;
        }
    } else if fresh.is_none() {
        return Err(format!(
            "No existing base selector at {}. Select the config.nix used by your host flake.",
            path.display()
        ));
    }
    let base = if backup.is_file() {
        let name = backup
            .file_name()
            .and_then(|x| x.to_str())
            .ok_or("Invalid backup filename")?;
        format!("(import (./. + {})) args", quote(&format!("/{name}")))
    } else if let Some((username, hostname, system)) = fresh {
        format!(
            "{{ username = {}; hostName = {}; system = {}; flakeDir = \"/etc/nixos\"; modules = []; }}",
            quote(username),
            quote(hostname),
            quote(system)
        )
    } else {
        let original = existing.as_deref().ok_or("Missing selector")?;
        original
            .lines()
            .find_map(|line| {
                line.strip_prefix("  base = ")
                    .and_then(|s| s.strip_suffix(';'))
            })
            .ok_or("Cannot locate original selector settings")?
            .to_owned()
    };
    let text = format!(
        "{MARKER}args: let\n  base = {base};\n  selected = {modules};\n  paths = map (bundle: bundle._componentPath or null) selected;\n  keep = bundle: !(bundle ? _componentPath && builtins.elem bundle._componentPath paths);\nin base // {{ modules = builtins.filter keep (base.modules or []) ++ selected; }}\n"
    );
    atomic(path, text.as_bytes())
}

pub fn fresh_flake(dir: &Path, hostname: &str) -> Result<()> {
    let path = dir.join("flake.nix");
    if path.try_exists().map_err(|e| e.to_string())? {
        return Err("An existing target flake would be overwritten".into());
    }
    let template = include_str!("../templates/flake.nix");
    atomic(
        &path,
        template.replace("@HOST@", &quote(hostname)).as_bytes(),
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
