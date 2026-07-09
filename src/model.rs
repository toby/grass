//! Data model for the subset of the GitHub Gists API that `grass` consumes.

use serde::Deserialize;
use std::collections::BTreeMap;

/// A single gist as returned by the GitHub REST API list endpoint
/// (`GET /gists`).
#[derive(Debug, Clone, Deserialize)]
pub struct Gist {
    pub id: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub public: bool,
    pub created_at: String,
    pub updated_at: String,
    #[serde(default)]
    pub html_url: Option<String>,
    /// Files keyed by filename. The list endpoint provides metadata (including
    /// `raw_url`) but **not** the file `content`.
    #[serde(default)]
    pub files: BTreeMap<String, GistFile>,
}

/// One file within a gist.
#[derive(Debug, Clone, Deserialize)]
pub struct GistFile {
    pub filename: String,
    /// URL to the raw bytes of this file at its current revision. `grass`
    /// downloads files from here rather than from the (truncated) API content.
    #[serde(default)]
    pub raw_url: Option<String>,
    #[serde(default)]
    pub size: u64,
    #[serde(rename = "type", default)]
    pub content_type: Option<String>,
    #[serde(default)]
    pub language: Option<String>,
}

impl Gist {
    /// The first filename in deterministic (sorted) order, if any.
    pub fn first_filename(&self) -> Option<&str> {
        self.files.values().next().map(|f| f.filename.as_str())
    }
}
