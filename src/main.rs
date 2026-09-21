//! grass — download and sync all of your GitHub gists to a local directory.

mod auth;
mod cli;
mod client;
mod model;
mod naming;
mod search;
mod state;
mod sync;

use anyhow::Result;
use clap::Parser;
use cli::{Cli, Command, SearchArgs};
use client::GitHubClient;
use console::{style, truncate_str, Emoji, Term};
use indicatif::{ProgressBar, ProgressStyle};
use model::Gist;
use search::{LineMatch, Query, SearchResults};
use std::path::Path;
use std::time::Duration;
use sync::{Action, Reporter, Summary, SyncOptions};

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    match &cli.command {
        Some(Command::Search(args)) => run_search(&cli.output, args),
        None => run_sync(&cli).await,
    }
}

/// Download new and changed gists into the output directory.
async fn run_sync(cli: &Cli) -> Result<()> {
    let token = auth::resolve_token(cli.token.clone())?;
    let client = GitHubClient::new(&token)?;

    let opts = SyncOptions {
        output: cli.output.clone(),
        concurrency: cli.concurrency as usize,
        prune: cli.prune,
        dry_run: cli.dry_run,
        force: cli.force,
    };

    let reporter = BarReporter::new(cli);
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

/// Search the downloaded gists and print the matches, exiting with status 1
/// (like grep) when nothing matched.
fn run_search(dir: &Path, args: &SearchArgs) -> Result<()> {
    let query = Query {
        pattern: args.pattern.clone(),
        ignore_case: args.ignore_case,
        fixed_strings: args.fixed_strings,
        word: args.word,
    };
    let results = search::run(dir, &query)?;
    if let Some(warning) = &results.warning {
        eprintln!("warning: {warning}");
    }
    if results.gists.is_empty() {
        let pattern = style(&query.pattern).bold();
        let dir = style(dir.display()).cyan();
        println!("No matches for {pattern} in {dir}");
        std::process::exit(1);
    }
    print_search_results(&results);
    Ok(())
}

/// Print search results grouped by gist: its title and URL, then each
/// matching file with its highlighted lines, and a one-line summary.
fn print_search_results(results: &SearchResults) {
    let all_lines = || {
        results
            .gists
            .iter()
            .flat_map(|g| &g.files)
            .flat_map(|f| &f.lines)
    };
    let number_width = all_lines()
        .map(|l| l.line_number.to_string().len())
        .max()
        .unwrap_or(1);
    // Room left for the line text after the gutter (indent, number, bar),
    // when printing to a terminal. Piped output is never clipped.
    let text_width = Term::stdout()
        .size_checked()
        .map(|(_, cols)| (cols as usize).saturating_sub(number_width + 7).max(20));

    let bullet = style("●").green().bold();
    let bar = style("│").dim();
    for gist in &results.gists {
        println!("{bullet} {}", style(gist.title()).bold());
        if let Some(url) = &gist.url {
            println!("  {}", style(url).cyan().underlined());
        }
        for file in &gist.files {
            println!("  {}", style(&file.filename).magenta());
            for line in &file.lines {
                let number = style(format!("{:>number_width$}", line.line_number)).dim();
                println!("    {number} {bar} {}", highlight_line(line, text_width));
            }
        }
        println!();
    }

    let matches: usize = all_lines().map(|l| l.ranges.len().max(1)).sum();
    let files: usize = results.gists.iter().map(|g| g.files.len()).sum();
    let parts = [
        style(count(matches, "match", "matches"))
            .green()
            .bold()
            .to_string(),
        style(count(files, "file", "files")).bold().to_string(),
        style(count(results.gists.len(), "gist", "gists"))
            .bold()
            .to_string(),
    ];
    let sparkle = Emoji("✨ ", "");
    let sep = style(" · ").dim().to_string();
    let via = style(format!("(via {})", results.engine.program())).dim();
    println!("{sparkle}{}  {via}", parts.join(&sep));
}

/// `n` followed by the singular or plural noun.
fn count(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

/// Render a matching line with its matches highlighted. Indentation is
/// dropped, and when `width` is given the line is clipped to fit — scrolling
/// right first, if needed, so the first match stays in view.
fn highlight_line(line: &LineMatch, width: Option<usize>) -> String {
    let text = &line.text;
    let indent = text.len() - text.trim_start().len();

    // Alternating (text, is_match) segments, always starting with a
    // (possibly empty) non-match.
    let mut pos = line.ranges.first().map_or(indent, |r| r.start.min(indent));
    let mut segments: Vec<(String, bool)> = Vec::new();
    for r in &line.ranges {
        if r.start < pos {
            continue;
        }
        segments.push((clean(&text[pos..r.start]), false));
        segments.push((clean(&text[r.clone()]), true));
        pos = r.end;
    }
    segments.push((clean(text[pos..].trim_end()), false));

    if let (Some(width), Some((first_match, _))) = (width, segments.get(1)) {
        let lead = segments[0].0.chars().count();
        let keep = width / 4;
        if lead > keep && lead + first_match.chars().count() > width {
            let clipped: String = segments[0].0.chars().skip(lead - keep).collect();
            segments[0].0 = format!("…{clipped}");
        }
    }

    let rendered: String = segments
        .iter()
        .map(|(s, is_match)| {
            if *is_match {
                style(s).black().on_yellow().to_string()
            } else {
                s.clone()
            }
        })
        .collect();
    match width {
        Some(width) => truncate_str(&rendered, width, "…").into_owned(),
        None => rendered,
    }
}

/// Make file text safe to print on one line: expand tabs, and replace control
/// characters, which could otherwise move the cursor or restyle the terminal.
fn clean(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\t' => out.push_str("    "),
            c if c.is_control() => out.push(char::REPLACEMENT_CHARACTER),
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
// Match ranges are data here; a one-element Vec of them is intended.
#[allow(clippy::single_range_in_vec_init)]
mod tests {
    use super::*;

    fn line(text: &str, ranges: Vec<std::ops::Range<usize>>) -> LineMatch {
        LineMatch {
            line_number: 1,
            text: text.to_string(),
            ranges,
        }
    }

    fn plain(s: &str) -> String {
        console::strip_ansi_codes(s).into_owned()
    }

    #[test]
    fn highlight_drops_indentation() {
        let l = line("    let x = 1;  ", vec![8..9]);
        assert_eq!(plain(&highlight_line(&l, None)), "let x = 1;");
    }

    #[test]
    fn highlight_keeps_matched_indentation() {
        let l = line("  x", vec![0..3]);
        assert_eq!(plain(&highlight_line(&l, None)), "  x");
    }

    #[test]
    fn highlight_clips_to_width() {
        let l = line("abcdefghij needle", vec![0..3]);
        assert_eq!(plain(&highlight_line(&l, Some(8))), "abcdefg…");
    }

    #[test]
    fn highlight_scrolls_to_first_match() {
        let text = format!("{}needle{}", "a".repeat(100), "b".repeat(100));
        let l = line(&text, vec![100..106]);
        let out = plain(&highlight_line(&l, Some(40)));
        assert_eq!(out.chars().count(), 40);
        assert!(out.starts_with('…'));
        assert!(out.ends_with('…'));
        assert!(out.contains("needle"));
    }

    #[test]
    fn highlight_sanitizes_control_characters() {
        let l = line("a\tb\x1b[31mc", vec![]);
        assert_eq!(plain(&highlight_line(&l, None)), "a    b\u{fffd}[31mc");
    }

    #[test]
    fn count_pluralizes() {
        assert_eq!(count(1, "gist", "gists"), "1 gist");
        assert_eq!(count(2, "match", "matches"), "2 matches");
    }
}
