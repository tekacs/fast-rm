use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result, bail};
use clap::Parser;
use fast_delete::{PurgeOptions, purge_paths, stage_path};

#[derive(Debug, Parser)]
#[command(author, version, about)]
struct Args {
    /// Delete paths directly instead of first moving them to the OS Trash.
    #[arg(long, alias = "purge-only", conflicts_with = "trash_only")]
    direct: bool,

    /// Move paths to the OS Trash and stop without purging the staged item.
    #[arg(long, conflicts_with = "direct")]
    trash_only: bool,

    /// Allow purge traversal to cross filesystem/device boundaries.
    #[arg(long)]
    cross_device: bool,

    /// Number of purge worker threads.
    #[arg(long, value_name = "N")]
    jobs: Option<usize>,

    #[arg(required = true)]
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
