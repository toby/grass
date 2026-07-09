//! Slug and folder-name generation for gists.

use crate::model::Gist;

/// Maximum length of the slug portion of a folder name.
const MAX_SLUG_LEN: usize = 50;
/// Number of leading characters of the gist id used to disambiguate folders.
const SHORT_ID_LEN: usize = 8;

/// Convert an arbitrary string into a filesystem-friendly slug.
///
/// Lowercases ASCII, replaces any run of non-alphanumeric characters (including
/// unicode) with a single `-`, and trims leading/trailing `-`. The result is
/// pure ASCII, capped at [`MAX_SLUG_LEN`] characters. Returns an empty string
/// when the input has no alphanumeric characters.
pub fn slugify(input: &str) -> String {
    let mut slug = String::with_capacity(input.len());
    for c in input.chars() {
        if c.is_ascii_alphanumeric() {
            slug.push(c.to_ascii_lowercase());
        } else if !slug.ends_with('-') && !slug.is_empty() {
            slug.push('-');
        }
    }
    // slug is pure ASCII, so byte-based trimming/truncation is char-safe.
    while slug.ends_with('-') {
        slug.pop();
    }
    if slug.len() > MAX_SLUG_LEN {
        slug.truncate(MAX_SLUG_LEN);
        while slug.ends_with('-') {
            slug.pop();
        }
    }
    slug
}

/// Compute the local folder name for a gist: `<slug>-<shortid>`.
///
/// The slug is derived from the description, falling back to the stem of the
/// first filename, and finally to `"gist"` if neither yields usable text. The
/// short id keeps folders unique even when descriptions collide.
pub fn folder_name(gist: &Gist) -> String {
    let short_id: String = gist.id.chars().take(SHORT_ID_LEN).collect();

    let mut slug = slugify(gist.description.as_deref().unwrap_or(""));
    if slug.is_empty() {
        if let Some(first) = gist.first_filename() {
            let stem = match first.rsplit_once('.') {
                Some((s, _)) if !s.is_empty() => s,
                _ => first,
            };
            slug = slugify(stem);
        }
    }
    if slug.is_empty() {
        slug = "gist".to_string();
    }

    if short_id.is_empty() {
        slug
    } else {
        format!("{slug}-{short_id}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::GistFile;
    use std::collections::BTreeMap;

    fn gist(id: &str, description: Option<&str>, filenames: &[&str]) -> Gist {
        let mut files = BTreeMap::new();
        for name in filenames {
            files.insert(
                name.to_string(),
                GistFile {
                    filename: name.to_string(),
                    raw_url: Some(format!("https://example/{name}")),
                    size: 0,
                    content_type: None,
                    language: None,
                },
            );
        }
        Gist {
            id: id.to_string(),
            description: description.map(str::to_string),
            public: true,
            created_at: "2020-01-01T00:00:00Z".to_string(),
            updated_at: "2020-01-01T00:00:00Z".to_string(),
            html_url: None,
            files,
        }
    }

    #[test]
    fn slugify_basic() {
        assert_eq!(slugify("Hello, World!"), "hello-world");
        assert_eq!(slugify("  spaced   out  "), "spaced-out");
        assert_eq!(slugify("Already-slugified"), "already-slugified");
        assert_eq!(slugify("multiple___separators"), "multiple-separators");
    }

    #[test]
    fn slugify_empty_and_symbols() {
        assert_eq!(slugify(""), "");
        assert_eq!(slugify("!!!"), "");
        assert_eq!(slugify("---"), "");
    }

    #[test]
    fn slugify_unicode_is_separator() {
        assert_eq!(slugify("café notes"), "caf-notes");
        assert_eq!(slugify("emoji 🎉 party"), "emoji-party");
    }

    #[test]
    fn slugify_length_cap() {
        let long = "a".repeat(200);
        assert_eq!(slugify(&long).len(), MAX_SLUG_LEN);
        // A cut landing on a separator should not leave a trailing dash.
        let s = slugify(&format!("{}-tail", "b".repeat(49)));
        assert!(!s.ends_with('-'));
        assert!(s.len() <= MAX_SLUG_LEN);
    }

    #[test]
    fn folder_name_from_description() {
        let g = gist("abcdef1234567890", Some("My Cool Notes"), &["notes.md"]);
        assert_eq!(folder_name(&g), "my-cool-notes-abcdef12");
    }

    #[test]
    fn folder_name_falls_back_to_filename() {
        let g = gist("abcdef1234567890", Some(""), &["deploy.sh"]);
        assert_eq!(folder_name(&g), "deploy-abcdef12");
    }

    #[test]
    fn folder_name_dotfile_uses_full_name() {
        let g = gist("abcdef1234567890", None, &[".bashrc"]);
        assert_eq!(folder_name(&g), "bashrc-abcdef12");
    }

    #[test]
    fn folder_name_final_fallback() {
        let g = gist("abcdef1234567890", Some("***"), &["!!!"]);
        assert_eq!(folder_name(&g), "gist-abcdef12");
    }
}
