use std::ffi::OsString;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use time::OffsetDateTime;

use crate::trash::StagedItem;

pub fn stage_path(path: &Path) -> Result<StagedItem> {
    refuse_dangerous_path(path)?;

    let original_path = absolute_path(path)?;
    let metadata = fs::symlink_metadata(&original_path)
        .with_context(|| format!("failed to stat {}", original_path.display()))?;
    let target_dev = metadata.dev();
    let trash_root = trash_root_for(&original_path, &metadata)?;
    let files_dir = trash_root.join("files");
    let info_dir = trash_root.join("info");

    ensure_private_dir(&trash_root)
        .with_context(|| format!("failed to prepare {}", trash_root.display()))?;
    ensure_private_dir(&files_dir)
        .with_context(|| format!("failed to prepare {}", files_dir.display()))?;
    ensure_private_dir(&info_dir)
        .with_context(|| format!("failed to prepare {}", info_dir.display()))?;

    let name = original_path
        .file_name()
        .ok_or_else(|| anyhow!("{} has no file name", original_path.display()))?;
    let trash_name = unique_trash_name(&files_dir, &info_dir, name)?;
    let staged_path = files_dir.join(&trash_name);
    let info_path = info_dir.join(with_suffix(&trash_name, ".trashinfo"));
    let temp_info_path = info_dir.join(with_suffix(
        &trash_name,
        &format!(".trashinfo.tmp.{}", std::process::id()),
    ));

    write_trashinfo(&temp_info_path, &original_path)
        .with_context(|| format!("failed to write {}", temp_info_path.display()))?;

    fs::rename(&original_path, &staged_path).with_context(|| {
        let _ = fs::remove_file(&temp_info_path);
        format!(
            "failed to move {} to {}",
            original_path.display(),
            staged_path.display()
        )
    })?;

    fs::rename(&temp_info_path, &info_path)
        .with_context(|| format!("failed to publish trash metadata {}", info_path.display()))?;

    let staged_dev = fs::symlink_metadata(&staged_path)
        .map(|metadata| metadata.dev())
        .unwrap_or(target_dev);
    if staged_dev != target_dev {
        bail!(
            "trash staging crossed devices: {} -> {}",
            original_path.display(),
            staged_path.display()
        );
    }

    Ok(StagedItem {
        original_path,
        staged_path,
        info_path: Some(info_path),
    })
}

fn trash_root_for(path: &Path, metadata: &fs::Metadata) -> Result<PathBuf> {
    let target_dev = metadata.dev();
    if let Some(home_trash) = home_trash_for(target_dev)? {
        return Ok(home_trash);
    }

    let mount_root = filesystem_root(path, metadata)?;
    let uid = unsafe { libc::geteuid() };
    let shared_trash = mount_root.join(".Trash");

    if is_directory(&shared_trash) {
        return Ok(shared_trash.join(uid.to_string()));
    }

    Ok(mount_root.join(format!(".Trash-{uid}")))
}

fn home_trash_for(target_dev: u64) -> Result<Option<PathBuf>> {
    let data_home = xdg_data_home()?;
    let probe = nearest_existing_ancestor(&data_home)?;
    let probe_dev = fs::symlink_metadata(&probe)
        .with_context(|| format!("failed to stat {}", probe.display()))?
        .dev();

    if probe_dev == target_dev {
        Ok(Some(data_home.join("Trash")))
    } else {
        Ok(None)
    }
}

fn xdg_data_home() -> Result<PathBuf> {
    if let Some(value) = std::env::var_os("XDG_DATA_HOME")
        && !value.is_empty()
    {
        let path = PathBuf::from(value);
        if path.is_absolute() {
            return Ok(path);
        }
    }

    let home = std::env::var_os("HOME").ok_or_else(|| anyhow!("HOME is not set"))?;
    Ok(PathBuf::from(home).join(".local/share"))
}

