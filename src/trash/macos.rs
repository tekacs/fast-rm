use std::path::Path;

use anyhow::{Context, Result, anyhow};
use objc2::rc::autoreleasepool;
use objc2_foundation::{NSFileManager, NSURL};

use crate::trash::StagedItem;

pub fn stage_path(path: &Path) -> Result<StagedItem> {
    autoreleasepool(|_| {
        let url = NSURL::from_file_path(path)
            .with_context(|| format!("invalid file URL for {}", path.display()))?;
        let manager = NSFileManager::defaultManager();
        let mut trashed_url = None;

        manager
            .trashItemAtURL_resultingItemURL_error(&url, Some(&mut trashed_url))
            .map_err(|error| anyhow!("{}", error.localizedDescription()))
            .with_context(|| format!("Foundation failed to trash {}", path.display()))?;

        let staged_path = trashed_url
            .as_ref()
            .and_then(|url| url.to_file_path())
            .ok_or_else(|| {
                anyhow!(
                    "Foundation did not return the trashed destination for {}",
                    path.display()
                )
            })?;

        Ok(StagedItem {
            original_path: absolute_path(path)?,
            staged_path,
            info_path: None,
        })
    })
}

fn absolute_path(path: &Path) -> Result<std::path::PathBuf> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        Ok(std::env::current_dir()?.join(path))
    }
}
