//! On-disk sync state.
//!
//! Tracks which gists have been downloaded and at what revision so that
//! re-runs only fetch new or changed gists.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Name of the state file, stored at the root of the output directory.
pub const STATE_FILE: &str = ".grass-state.json";

/// The full sync state: a map from gist id to what we know about it locally.
#[derive(Debug, Default, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct SyncState {
    #[serde(default)]
    pub gists: BTreeMap<String, GistRecord>,
}

/// What `grass` recorded about a single gist on the last successful sync.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct GistRecord {
    /// The gist's `updated_at` timestamp at download time (change detector).
    pub updated_at: String,
    /// The local folder name (relative to the output dir) it was written to.
    pub folder: String,
    /// Filenames written, used to prune files removed from the gist.
    #[serde(default)]
    pub files: Vec<String>,
}

impl SyncState {
    /// Path to the state file within `dir`.
    pub fn path(dir: &Path) -> PathBuf {
        dir.join(STATE_FILE)
    }

    /// Load state from `<dir>/.grass-state.json`.
    ///
    /// A missing, unreadable, or corrupt file yields an empty state (i.e. a
    /// fresh sync) rather than an error, so a damaged state file self-heals.
    pub fn load(dir: &Path) -> Self {
        match std::fs::read(Self::path(dir)) {
            Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_default(),
            Err(_) => Self::default(),
        }
    }

    /// Persist state atomically: write to a temp file, then rename into place.
    pub fn save(&self, dir: &Path) -> Result<()> {
        let path = Self::path(dir);
        let tmp = dir.join(format!("{STATE_FILE}.tmp"));
        let json = serde_json::to_vec_pretty(self).context("serializing sync state")?;
        std::fs::write(&tmp, json).with_context(|| format!("writing {}", tmp.display()))?;
        std::fs::rename(&tmp, &path).with_context(|| format!("finalizing {}", path.display()))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_missing_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        let state = SyncState::load(dir.path());
        assert!(state.gists.is_empty());
    }

    #[test]
    fn save_then_load_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let mut state = SyncState::default();
        state.gists.insert(
            "abc".to_string(),
            GistRecord {
                updated_at: "2020-01-01T00:00:00Z".to_string(),
                folder: "notes-abc".to_string(),
                files: vec!["a.md".to_string(), "b.txt".to_string()],
            },
        );
        state.save(dir.path()).unwrap();
        let loaded = SyncState::load(dir.path());
        assert_eq!(state, loaded);
    }

    #[test]
    fn corrupt_file_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(SyncState::path(dir.path()), b"{not valid json").unwrap();
        let state = SyncState::load(dir.path());
        assert!(state.gists.is_empty());
    }
}
