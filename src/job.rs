use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::{PurgeOptions, StagedItem, purge_paths};

#[derive(Debug, Serialize, Deserialize)]
pub struct JobManifest {
    pub id: String,
    pub status: JobStatus,
    pub created_at_unix: i64,
    pub updated_at_unix: i64,
    pub options: JobOptions,
    pub items: Vec<JobItem>,
    pub report: Option<JobReport>,
    pub errors: Vec<String>,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct JobOptions {
    pub jobs: usize,
    pub cross_device: bool,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct JobItem {
    pub original_path: PathBuf,
    pub staged_path: PathBuf,
    pub info_path: Option<PathBuf>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct JobReport {
    pub files_removed: u64,
    pub dirs_removed: u64,
    pub skipped: u64,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum JobStatus {
    Queued,
    Running,
    Completed,
    Failed,
}

pub fn create_job(staged: &[StagedItem], options: PurgeOptions) -> Result<PathBuf> {
    if staged.is_empty() {
        return Err(anyhow!("cannot detach without any staged paths"));
    }

    let root = state_root()?;
    let jobs_dir = root.join("jobs");
    fs::create_dir_all(&jobs_dir)
        .with_context(|| format!("failed to create {}", jobs_dir.display()))?;

    let id = job_id();
    let path = jobs_dir.join(format!("{id}.json"));
    let now = now_unix();
    let manifest = JobManifest {
        id,
        status: JobStatus::Queued,
        created_at_unix: now,
        updated_at_unix: now,
        options: JobOptions {
            jobs: options.jobs,
            cross_device: options.cross_device,
        },
        items: staged.iter().map(JobItem::from).collect(),
        report: None,
        errors: Vec::new(),
    };

    write_manifest(&path, &manifest)?;
    Ok(path)
}

pub fn run_job(path: &Path) -> Result<JobManifest> {
    let mut manifest = read_manifest(path)?;
    manifest.status = JobStatus::Running;
    manifest.updated_at_unix = now_unix();
    write_manifest(path, &manifest)?;

    let options = PurgeOptions {
        jobs: manifest.options.jobs,
        cross_device: manifest.options.cross_device,
    };
    let staged_paths = manifest
        .items
        .iter()
        .map(|item| item.staged_path.clone())
        .collect::<Vec<_>>();

    match purge_paths(staged_paths, options) {
        Ok(report) => {
            for item in &manifest.items {
                if missing(&item.staged_path)
                    && let Some(info_path) = &item.info_path
                    && let Err(error) = fs::remove_file(info_path)
                    && error.kind() != std::io::ErrorKind::NotFound
                {
                    manifest
                        .errors
                        .push(format!("remove-trashinfo {}: {error}", info_path.display()));
                }
            }

            manifest
                .errors
                .extend(report.errors.iter().map(std::string::ToString::to_string));
            manifest.report = Some(JobReport {
                files_removed: report.files_removed,
                dirs_removed: report.dirs_removed,
                skipped: report.skipped,
            });
            manifest.status = if manifest.errors.is_empty() {
                JobStatus::Completed
            } else {
                JobStatus::Failed
            };
        }
        Err(error) => {
            manifest.errors.push(error.to_string());
            manifest.status = JobStatus::Failed;
        }
    }

    manifest.updated_at_unix = now_unix();
    write_manifest(path, &manifest)?;
    Ok(manifest)
}

fn read_manifest(path: &Path) -> Result<JobManifest> {
    let bytes = fs::read(path).with_context(|| format!("failed to read {}", path.display()))?;
    serde_json::from_slice(&bytes).with_context(|| format!("failed to parse {}", path.display()))
}

fn write_manifest(path: &Path, manifest: &JobManifest) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("manifest path has no parent: {}", path.display()))?;
    fs::create_dir_all(parent).with_context(|| format!("failed to create {}", parent.display()))?;

    let tmp = path.with_extension(format!("json.tmp.{}", std::process::id()));
    {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)
            .with_context(|| format!("failed to create {}", tmp.display()))?;
        serde_json::to_writer_pretty(&mut file, manifest)
            .with_context(|| format!("failed to write {}", tmp.display()))?;
        writeln!(file)?;
        file.sync_all()?;
    }
    fs::rename(&tmp, path)
        .with_context(|| format!("failed to replace manifest {}", path.display()))?;
    Ok(())
}

fn state_root() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("FAST_DELETE_STATE_DIR")
        && !path.is_empty()
    {
        return Ok(PathBuf::from(path));
    }

    #[cfg(target_os = "macos")]
    {
        let home = std::env::var_os("HOME").ok_or_else(|| anyhow!("HOME is not set"))?;
        Ok(PathBuf::from(home)
            .join("Library/Application Support")
            .join("fast-delete"))
    }

    #[cfg(not(target_os = "macos"))]
    {
        if let Some(path) = std::env::var_os("XDG_STATE_HOME")
            && !path.is_empty()
        {
            return Ok(PathBuf::from(path).join("fast-delete"));
        }

        let home = std::env::var_os("HOME").ok_or_else(|| anyhow!("HOME is not set"))?;
        Ok(PathBuf::from(home).join(".local/state/fast-delete"))
    }
}

fn job_id() -> String {
    format!("{}-{}", now_unix(), std::process::id())
}

fn now_unix() -> i64 {
    OffsetDateTime::now_utc().unix_timestamp()
}

fn missing(path: &Path) -> bool {
    fs::symlink_metadata(path).is_err()
}

impl From<&StagedItem> for JobItem {
    fn from(item: &StagedItem) -> Self {
        Self {
            original_path: item.original_path.clone(),
            staged_path: item.staged_path.clone(),
            info_path: item.info_path.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::*;

    #[test]
    fn job_purges_staged_paths_and_records_completion() {
        let tmp = tempdir().unwrap();
        let state = tmp.path().join("state");
        let staged = tmp.path().join("staged");
        fs::create_dir_all(staged.join("a/b")).unwrap();
        fs::write(staged.join("a/file"), "x").unwrap();
        fs::write(staged.join("a/b/file"), "y").unwrap();

        unsafe {
            std::env::set_var("FAST_DELETE_STATE_DIR", &state);
        }

        let item = StagedItem {
            original_path: tmp.path().join("original"),
            staged_path: staged.clone(),
            info_path: None,
        };
        let job_path = create_job(
            &[item],
            PurgeOptions {
                jobs: 4,
                cross_device: false,
            },
        )
        .unwrap();

        let manifest = run_job(&job_path).unwrap();

        assert!(!staged.exists());
        assert!(matches!(manifest.status, JobStatus::Completed));
        assert!(manifest.errors.is_empty());
        assert_eq!(manifest.report.unwrap().files_removed, 2);
    }
}
