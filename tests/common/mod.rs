//! Helpers shared by the integration tests.
#![allow(dead_code)] // Each test crate uses only some of them.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Makes credhop, and any shell that runs it, use `home` as the home directory.
pub fn isolate<'a>(command: &'a mut Command, home: &Path) -> &'a mut Command {
    if cfg!(windows) {
        command
            .env("USERPROFILE", home)
            .env("LOCALAPPDATA", home.join(r"AppData\Local"))
    } else {
        command.env("HOME", home).env_remove("XDG_DATA_HOME")
    }
}

/// The data directory credhop uses when isolated in `home`.
pub fn data_dir(home: &Path) -> PathBuf {
    if cfg!(windows) {
        home.join(r"AppData\Local\credhop")
    } else {
        home.join(".local/share/credhop")
    }
}

/// Creates a home directory under `parent` whose path has a space and a non-ASCII character.
pub fn unusual_home(parent: &Path) -> PathBuf {
    let home = parent.join("home dir ü");
    fs::create_dir(&home).unwrap();
    home
}

/// Saves empty profiles, given as `(tool, name)` pairs, for credhop isolated in `home`.
pub fn save_profiles(home: &Path, profiles: &[(&str, &str)]) {
    for (tool, name) in profiles {
        let dir = data_dir(home).join("profiles").join(tool);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(format!("{name}.json")), "{}").unwrap();
    }
}
