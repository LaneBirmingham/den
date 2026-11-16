//! Where Den keeps config, data and runtime state.
//!
//! Set `DEN_HOME` to keep everything under one directory (portable
//! installs, tests, running several instances on one machine).
//! (`TAILCHAT_HOME`, from before the rename, still works.)

use std::path::{Path, PathBuf};

#[derive(Clone, Debug)]
pub struct Paths {
    /// `config.toml`, `server.toml`, `mods.toml`, `themes/`, `layouts/`, `mods/`.
    pub config: PathBuf,
    /// Persistent data: saved rooms, mod state.
    pub data: PathBuf,
    /// Runtime state: control API instance files.
    pub state: PathBuf,
}

impl Paths {
    pub fn detect() -> Paths {
        let home = ["DEN_HOME", "TAILCHAT_HOME"].into_iter().find_map(|k| std::env::var_os(k).filter(|v| !v.is_empty()));
        if let Some(home) = home {
            return Paths::under(Path::new(&home));
        }
        if let Some(new) = Paths::project("den") {
            if let Some(old) = Paths::project("tailchat") {
                migrate(&old, &new);
            }
            return new;
        }
        Paths::under(Path::new(".den"))
    }

    /// The OS's usual folders for an app called `name`.
    fn project(name: &str) -> Option<Paths> {
        let dirs = directories::ProjectDirs::from("dev", name, name)?;
        let state = dirs.state_dir().map(Path::to_path_buf).unwrap_or_else(|| dirs.data_local_dir().join("state"));
        Some(Paths { config: dirs.config_dir().to_path_buf(), data: dirs.data_dir().to_path_buf(), state })
    }

    pub fn under(root: &Path) -> Paths {
        Paths { config: root.join("config"), data: root.join("data"), state: root.join("state") }
    }

    pub fn config_file(&self) -> PathBuf {
        self.config.join("config.toml")
    }
    pub fn server_config_file(&self) -> PathBuf {
        self.config.join("server.toml")
    }
    pub fn mods_file(&self) -> PathBuf {
        self.config.join("mods.toml")
    }
    pub fn mods_dir(&self) -> PathBuf {
        self.config.join("mods")
    }
    pub fn themes_dir(&self) -> PathBuf {
        self.config.join("themes")
    }
    pub fn layouts_dir(&self) -> PathBuf {
        self.config.join("layouts")
    }
    /// Saved rooms, one folder per room id (see `rooms`).
    pub fn rooms_dir(&self) -> PathBuf {
        self.data.join("rooms")
    }
    /// Where `den serve` kept history before rooms were saved per room.
    pub fn legacy_history_dir(&self) -> PathBuf {
        self.data.join("history")
    }
    pub fn mod_state_dir(&self, mod_id: &str) -> PathBuf {
        self.data.join("mod-state").join(mod_id)
    }
    pub fn mod_config_dir(&self, mod_id: &str) -> PathBuf {
        self.config.join("mod-config").join(mod_id)
    }
    pub fn instances_dir(&self) -> PathBuf {
        self.state.join("instances")
    }
}

/// Move folders left by the app's old name (tailchat) to the new ones, once:
/// only those that exist where the new one doesn't yet.
fn migrate(old: &Paths, new: &Paths) {
    let pairs = [(&old.config, &new.config), (&old.data, &new.data), (&old.state, &new.state)];
    for (from, to) in pairs {
        if !from.is_dir() || to.exists() {
            continue;
        }
        let moved = to.parent().map_or(Ok(()), std::fs::create_dir_all).and_then(|()| std::fs::rename(from, to));
        match moved {
            Ok(()) => tracing::info!("moved {} to {} (the app was renamed)", from.display(), to.display()),
            Err(e) => tracing::warn!("couldn't move {} to {}: {e}", from.display(), to.display()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrates_old_folders_once() {
        let root = std::env::temp_dir().join(format!("den-paths-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let old = Paths::under(&root.join("tailchat"));
        let new = Paths::under(&root.join("den"));
        std::fs::create_dir_all(&old.config).unwrap();
        std::fs::write(old.config.join("config.toml"), "nick = \"x\"\n").unwrap();
        std::fs::create_dir_all(old.data.join("rooms")).unwrap();
        std::fs::create_dir_all(&new.state).unwrap();
        std::fs::write(new.state.join("keep"), "").unwrap();
        migrate(&old, &new);
        assert!(new.config.join("config.toml").is_file() && !old.config.exists());
        assert!(new.data.join("rooms").is_dir() && !old.data.exists());
        // A folder the new name already has is left alone.
        assert!(new.state.join("keep").is_file());
        let _ = std::fs::remove_dir_all(&root);
    }
}
