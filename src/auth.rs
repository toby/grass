//! Resolve the GitHub token used for API requests.

use anyhow::{bail, Result};
use std::process::Command;

/// Resolve a GitHub token using the following precedence:
/// 1. the `--token` flag (passed here as `explicit`)
/// 2. the `GITHUB_TOKEN` environment variable
/// 3. the `GH_TOKEN` environment variable
/// 4. `gh auth token` (the GitHub CLI)
pub fn resolve_token(explicit: Option<String>) -> Result<String> {
    if let Some(token) = explicit.and_then(non_empty) {
        return Ok(token);
    }
    for var in ["GITHUB_TOKEN", "GH_TOKEN"] {
        if let Some(token) = std::env::var(var).ok().and_then(non_empty) {
            return Ok(token);
        }
    }
    if let Some(token) = gh_auth_token() {
        return Ok(token);
    }
    bail!(
        "no GitHub token found. Provide --token, set GITHUB_TOKEN or GH_TOKEN, \
         or run `gh auth login` (grass falls back to `gh auth token`)."
    )
}

/// Trim a candidate token, returning `None` if it is blank.
fn non_empty(value: String) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// Try to obtain a token from the GitHub CLI (`gh auth token`).
fn gh_auth_token() -> Option<String> {
    let output = Command::new("gh").args(["auth", "token"]).output().ok()?;
    if !output.status.success() {
        return None;
    }
    non_empty(String::from_utf8(output.stdout).ok()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_token_wins() {
        assert_eq!(resolve_token(Some("  tok  ".to_string())).unwrap(), "tok");
    }

    #[test]
    fn blank_is_not_a_token() {
        assert_eq!(non_empty("   ".to_string()), None);
        assert_eq!(non_empty("x".to_string()), Some("x".to_string()));
    }
}
