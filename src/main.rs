//! grass — download and sync all of your GitHub gists to a local directory.

mod auth;
mod cli;
mod client;
mod model;
mod naming;
mod state;
mod sync;

use anyhow::Result;
use clap::Parser;
use cli::Cli;
use client::GitHubClient;
use indicatif::{ProgressBar, ProgressStyle};
use model::Gist;
use std::time::Duration;
use sync::{Action, Reporter, Summary, SyncOptions};

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let token = auth::resolve_token(cli.token.clone())?;
    let client = GitHubClient::new(&token)?;

    let opts = SyncOptions {
        output: cli.output.clone(),
        concurrency: cli.concurrency as usize,
        prune: cli.prune,
        dry_run: cli.dry_run,
        force: cli.force,
    };

    if !cli.quiet {
        let mode = if cli.dry_run { " (dry run)" } else { "" };
        eprintln!("grass: syncing gists to {}{}", opts.output.display(), mode);
    }

    let reporter = BarReporter::new(&cli);
    let summary = sync::run(&client, &opts, &reporter).await?;
    reporter.finish();

    if !cli.quiet {
        print_summary(&summary, cli.dry_run);
    }

    if !summary.errors.is_empty() {
        for e in &summary.errors {
            eprintln!("error: {e}");
        }
        std::process::exit(1);
    }
    Ok(())
}

/// Progress reporter backed by an `indicatif` progress bar.
struct BarReporter {
    bar: ProgressBar,
    verbose: bool,
}

impl BarReporter {
    fn new(cli: &Cli) -> Self {
        let bar = if cli.quiet {
            ProgressBar::hidden()
        } else {
            let b = ProgressBar::new(0);
            b.set_style(
                ProgressStyle::with_template(
                    "{spinner:.green.bold} {prefix} │{bar:26.green/dim}│ {percent:>3}% ({pos}/{len}) {msg:.dim}",
                )
                .unwrap()
                .progress_chars("█▉▊▋▌▍▎▏─")
                .tick_chars("⣾⣽⣻⢿⡿⣟⣯⣷ "),
            );
            b.set_prefix("🌱 grass");
            b.enable_steady_tick(Duration::from_millis(90));
            b
        };
        Self {
            bar,
            verbose: cli.verbose && !cli.quiet,
        }
    }
}

impl Reporter for BarReporter {
    fn set_total(&self, total: usize) {
        self.bar.set_length(total as u64);
    }

    fn gist_done(&self, action: Action, gist: &Gist) {
        let label = gist_label(gist);
        if self.verbose {
            let verb = match action {
                Action::Create => "new",
                Action::Update => "updated",
                Action::Skip => "skip",
            };
            self.bar.println(format!("  {verb:>7}  {label}"));
        }
        let icon = match action {
            Action::Create => "✚",
            Action::Update => "↻",
            Action::Skip => "·",
        };
        let short: String = label.chars().take(28).collect();
        self.bar.set_message(format!("{icon} {short}"));
        self.bar.inc(1);
    }

    fn finish(&self) {
        self.bar.finish_and_clear();
    }
}

/// A short human-readable label for a gist (description or first filename).
fn gist_label(gist: &Gist) -> String {
    let desc = gist.description.as_deref().unwrap_or("").trim();
    if !desc.is_empty() {
        desc.chars().take(60).collect()
    } else {
        gist.first_filename().unwrap_or(&gist.id).to_string()
    }
}

/// Print a one-line summary of the run to stdout.
fn print_summary(s: &Summary, dry_run: bool) {
    let mut parts = vec![
        format!("{} new", s.created),
        format!("{} updated", s.updated),
        format!("{} unchanged", s.skipped),
    ];
    if s.pruned > 0 {
        parts.push(format!("{} pruned", s.pruned));
    }
    if !s.errors.is_empty() {
        parts.push(format!("{} errors", s.errors.len()));
    }
    let verb = if dry_run { "planned" } else { "written" };
    println!(
        "grass: {} ({} files {})",
        parts.join(", "),
        s.files_written,
        verb
    );
}
