//! Where tailchat keeps config, data and runtime state.
//!
//! Set `TAILCHAT_HOME` to keep everything under one directory (portable
//! installs, tests, running several instances on one machine).

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
        if let Some(home) = std::env::var_os("TAILCHAT_HOME").filter(|v| !v.is_empty()) {
            return Paths::under(Path::new(&home));
        }
        if let Some(dirs) = directories::ProjectDirs::from("dev", "tailchat", "tailchat") {
            let state = dirs.state_dir().map(Path::to_path_buf).unwrap_or_else(|| dirs.data_local_dir().join("state"));
            return Paths { config: dirs.config_dir().to_path_buf(), data: dirs.data_dir().to_path_buf(), state };
        }
        Paths::under(Path::new(".tailchat"))
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
    /// Where `tailchat serve` kept history before rooms were saved per room.
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
