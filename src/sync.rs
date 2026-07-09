//! The sync engine: classify gists, download them concurrently, and reconcile
//! the local output directory with what exists on GitHub.

use crate::client::GistSource;
use crate::model::Gist;
use crate::naming::folder_name;
use crate::state::{GistRecord, SyncState};
use anyhow::{Context, Result};
use futures::stream::{self, StreamExt};
use serde::Serialize;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// Name of the per-gist metadata sidecar written into each gist folder.
pub const META_FILE: &str = ".grass-meta.json";

/// What to do with a single gist on this run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Not seen before — download fresh.
    Create,
    /// Known but changed remotely (or `--force`) — refresh.
    Update,
    /// Unchanged since the last sync — skip.
    Skip,
}

/// Options controlling a sync run.
#[derive(Debug, Clone)]
pub struct SyncOptions {
    pub output: PathBuf,
    pub concurrency: usize,
    pub prune: bool,
    pub dry_run: bool,
    pub force: bool,
}

/// Tallies produced by a sync run.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Summary {
    pub created: usize,
    pub updated: usize,
    pub skipped: usize,
    pub pruned: usize,
    pub files_written: usize,
    pub errors: Vec<String>,
}

/// Receives progress notifications during a sync. All methods default to no-ops
/// so callers (and tests) can implement only what they need.
pub trait Reporter {
    fn set_total(&self, _total: usize) {}
    fn gist_done(&self, _action: Action, _gist: &Gist) {}
    fn finish(&self) {}
}

/// A reporter that does nothing (used by tests).
#[cfg(test)]
pub struct NoopReporter;
#[cfg(test)]
impl Reporter for NoopReporter {}

/// Decide what to do with `gist` given any `prior` record and the `force` flag.
pub fn classify(gist: &Gist, prior: Option<&GistRecord>, force: bool) -> Action {
    match prior {
        None => Action::Create,
        Some(_) if force => Action::Update,
        Some(rec) if rec.updated_at != gist.updated_at => Action::Update,
        Some(_) => Action::Skip,
    }
}

/// Run a full sync: download new/changed gists, optionally prune deleted ones,
/// and persist updated state. Per-gist failures are collected into
/// [`Summary::errors`] rather than aborting the whole run.
pub async fn run<S, R>(source: &S, opts: &SyncOptions, reporter: &R) -> Result<Summary>
where
    S: GistSource,
    R: Reporter,
{
    if !opts.dry_run {
        tokio::fs::create_dir_all(&opts.output)
            .await
            .with_context(|| format!("creating output dir {}", opts.output.display()))?;
    }

    let mut state = SyncState::load(&opts.output);
    let gists = source
        .list_gists()
        .await
        .context("listing gists from GitHub")?;
    reporter.set_total(gists.len());

    let remote_ids: BTreeSet<&str> = gists.iter().map(|g| g.id.as_str()).collect();
    let mut summary = Summary::default();

    // Classify first so unchanged gists are reported without a download task.
    let mut to_process: Vec<&Gist> = Vec::new();
    for gist in &gists {
        if classify(gist, state.gists.get(&gist.id), opts.force) == Action::Skip {
            summary.skipped += 1;
            reporter.gist_done(Action::Skip, gist);
        } else {
            to_process.push(gist);
        }
    }

    // Snapshot each gist's prior record up front so the download futures own
    // their inputs and don't borrow `state` (we mutate `state` below as
    // results stream in).
    let jobs: Vec<(&Gist, Option<GistRecord>)> = to_process
        .into_iter()
        .map(|gist| {
            let prior = state.gists.get(&gist.id).cloned();
            (gist, prior)
        })
        .collect();

    // Download new/updated gists with bounded concurrency, reporting progress
    // as each one finishes rather than after the whole batch completes — so the
    // bar actually advances while downloads are in flight.
    let mut in_flight = stream::iter(jobs.into_iter().map(|(gist, prior)| async move {
        let action = classify(gist, prior.as_ref(), opts.force);
        let outcome = process_gist(source, &opts.output, gist, prior.as_ref(), opts.dry_run).await;
        (gist, action, outcome)
    }))
    .buffer_unordered(opts.concurrency.max(1));

    while let Some((gist, action, outcome)) = in_flight.next().await {
        match outcome {
            Ok((record, written)) => {
                summary.files_written += written;
                match action {
                    Action::Create => summary.created += 1,
                    Action::Update => summary.updated += 1,
                    Action::Skip => {}
                }
                state.gists.insert(gist.id.clone(), record);
            }
            Err(e) => summary.errors.push(format!("{}: {e:#}", gist.id)),
        }
        reporter.gist_done(action, gist);
    }

    // Prune local folders whose gists no longer exist remotely.
    if opts.prune {
        let stale: Vec<(String, String)> = state
            .gists
            .iter()
            .filter(|(id, _)| !remote_ids.contains(id.as_str()))
            .map(|(id, rec)| (id.clone(), rec.folder.clone()))
            .collect();
        for (id, folder) in stale {
            if opts.dry_run {
                summary.pruned += 1;
                continue;
            }
            let dir = opts.output.join(&folder);
            match tokio::fs::remove_dir_all(&dir).await {
                Ok(()) => {
                    summary.pruned += 1;
                    state.gists.remove(&id);
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    summary.pruned += 1;
                    state.gists.remove(&id);
                }
                Err(e) => summary.errors.push(format!("pruning {folder}: {e}")),
            }
        }
    }

    if !opts.dry_run {
        state.save(&opts.output).context("saving sync state")?;
    }

    Ok(summary)
}

