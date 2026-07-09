//! GitHub Gists API client.

use crate::model::Gist;
use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use reqwest::header::{HeaderMap, HeaderValue, ACCEPT, AUTHORIZATION};
use reqwest::Client;
use std::time::Duration;

const API_ROOT: &str = "https://api.github.com";
const USER_AGENT_STR: &str = concat!("grass/", env!("CARGO_PKG_VERSION"));
const PER_PAGE: u32 = 100;
const RAW_RETRIES: u32 = 3;

/// Abstraction over the source of gists so the sync engine can be tested
/// against a fake without touching the network.
#[async_trait]
pub trait GistSource {
    /// List every gist owned by the authenticated user.
    async fn list_gists(&self) -> Result<Vec<Gist>>;

    /// Fetch the raw bytes of a file from its `raw_url`.
    async fn fetch_raw(&self, url: &str) -> Result<Vec<u8>>;
}

/// Live client for the GitHub REST API.
///
/// Uses two HTTP clients: an authenticated one for `api.github.com`, and an
/// unauthenticated one for raw file downloads. `raw_url`s are capability URLs
/// (they embed a commit SHA) and work without auth even for secret gists, so we
/// avoid sending the token to `gist.githubusercontent.com`.
pub struct GitHubClient {
    api: Client,
    raw: Client,
    api_root: String,
}

impl GitHubClient {
    /// Build a client against the public GitHub API.
    pub fn new(token: &str) -> Result<Self> {
        Self::with_root(token, API_ROOT)
    }

    /// Build a client against a custom API root (used in tests).
    pub fn with_root(token: &str, api_root: &str) -> Result<Self> {
        let mut headers = HeaderMap::new();
        headers.insert(
            ACCEPT,
            HeaderValue::from_static("application/vnd.github+json"),
        );
        headers.insert(
            "X-GitHub-Api-Version",
            HeaderValue::from_static("2022-11-28"),
        );
        let mut auth = HeaderValue::from_str(&format!("Bearer {token}"))
            .context("building authorization header")?;
        auth.set_sensitive(true);
        headers.insert(AUTHORIZATION, auth);

        let api = Client::builder()
            .user_agent(USER_AGENT_STR)
            .default_headers(headers)
            .build()
            .context("building API HTTP client")?;
        let raw = Client::builder()
            .user_agent(USER_AGENT_STR)
            .build()
            .context("building raw HTTP client")?;

        Ok(Self {
            api,
            raw,
            api_root: api_root.trim_end_matches('/').to_string(),
        })
    }

    /// Single attempt to download a raw URL.
    async fn try_fetch_raw(&self, url: &str) -> Result<Vec<u8>> {
        let resp = self
            .raw
            .get(url)
            .send()
            .await
            .with_context(|| format!("requesting {url}"))?;
        let status = resp.status();
        if !status.is_success() {
            bail!("HTTP {status}");
        }
        let bytes = resp
            .bytes()
            .await
            .with_context(|| format!("reading body of {url}"))?;
        Ok(bytes.to_vec())
    }
}

#[async_trait]
impl GistSource for GitHubClient {
    async fn list_gists(&self) -> Result<Vec<Gist>> {
        let mut all = Vec::new();
        let mut page = 1u32;
        loop {
            let url = format!("{}/gists?per_page={PER_PAGE}&page={page}", self.api_root);
            let resp = self
                .api
                .get(&url)
                .send()
                .await
                .with_context(|| format!("requesting {url}"))?;
            let status = resp.status();
            if !status.is_success() {
                let body = resp.text().await.unwrap_or_default();
                bail!("GitHub API returned {status} for {url}: {}", body.trim());
            }
            let batch: Vec<Gist> = resp
                .json()
                .await
                .with_context(|| format!("parsing gist list page {page}"))?;
            let count = batch.len();
            all.extend(batch);
            if count < PER_PAGE as usize {
                break;
            }
            page += 1;
        }
        Ok(all)
    }

    async fn fetch_raw(&self, url: &str) -> Result<Vec<u8>> {
        let mut last_err = None;
        for attempt in 0..RAW_RETRIES {
            if attempt > 0 {
                tokio::time::sleep(Duration::from_millis(400 * attempt as u64)).await;
            }
            match self.try_fetch_raw(url).await {
                Ok(bytes) => return Ok(bytes),
                Err(e) => last_err = Some(e),
            }
        }
        Err(last_err
            .unwrap_or_else(|| anyhow::anyhow!("failed to download {url}"))
            .context(format!("downloading {url} after {RAW_RETRIES} attempts")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{header, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn list_gists_sends_auth_and_parses() {
        let server = MockServer::start().await;
        let body = serde_json::json!([
            {
                "id": "aaa111",
                "description": "first",
                "public": true,
                "created_at": "2020-01-01T00:00:00Z",
                "updated_at": "2020-01-02T00:00:00Z",
                "files": {
                    "a.txt": {
                        "filename": "a.txt",
                        "raw_url": "https://example/a.txt",
                        "size": 3,
                        "type": "text/plain",
                        "language": null
                    }
                }
            }
        ]);
        Mock::given(method("GET"))
            .and(path("/gists"))
            .and(query_param("page", "1"))
            .and(header("authorization", "Bearer testtoken"))
            .and(header("accept", "application/vnd.github+json"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(&server)
            .await;

        let client = GitHubClient::with_root("testtoken", &server.uri()).unwrap();
        let gists = client.list_gists().await.unwrap();
        assert_eq!(gists.len(), 1);
        assert_eq!(gists[0].id, "aaa111");
        assert_eq!(gists[0].first_filename(), Some("a.txt"));
    }

    #[tokio::test]
    async fn fetch_raw_returns_bytes() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/raw/file"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(b"hello".to_vec()))
            .mount(&server)
            .await;

        let client = GitHubClient::with_root("testtoken", &server.uri()).unwrap();
        let url = format!("{}/raw/file", server.uri());
        let bytes = client.fetch_raw(&url).await.unwrap();
        assert_eq!(bytes, b"hello");
    }
}