fn filesystem_root(path: &Path, metadata: &fs::Metadata) -> Result<PathBuf> {
    let dev = metadata.dev();
    let file_type = metadata.file_type();
    let mut current = if file_type.is_dir() && !file_type.is_symlink() {
        path.to_path_buf()
    } else {
        path.parent()
            .ok_or_else(|| anyhow!("{} has no parent directory", path.display()))?
            .to_path_buf()
    };

    loop {
        let Some(parent) = current.parent() else {
            return Ok(current);
        };

        let parent_metadata = fs::symlink_metadata(parent)
            .with_context(|| format!("failed to stat {}", parent.display()))?;
        if parent_metadata.dev() != dev {
            return Ok(current);
        }

        current = parent.to_path_buf();
    }
}

fn ensure_private_dir(path: &Path) -> Result<()> {
    if path.exists() {
        let metadata = fs::symlink_metadata(path)?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            bail!("{} exists but is not a real directory", path.display());
        }
        return Ok(());
    }

    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)?;
    Ok(())
}

fn unique_trash_name(
    files_dir: &Path,
    info_dir: &Path,
    name: &std::ffi::OsStr,
) -> Result<OsString> {
    for index in 0u64..10_000 {
        let candidate = if index == 0 {
            name.to_os_string()
        } else {
            with_suffix(name, &format!(".{index}"))
        };

        if !files_dir.join(&candidate).exists()
            && !info_dir
                .join(with_suffix(&candidate, ".trashinfo"))
                .exists()
        {
            return Ok(candidate);
        }
    }

    bail!("could not allocate a unique Trash name for {:?}", name)
}

fn write_trashinfo(path: &Path, original_path: &Path) -> Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;

    writeln!(file, "[Trash Info]")?;
    writeln!(file, "Path={}", encode_trashinfo_path(original_path))?;
    writeln!(file, "DeletionDate={}", deletion_date())?;
    file.sync_all()?;
    Ok(())
}

fn deletion_date() -> String {
    let now = OffsetDateTime::now_local().unwrap_or_else(|_| OffsetDateTime::now_utc());
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}",
        now.year(),
        u8::from(now.month()),
        now.day(),
        now.hour(),
        now.minute(),
        now.second()
    )
}

fn encode_trashinfo_path(path: &Path) -> String {
    let mut encoded = String::new();
    for byte in path.as_os_str().as_bytes() {
        match *byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'/' | b'-' | b'_' | b'.' | b'~' => {
                encoded.push(*byte as char);
            }
            byte => {
                encoded.push('%');
                encoded.push(hex(byte >> 4));
                encoded.push(hex(byte & 0x0f));
            }
        }
    }
    encoded
}

fn hex(nibble: u8) -> char {
    match nibble {
        0..=9 => (b'0' + nibble) as char,
        10..=15 => (b'A' + (nibble - 10)) as char,
        _ => unreachable!(),
    }
}

fn with_suffix(name: &std::ffi::OsStr, suffix: &str) -> OsString {
    let mut bytes = name.as_bytes().to_vec();
    bytes.extend_from_slice(suffix.as_bytes());
    OsString::from_vec(bytes)
}

fn nearest_existing_ancestor(path: &Path) -> Result<PathBuf> {
    let mut current = path;
    loop {
        if current.exists() {
            return Ok(current.to_path_buf());
        }
        current = current
            .parent()
            .ok_or_else(|| anyhow!("no existing ancestor for {}", path.display()))?;
    }
}

fn is_directory(path: &Path) -> bool {
    fs::symlink_metadata(path)
        .map(|metadata| metadata.is_dir() && !metadata.file_type().is_symlink())
        .unwrap_or(false)
}

fn absolute_path(path: &Path) -> Result<PathBuf> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        Ok(std::env::current_dir()?.join(path))
    }
}

fn refuse_dangerous_path(path: &Path) -> Result<()> {
    if path.as_os_str().is_empty() {
        bail!("refusing to trash empty path");
    }

    if path.parent().is_none() {
        bail!("refusing to trash filesystem root");
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_trashinfo_path_as_url_path() {
        let path = Path::new("/tmp/a file/%/snow");
        assert_eq!(encode_trashinfo_path(path), "/tmp/a%20file/%25/snow");
    }

    #[test]
    fn suffix_preserves_non_utf8_bytes() {
        let name = OsString::from_vec(vec![b'a', 0xff]);
        assert_eq!(with_suffix(&name, ".trash").as_bytes(), b"a\xff.trash");
    }
}
