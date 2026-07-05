use std::path::{Path, PathBuf};
use std::process::{ExitCode, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::Parser;
use fast_delete::{
    PurgeOptions, PurgeProgress, PurgeReport, create_job, purge_paths_with_progress, run_job,
    stage_path,
};
use indicatif::{ProgressBar, ProgressStyle};

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
        let report = purge_foreground(args.paths, options, "deleting directly")
            .context("purge failed to start")?;
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
    let report = purge_foreground(staged_paths, options, "purging staged Trash paths")
        .context("purge failed to start")?;

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

fn purge_foreground(
    paths: Vec<PathBuf>,
    options: PurgeOptions,
    label: &'static str,
) -> Result<PurgeReport> {
    let progress = Arc::new(CliProgress::new(label, options));
    let report = purge_paths_with_progress(paths, options, progress.clone())?;
    progress.finish();
    Ok(report)
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

struct CliProgress {
    label: &'static str,
    jobs: usize,
    pb: ProgressBar,
    work_known: AtomicU64,
    work_done: AtomicU64,
    roots: AtomicU64,
    dirs_scanned: AtomicU64,
    files_removed: AtomicU64,
    dirs_removed: AtomicU64,
    skipped: AtomicU64,
    errors: AtomicU64,
}

impl CliProgress {
    fn new(label: &'static str, options: PurgeOptions) -> Self {
        let pb = ProgressBar::new(1);
        pb.set_style(
            ProgressStyle::with_template(
                "{spinner:.green} {prefix}\n  [{wide_bar:.cyan/blue}] {msg}",
            )
            .expect("progress template should be valid")
            .tick_strings(&["-", "\\", "|", "/"]),
        );
        pb.enable_steady_tick(Duration::from_millis(80));

        let progress = Self {
            label,
            jobs: options.jobs,
            pb,
            work_known: AtomicU64::new(0),
            work_done: AtomicU64::new(0),
            roots: AtomicU64::new(0),
            dirs_scanned: AtomicU64::new(0),
            files_removed: AtomicU64::new(0),
            dirs_removed: AtomicU64::new(0),
            skipped: AtomicU64::new(0),
            errors: AtomicU64::new(0),
        };
        progress.refresh("starting");
        progress
    }

    fn finish(&self) {
        self.refresh("done");
        self.pb.finish_and_clear();
    }

    fn refresh(&self, active: impl AsRef<str>) {
        let known = self.work_known.load(Ordering::Relaxed).max(1);
        let done = self.work_done.load(Ordering::Relaxed).min(known);

        self.pb.set_length(known);
        self.pb.set_position(done);
        self.pb.set_prefix(format!(
            "{} | roots {} | scanned dirs {} | removed {} files + {} dirs | skipped {} | errors {} | jobs {}",
            self.label,
            self.roots.load(Ordering::Relaxed),
            self.dirs_scanned.load(Ordering::Relaxed),
            self.files_removed.load(Ordering::Relaxed),
            self.dirs_removed.load(Ordering::Relaxed),
            self.skipped.load(Ordering::Relaxed),
            self.errors.load(Ordering::Relaxed),
            self.jobs
        ));
        self.pb.set_message(format!(
            "{} / {} known ({:.0}%) | {}",
            format_count(done),
            format_count(known),
            (done as f64 / known as f64) * 100.0,
            active.as_ref()
        ));
    }

    fn maybe_refresh(&self, active: impl AsRef<str>, count: u64) {
        if count < 16 || count.is_multiple_of(64) {
            self.refresh(active);
        }
    }
}

impl PurgeProgress for CliProgress {
    fn work_discovered(&self, count: u64) {
        let total = self.work_known.fetch_add(count, Ordering::Relaxed) + count;
        self.maybe_refresh("discovering work", total);
    }

    fn root_started(&self, path: &Path) {
        let count = self.roots.fetch_add(1, Ordering::Relaxed) + 1;
        self.maybe_refresh(format!("root {}", short_path(path)), count);
    }

    fn dir_scanned(&self, path: &Path) {
        let count = self.dirs_scanned.fetch_add(1, Ordering::Relaxed) + 1;
        self.maybe_refresh(format!("scanning {}", short_path(path)), count);
    }

    fn file_removed(&self) {
        let count = self.files_removed.fetch_add(1, Ordering::Relaxed) + 1;
        self.work_done.fetch_add(1, Ordering::Relaxed);
        self.maybe_refresh("unlinking files", count);
    }

    fn dir_removed(&self) {
        let count = self.dirs_removed.fetch_add(1, Ordering::Relaxed) + 1;
        self.work_done.fetch_add(1, Ordering::Relaxed);
        self.maybe_refresh("removing directories", count);
    }

    fn skipped(&self) {
        let count = self.skipped.fetch_add(1, Ordering::Relaxed) + 1;
        self.work_done.fetch_add(1, Ordering::Relaxed);
        self.maybe_refresh("skipping cross-device directory", count);
    }

    fn error(&self) {
        let count = self.errors.fetch_add(1, Ordering::Relaxed) + 1;
        self.maybe_refresh("recording errors", count);
    }
}

fn format_count(count: u64) -> String {
    let text = count.to_string();
    let mut formatted = String::with_capacity(text.len() + text.len() / 3);
    let first_group = text.len() % 3;

    for (index, ch) in text.chars().enumerate() {
        if index > 0
            && (index == first_group
                || (index > first_group && (index - first_group).is_multiple_of(3)))
        {
            formatted.push(',');
        }
        formatted.push(ch);
    }

    formatted
}

fn short_path(path: &Path) -> String {
    let text = path.display().to_string();
    let char_count = text.chars().count();
    if char_count <= 96 {
        return text;
    }

    let tail = text
        .chars()
        .skip(char_count.saturating_sub(93))
        .collect::<String>();
    format!("...{tail}")
}