/// Download and write a single gist, returning its new [`GistRecord`] and the
/// number of files written (or that would be written, in dry-run mode).
async fn process_gist<S: GistSource>(
    source: &S,
    output: &Path,
    gist: &Gist,
    prior: Option<&GistRecord>,
    dry_run: bool,
) -> Result<(GistRecord, usize)> {
    let folder = folder_name(gist);
    let dir = output.join(&folder);

    // If the description (and thus slug) changed, move the old folder so we
    // keep history/untracked files instead of orphaning them.
    if let Some(prev) = prior {
        if prev.folder != folder && !dry_run {
            let old_dir = output.join(&prev.folder);
            if old_dir.exists() && !dir.exists() {
                let _ = tokio::fs::rename(&old_dir, &dir).await;
            }
        }
    }

    if !dry_run {
        tokio::fs::create_dir_all(&dir)
            .await
            .with_context(|| format!("creating {}", dir.display()))?;
    }

    let mut written = 0usize;
    let mut filenames = Vec::with_capacity(gist.files.len());
    for file in gist.files.values() {
        filenames.push(file.filename.clone());
        let Some(raw_url) = file.raw_url.as_deref() else {
            continue;
        };
        if dry_run {
            written += 1;
            continue;
        }
        let bytes = source
            .fetch_raw(raw_url)
            .await
            .with_context(|| format!("downloading {}", file.filename))?;
        let path = safe_file_path(&dir, &file.filename)?;
        tokio::fs::write(&path, &bytes)
            .await
            .with_context(|| format!("writing {}", path.display()))?;
        written += 1;
    }

    // Remove files that existed before but are no longer part of the gist.
    if let Some(prev) = prior {
        if !dry_run {
            for old in &prev.files {
                if !filenames.contains(old) {
                    if let Ok(path) = safe_file_path(&dir, old) {
                        let _ = tokio::fs::remove_file(&path).await;
                    }
                }
            }
        }
    }

    if !dry_run {
        write_meta(&dir, gist).await?;
    }

    Ok((
        GistRecord {
            updated_at: gist.updated_at.clone(),
            folder,
            files: filenames,
        },
        written,
    ))
}

/// Join `filename` onto `dir`, guarding against path traversal. Gist filenames
/// are flat, but we defensively use only the final path component.
fn safe_file_path(dir: &Path, filename: &str) -> Result<PathBuf> {
    let name = Path::new(filename)
        .file_name()
        .with_context(|| format!("refusing unsafe gist filename {filename:?}"))?;
    Ok(dir.join(name))
}

/// Metadata written to `.grass-meta.json` inside each gist folder.
#[derive(Serialize)]
struct Meta<'a> {
    id: &'a str,
    description: &'a str,
    public: bool,
    html_url: Option<&'a str>,
    created_at: &'a str,
    updated_at: &'a str,
    files: Vec<FileMeta<'a>>,
}

/// Per-file details recorded in the metadata sidecar.
#[derive(Serialize)]
struct FileMeta<'a> {
    filename: &'a str,
    size: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    language: Option<&'a str>,
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    content_type: Option<&'a str>,
}

