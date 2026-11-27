use crate::{Result, catalog::SourceSpec, config, config::Selected, install::InstallRequest};
use serde::{Deserialize, Serialize};
use std::{
    fs, io,
    os::unix::fs::{DirBuilderExt, MetadataExt},
    path::{Path, PathBuf},
};

pub const DIRECTORY: &str = "/tmp/nixos-installer-draft";

#[derive(PartialEq, Serialize, Deserialize)]
pub struct Draft {
    pub request: InstallRequest,
    pub repositories: Vec<SourceSpec>,
    pub selections: Vec<Selected>,
}

pub struct Store {
    path: PathBuf,
    saved: Option<Draft>,
}

impl Store {
    pub fn open(directory: &Path) -> Result<Self> {
        let uid = unsafe { libc::geteuid() };
        let error = |err: io::Error| format!("{}: {err}", directory.display());
        match fs::DirBuilder::new().mode(0o700).create(directory) {
            Ok(()) => {}
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {}
            Err(err) => return Err(error(err)),
        }
        let metadata = fs::symlink_metadata(directory).map_err(error)?;
        if !metadata.is_dir() || metadata.uid() != uid || metadata.mode() & 0o077 != 0 {
            return Err(format!(
                "{} must be a private installer directory.",
                directory.display()
            ));
        }
        let path = directory.join("state.json");
        let saved = match fs::symlink_metadata(&path) {
            Ok(metadata)
                if metadata.is_file() && metadata.uid() == uid && metadata.mode() & 0o077 == 0 =>
            {
                let data = fs::read(&path).map_err(error)?;
                match serde_json::from_slice(&data) {
                    Ok(draft) => Some(draft),
                    Err(error) => {
                        eprintln!(
                            "Ignoring the installation draft at {}: {error}",
                            path.display()
                        );
                        None
                    }
                }
            }
            Ok(_) => {
                return Err(format!(
                    "{} must be a private installer file.",
                    path.display()
                ));
            }
            Err(err) if err.kind() == io::ErrorKind::NotFound => None,
            Err(err) => return Err(error(err)),
        };
        Ok(Self { path, saved })
    }

    pub fn saved(&self) -> Option<&Draft> {
        self.saved.as_ref()
    }

    pub fn save(&mut self, draft: Draft) -> Result<()> {
        if self.saved.as_ref() != Some(&draft) {
            let data = serde_json::to_vec_pretty(&draft).map_err(|error| error.to_string())?;
            config::atomic(&self.path, &data)?;
            self.saved = Some(draft);
        }
        Ok(())
    }

    pub fn clear(&self) -> Result<()> {
        match fs::remove_file(&self.path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(format!("Removing {}: {error}", self.path.display())),
        }
    }
}
