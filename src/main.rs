use std::path::{Path, PathBuf};
use std::process::{ExitCode, Stdio};

use anyhow::{Context, Result, bail};
use clap::Parser;
use fast_delete::{PurgeOptions, create_job, purge_paths, run_job, stage_path};

#[cfg(unix)]
use std::os::unix::process::CommandExt;

#[derive(Debug, Parser)]
#[command(author, version, about)]
struct Args {
    #[arg(long, hide = true, value_name = "PATH")]
    worker_job: Option<PathBuf>,

    /// Delete paths directly instead of first moving them to the OS Trash.
    #[arg(long, alias = "purge-only", conflicts_with_all = ["trash_only", "detach"])]
    direct: bool,

    /// Move paths to the OS Trash and stop without purging the staged item.
    #[arg(long, conflicts_with_all = ["direct", "detach"])]
    trash_only: bool,

    /// Move paths to the OS Trash, spawn a detached purge worker, and return.
    #[arg(long, conflicts_with_all = ["direct", "trash_only"])]
    detach: bool,

    /// Allow purge traversal to cross filesystem/device boundaries.
    #[arg(long)]
    cross_device: bool,

    /// Number of purge worker threads.
    #[arg(long, value_name = "N")]
    jobs: Option<usize>,

    paths: Vec<PathBuf>,
}

fn main() -> ExitCode {
    match run() {
        Ok(code) => code,
        Err(error) => {
            eprintln!("fast-delete: {error:#}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<ExitCode> {
    let args = Args::parse();

    if let Some(job) = args.worker_job {
        let manifest = run_job(&job)
            .with_context(|| format!("background worker failed for job {}", job.display()))?;
        return Ok(exit_for_errors(manifest.errors.len()));
    }

    if args.paths.is_empty() {
        bail!("at least one path is required");
    }

    let options = PurgeOptions {
        jobs: args.jobs.unwrap_or_else(default_jobs),
        cross_device: args.cross_device,
    };

    if options.jobs == 0 {
        bail!("--jobs must be greater than zero");
    }

    if args.direct {
        let report = purge_paths(args.paths, options).context("purge failed to start")?;
        print_report(&report);
        return Ok(exit_for_errors(report.errors.len()));
    }

    let mut staged = Vec::new();
    let mut stage_errors = 0usize;

    for path in &args.paths {
        match stage_path(path) {
            Ok(item) => {
                println!(
                    "trashed {} -> {}",
                    item.original_path.display(),
                    item.staged_path.display()
                );
                staged.push(item);
            }
            Err(error) => {
                stage_errors += 1;
                eprintln!("failed to trash {}: {error:#}", path.display());
            }
        }
    }

    if args.trash_only {
        return Ok(exit_for_errors(stage_errors));
    }

    if args.detach {
        let job_path = create_job(&staged, options).context("failed to write detach job")?;
        spawn_worker(&job_path).context("failed to spawn detached worker")?;
        println!("detached purge job {}", job_path.display());
        return Ok(exit_for_errors(stage_errors));
    }

    let staged_paths = staged
        .iter()
        .map(|item| item.staged_path.clone())
        .collect::<Vec<_>>();
    let report = purge_paths(staged_paths, options).context("purge failed to start")?;

    for item in &staged {
        if missing(&item.staged_path)
            && let Some(info_path) = &item.info_path
            && let Err(error) = std::fs::remove_file(info_path)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            eprintln!(
                "failed to remove trash metadata {}: {error}",
                info_path.display()
            );
        }
    }

    print_report(&report);
    Ok(exit_for_errors(stage_errors + report.errors.len()))
}

fn spawn_worker(job_path: &Path) -> Result<()> {
    let exe = std::env::current_exe().context("failed to resolve current executable")?;
    let log_path = job_path.with_extension("log");
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .with_context(|| format!("failed to open worker log {}", log_path.display()))?;
    let log_for_stderr = log
        .try_clone()
        .with_context(|| format!("failed to clone worker log {}", log_path.display()))?;

    let mut command = std::process::Command::new(exe);
    command
        .arg("--worker-job")
        .arg(job_path)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(log_for_stderr));

    #[cfg(unix)]
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                Err(std::io::Error::last_os_error())
            } else {
                Ok(())
            }
        });
    }

    command.spawn()?;
    Ok(())
}

fn default_jobs() -> usize {
    std::thread::available_parallelism()
        .map(|parallelism| parallelism.get().saturating_mul(2).clamp(4, 32))
        .unwrap_or(8)
}

fn exit_for_errors(errors: usize) -> ExitCode {
    if errors == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

fn missing(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_err()
}

fn print_report(report: &fast_delete::PurgeReport) {
    println!(
        "purged {} files and {} directories; skipped {}; errors {}",
        report.files_removed,
        report.dirs_removed,
        report.skipped,
        report.errors.len()
    );

    for error in &report.errors {
        eprintln!("{error}");
    }
}