async fn write_meta(dir: &Path, gist: &Gist) -> Result<()> {
    let meta = Meta {
        id: &gist.id,
        description: gist.description.as_deref().unwrap_or(""),
        public: gist.public,
        html_url: gist.html_url.as_deref(),
        created_at: &gist.created_at,
        updated_at: &gist.updated_at,
        files: gist
            .files
            .values()
            .map(|f| FileMeta {
                filename: &f.filename,
                size: f.size,
                language: f.language.as_deref(),
                content_type: f.content_type.as_deref(),
            })
            .collect(),
    };
    let json = serde_json::to_vec_pretty(&meta).context("serializing gist metadata")?;
    let path = dir.join(META_FILE);
    tokio::fs::write(&path, json)
        .await
        .with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::GistFile;
    use async_trait::async_trait;
    use std::collections::{BTreeMap, HashMap};

    /// In-memory gist source for tests.
    #[derive(Default, Clone)]
    struct FakeSource {
        gists: Vec<Gist>,
        contents: HashMap<String, Vec<u8>>,
    }

    #[async_trait]
    impl GistSource for FakeSource {
        async fn list_gists(&self) -> Result<Vec<Gist>> {
            Ok(self.gists.clone())
        }
        async fn fetch_raw(&self, url: &str) -> Result<Vec<u8>> {
            self.contents
                .get(url)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("no content for {url}"))
        }
    }

    fn make_gist(id: &str, desc: &str, updated: &str, files: &[(&str, &str)]) -> Gist {
        let mut map = BTreeMap::new();
        for (name, _content) in files {
            let raw_url = format!("https://raw/{id}/{name}");
            map.insert(
                name.to_string(),
                GistFile {
                    filename: name.to_string(),
                    raw_url: Some(raw_url),
                    size: 0,
                    content_type: None,
                    language: None,
                },
            );
        }
        Gist {
            id: id.to_string(),
            description: Some(desc.to_string()),
            public: true,
            created_at: "2020-01-01T00:00:00Z".to_string(),
            updated_at: updated.to_string(),
            html_url: Some(format!("https://gist.github.com/{id}")),
            files: map,
        }
    }

    fn source_from(gists: &[Gist], files: &[(&str, &str, &str)]) -> FakeSource {
        let mut contents = HashMap::new();
        for (id, name, content) in files {
            contents.insert(
                format!("https://raw/{id}/{name}"),
                content.as_bytes().to_vec(),
            );
        }
        FakeSource {
            gists: gists.to_vec(),
            contents,
        }
    }

    fn opts(dir: &Path) -> SyncOptions {
        SyncOptions {
            output: dir.to_path_buf(),
            concurrency: 4,
            prune: false,
            dry_run: false,
            force: false,
        }
    }

    #[tokio::test]
    async fn creates_then_skips_on_rerun() {
        let dir = tempfile::tempdir().unwrap();
        let gists = vec![
            make_gist("aaa11111", "First", "t1", &[("a.txt", "AAA")]),
            make_gist("bbb22222", "Second", "t1", &[("b.txt", "BBB")]),
        ];
        let source = source_from(
            &gists,
            &[("aaa11111", "a.txt", "AAA"), ("bbb22222", "b.txt", "BBB")],
        );

        let s1 = run(&source, &opts(dir.path()), &NoopReporter)
            .await
            .unwrap();
        assert_eq!(s1.created, 2);
        assert_eq!(s1.skipped, 0);
        assert_eq!(s1.files_written, 2);

        let a = dir.path().join("first-aaa11111").join("a.txt");
        assert_eq!(std::fs::read_to_string(&a).unwrap(), "AAA");
        assert!(dir.path().join("first-aaa11111").join(META_FILE).exists());
        assert!(SyncState::path(dir.path()).exists());

        // Re-run with identical data: everything is skipped, nothing written.
        let s2 = run(&source, &opts(dir.path()), &NoopReporter)
            .await
            .unwrap();
        assert_eq!(s2.created, 0);
        assert_eq!(s2.updated, 0);
        assert_eq!(s2.skipped, 2);
        assert_eq!(s2.files_written, 0);
    }

    #[tokio::test]
    async fn updates_changed_gist() {
        let dir = tempfile::tempdir().unwrap();
        let g1 = vec![make_gist("aaa11111", "First", "t1", &[("a.txt", "AAA")])];
        let src1 = source_from(&g1, &[("aaa11111", "a.txt", "AAA")]);
        run(&src1, &opts(dir.path()), &NoopReporter).await.unwrap();

        // New revision: updated_at changes and content changes.
        let g2 = vec![make_gist("aaa11111", "First", "t2", &[("a.txt", "AAA2")])];
        let src2 = source_from(&g2, &[("aaa11111", "a.txt", "AAA2")]);
        let s = run(&src2, &opts(dir.path()), &NoopReporter).await.unwrap();
        assert_eq!(s.updated, 1);
        assert_eq!(s.created, 0);

        let a = dir.path().join("first-aaa11111").join("a.txt");
        assert_eq!(std::fs::read_to_string(&a).unwrap(), "AAA2");
    }

    #[tokio::test]
    async fn removes_deleted_file_within_gist() {
        let dir = tempfile::tempdir().unwrap();
        let g1 = vec![make_gist(
            "aaa11111",
            "First",
            "t1",
            &[("a.txt", "A"), ("b.txt", "B")],
        )];
        let src1 = source_from(
            &g1,
            &[("aaa11111", "a.txt", "A"), ("aaa11111", "b.txt", "B")],
        );
        run(&src1, &opts(dir.path()), &NoopReporter).await.unwrap();
        assert!(dir.path().join("first-aaa11111").join("b.txt").exists());

        // b.txt removed from the gist in a new revision.
        let g2 = vec![make_gist("aaa11111", "First", "t2", &[("a.txt", "A")])];
        let src2 = source_from(&g2, &[("aaa11111", "a.txt", "A")]);
        run(&src2, &opts(dir.path()), &NoopReporter).await.unwrap();
        assert!(dir.path().join("first-aaa11111").join("a.txt").exists());
        assert!(!dir.path().join("first-aaa11111").join("b.txt").exists());
    }

    #[tokio::test]
    async fn renames_folder_when_description_changes() {
        let dir = tempfile::tempdir().unwrap();
        let g1 = vec![make_gist("aaa11111", "Old Name", "t1", &[("a.txt", "A")])];
        let src1 = source_from(&g1, &[("aaa11111", "a.txt", "A")]);
        run(&src1, &opts(dir.path()), &NoopReporter).await.unwrap();
        assert!(dir.path().join("old-name-aaa11111").exists());

        let g2 = vec![make_gist("aaa11111", "New Name", "t2", &[("a.txt", "A")])];
        let src2 = source_from(&g2, &[("aaa11111", "a.txt", "A")]);
        run(&src2, &opts(dir.path()), &NoopReporter).await.unwrap();
        assert!(!dir.path().join("old-name-aaa11111").exists());
        assert!(dir.path().join("new-name-aaa11111").join("a.txt").exists());
    }

    #[tokio::test]
    async fn prune_removes_deleted_gist() {
        let dir = tempfile::tempdir().unwrap();
        let g1 = vec![
            make_gist("aaa11111", "Keep", "t1", &[("a.txt", "A")]),
            make_gist("bbb22222", "Drop", "t1", &[("b.txt", "B")]),
        ];
        let src1 = source_from(
            &g1,
            &[("aaa11111", "a.txt", "A"), ("bbb22222", "b.txt", "B")],
        );
        run(&src1, &opts(dir.path()), &NoopReporter).await.unwrap();
        assert!(dir.path().join("drop-bbb22222").exists());

        // Second gist is gone from GitHub; prune should delete its folder.
        let g2 = vec![make_gist("aaa11111", "Keep", "t1", &[("a.txt", "A")])];
        let src2 = source_from(&g2, &[("aaa11111", "a.txt", "A")]);
        let mut o = opts(dir.path());
        o.prune = true;
        let s = run(&src2, &o, &NoopReporter).await.unwrap();
        assert_eq!(s.pruned, 1);
        assert!(!dir.path().join("drop-bbb22222").exists());
        assert!(dir.path().join("keep-aaa11111").exists());

        let state = SyncState::load(dir.path());
        assert!(!state.gists.contains_key("bbb22222"));
    }

    #[tokio::test]
    async fn force_redownloads_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let g = vec![make_gist("aaa11111", "First", "t1", &[("a.txt", "A")])];
        let src = source_from(&g, &[("aaa11111", "a.txt", "A")]);
        run(&src, &opts(dir.path()), &NoopReporter).await.unwrap();

        let mut o = opts(dir.path());
        o.force = true;
        let s = run(&src, &o, &NoopReporter).await.unwrap();
        assert_eq!(s.updated, 1);
        assert_eq!(s.skipped, 0);
    }

    #[tokio::test]
    async fn dry_run_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let g = vec![make_gist("aaa11111", "First", "t1", &[("a.txt", "A")])];
        let src = source_from(&g, &[("aaa11111", "a.txt", "A")]);

        let mut o = opts(dir.path());
        o.dry_run = true;
        let s = run(&src, &o, &NoopReporter).await.unwrap();
        assert_eq!(s.created, 1);
        assert_eq!(s.files_written, 1);
        assert!(!dir.path().join("first-aaa11111").exists());
        assert!(!SyncState::path(dir.path()).exists());
    }
}
