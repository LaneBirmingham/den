//! Where themes and layouts come from: built-ins, then mods, then the user's
//! config dir. Later sources override earlier ones with the same name.

use std::path::{Path, PathBuf};

use tailchat_core::mods::ModRuntime;
use tailchat_core::paths::Paths;

#[derive(Clone, Debug, PartialEq)]
pub enum Source {
    Builtin(&'static str),
    File(PathBuf),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Entry {
    pub name: String,
    pub source: Source,
    /// `built-in`, `user`, or `mod <id>`.
    pub origin: String,
}

impl Entry {
    pub fn path(&self) -> Option<&Path> {
        match &self.source {
            Source::File(p) => Some(p),
            Source::Builtin(_) => None,
        }
    }

    pub fn text(&self) -> anyhow::Result<String> {
        match &self.source {
            Source::Builtin(t) => Ok((*t).to_owned()),
            Source::File(p) => Ok(std::fs::read_to_string(p)?),
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct Catalog {
    pub themes: Vec<Entry>,
    pub layouts: Vec<Entry>,
}

fn name_of(path: &Path) -> String {
    #[derive(serde::Deserialize)]
    struct Named {
        name: Option<String>,
    }
    std::fs::read_to_string(path)
        .ok()
        .and_then(|t| toml::from_str::<Named>(&t).ok())
        .and_then(|n| n.name)
        .unwrap_or_else(|| path.file_stem().unwrap_or_default().to_string_lossy().into_owned())
}

fn toml_files(dir: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "toml"))
        .collect();
    v.sort();
    v
}

fn upsert(list: &mut Vec<Entry>, e: Entry) {
    match list.iter_mut().find(|x| x.name == e.name) {
        Some(slot) => *slot = e,
        None => list.push(e),
    }
}

impl Catalog {
    pub fn scan(paths: &Paths, mods: &ModRuntime) -> Catalog {
        let mut c = Catalog::default();
        for (name, text) in crate::assets::THEMES {
            upsert(&mut c.themes, Entry { name: (*name).into(), source: Source::Builtin(text), origin: "built-in".into() });
        }
        for (name, text) in crate::assets::LAYOUTS {
            upsert(&mut c.layouts, Entry { name: (*name).into(), source: Source::Builtin(text), origin: "built-in".into() });
        }
        for m in mods.active() {
            for (kind, list) in [("themes", &m.manifest.themes), ("layouts", &m.manifest.layouts)] {
                for rel in list {
                    let path = m.root.join(rel);
                    if !path.is_file() {
                        continue;
                    }
                    let e = Entry { name: name_of(&path), source: Source::File(path), origin: format!("mod {}", m.manifest.id) };
                    upsert(if kind == "themes" { &mut c.themes } else { &mut c.layouts }, e);
                }
            }
        }
        for path in toml_files(&paths.themes_dir()) {
            upsert(&mut c.themes, Entry { name: name_of(&path), source: Source::File(path), origin: "user".into() });
        }
        for path in toml_files(&paths.layouts_dir()) {
            upsert(&mut c.layouts, Entry { name: name_of(&path), source: Source::File(path), origin: "user".into() });
        }
        c
    }

    pub fn theme(&self, name: &str) -> Option<&Entry> {
        self.themes.iter().find(|e| e.name == name)
    }

    pub fn layout(&self, name: &str) -> Option<&Entry> {
        self.layouts.iter().find(|e| e.name == name)
    }
}
