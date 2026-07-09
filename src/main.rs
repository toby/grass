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
use console::{style, Emoji};
use indicatif::{ProgressBar, ProgressStyle};
use model::Gist;
use std::path::Path;
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
            // Start with an unknown length so the bar renders at 0% while the
            // gist list is still loading. indicatif treats an explicit length
            // of 0 as 100%, but an unknown (None) length as 0%; set_total fills
            // in the real count once it's known.
            let b = ProgressBar::no_length();
            b.set_style(bar_style());
            b.set_prefix(bar_prefix(&cli.output, cli.dry_run));
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
        // Fill in the real count now that it's known; the bar style is already
        // set in `new`, and the bar has been showing 0% until this point.
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
        let short: String = label.chars().take(28).collect();
        let msg = match action {
            Action::Create => style(format!("✚ {short}")).green(),
            Action::Update => style(format!("↻ {short}")).yellow(),
            Action::Skip => style(format!("· {short}")).dim(),
        };
        self.bar.set_message(msg.for_stderr().to_string());
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

/// Build the styled prefix shown at the start of the progress bar line: the
/// tool mark, an arrow, and the destination directory (with a dry-run hint).
fn bar_prefix(output: &Path, dry_run: bool) -> String {
    let mark = style("grass").green().bold().for_stderr();
    let arrow = style("→").dim().for_stderr();
    let dest = style(output.display()).cyan().for_stderr();
    let mut prefix = format!("🌱 {mark} {arrow} {dest}");
    if dry_run {
        let hint = style("(dry run)").yellow().for_stderr();
        prefix.push_str(&format!(" {hint}"));
    }
    prefix
}

/// Full progress-bar style used for the whole run.
fn bar_style() -> ProgressStyle {
    ProgressStyle::with_template(
        "{spinner:.green.bold} {prefix} │{bar:24.green/dim}│ {percent:>3}% {pos}/{len} {msg}",
    )
    .unwrap()
    .progress_chars("█▉▊▋▌▍▎▏─")
    .tick_chars("⣾⣽⣻⢿⡿⣟⣯⣷ ")
}

/// Print a one-line summary of the run to stdout.
fn print_summary(s: &Summary, dry_run: bool) {
    let mut parts = vec![
        style(format!("{} new", s.created))
            .green()
            .bold()
            .to_string(),
        style(format!("{} updated", s.updated))
            .yellow()
            .bold()
            .to_string(),
        style(format!("{} unchanged", s.skipped)).dim().to_string(),
    ];
    if s.pruned > 0 {
        parts.push(
            style(format!("{} pruned", s.pruned))
                .magenta()
                .bold()
                .to_string(),
        );
    }
    if !s.errors.is_empty() {
        parts.push(
            style(format!("{} errors", s.errors.len()))
                .red()
                .bold()
                .to_string(),
        );
    }
    let sparkle = Emoji("✨ ", "");
    let verb = if dry_run { "planned" } else { "written" };
    let files = style(format!("({} files {})", s.files_written, verb)).dim();
    let sep = style(" · ").dim().to_string();
    println!("{sparkle}{}  {files}", parts.join(&sep));
}
