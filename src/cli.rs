//! Command-line interface definition.

use clap::Parser;
use std::path::PathBuf;

/// Download and sync all of your GitHub gists to a local directory.
///
/// On each run, new gists are downloaded and gists that changed remotely are
/// refreshed; unchanged gists are left untouched.
#[derive(Debug, Parser)]
#[command(name = "grass", version, about, long_about = None)]
pub struct Cli {
    /// Directory to store gists in (created if missing).
    #[arg(short, long, default_value = "./gists")]
    pub output: PathBuf,

    /// GitHub token. Overrides the GITHUB_TOKEN/GH_TOKEN env vars and the
    /// `gh` CLI fallback.
    #[arg(long)]
    pub token: Option<String>,

    /// Delete local gist folders that no longer exist on GitHub.
    #[arg(long)]
    pub prune: bool,

    /// Show what would happen without writing anything.
    #[arg(long)]
    pub dry_run: bool,

    /// Re-download every gist, ignoring saved sync state.
    #[arg(long)]
    pub force: bool,

    /// Maximum number of gists to download concurrently.
    #[arg(long, default_value_t = 8, value_parser = clap::value_parser!(u16).range(1..=64))]
    pub concurrency: u16,

    /// Print per-gist actions.
    #[arg(short, long)]
    pub verbose: bool,

    /// Suppress all output except errors.
    #[arg(short, long, conflicts_with = "verbose")]
    pub quiet: bool,
}
