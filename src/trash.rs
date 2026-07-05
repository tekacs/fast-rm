use std::path::PathBuf;

use anyhow::Result;

#[derive(Debug)]
pub struct StagedItem {
    pub original_path: PathBuf,
    pub staged_path: PathBuf,
    pub info_path: Option<PathBuf>,
}

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;

#[cfg(target_os = "linux")]
pub fn stage_path(path: &std::path::Path) -> Result<StagedItem> {
    linux::stage_path(path)
}

#[cfg(target_os = "macos")]
pub fn stage_path(path: &std::path::Path) -> Result<StagedItem> {
    macos::stage_path(path)
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub fn stage_path(path: &std::path::Path) -> Result<StagedItem> {
    anyhow::bail!(
        "OS Trash staging is not implemented for {} while handling {}",
        std::env::consts::OS,
        path.display()
    );
}
