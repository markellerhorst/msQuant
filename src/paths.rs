//! Locations of the files that msQuant writes.
//!
//! Everything the program needs for itself (settings, audit trail and temporary files) lives in
//! the folder [`DEFAULT_DIR_NAME`] in the root folder of the installation. Files the user creates
//! (projects, results, figures) go to a standard folder that can be chosen in the preferences; it
//! defaults to the same folder. Nothing else is written to disk during normal use.

use std::path::{Path, PathBuf};

/// Name of the folder next to the program that holds settings, audit trail and temporary files.
pub const DEFAULT_DIR_NAME: &str = "msQuant-default";

/// Derives the installation root from the path of the executable.
///
/// A program started through `cargo run` sits in `target/debug` or `target/release`. In that case
/// the project root (the folder that contains `target`) is returned, so that the data folder does
/// not end up hidden inside the build output.
#[must_use]
pub fn root_from_exe(exe: &Path) -> PathBuf {
    let dir = exe.parent().unwrap_or_else(|| Path::new("."));
    let folder_name = |p: &Path| p.file_name().and_then(|n| n.to_str()).map(str::to_owned);
    let in_build_output = matches!(folder_name(dir).as_deref(), Some("debug") | Some("release"))
        && dir.parent().and_then(folder_name).as_deref() == Some("target");
    if in_build_output {
        if let Some(project_root) = dir.parent().and_then(Path::parent) {
            return project_root.to_path_buf();
        }
    }
    dir.to_path_buf()
}

/// Root folder of the installation (see [`root_from_exe`]); the working directory if the path of
/// the executable cannot be determined.
#[must_use]
pub fn install_root() -> PathBuf {
    match std::env::current_exe() {
        Ok(exe) => root_from_exe(&exe),
        Err(_) => std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
    }
}

/// Path of the default data folder, without creating it.
#[must_use]
pub fn default_dir_path() -> PathBuf {
    install_root().join(DEFAULT_DIR_NAME)
}

/// Path of the default data folder, created if it does not exist yet.
pub fn ensure_default_dir() -> PathBuf {
    let dir = default_dir_path();
    // A failure shows up later, when the first file is written, with a precise error message.
    let _ = std::fs::create_dir_all(&dir);
    dir
}

/// File with the user settings.
#[must_use]
pub fn settings_file() -> PathBuf {
    default_dir_path().join("settings.txt")
}

/// File with the audit trail.
#[must_use]
pub fn audit_file() -> PathBuf {
    default_dir_path().join("audit_trail.log")
}

/// Folder for temporary files, created if it does not exist yet.
pub fn temp_dir() -> PathBuf {
    let dir = default_dir_path().join("tmp");
    let _ = std::fs::create_dir_all(&dir);
    dir
}

/// Deletes everything in the temporary folder (leftovers of an interrupted run).
pub fn clean_temp() {
    let _ = std::fs::remove_dir_all(default_dir_path().join("tmp"));
}
