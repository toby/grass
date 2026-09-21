//! Full-text search over downloaded gists.
//!
//! The matching itself is delegated to ripgrep (`rg`) when it is installed,
//! falling back to `grep` otherwise. Results are grouped by gist and file and
//! enriched with each gist's description and URL from its metadata sidecar.

use crate::state::STATE_FILE;
use crate::sync::META_FILE;
use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::io::ErrorKind;
use std::ops::Range;
use std::path::{Component, Path};
use std::process::{Command, Output, Stdio};

/// What to search for.
#[derive(Debug, Clone)]
pub struct Query {
    pub pattern: String,
    pub ignore_case: bool,
    pub fixed_strings: bool,
    pub word: bool,
}

/// The external tool that performed a search.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Engine {
    Ripgrep,
    Grep,
}

impl Engine {
    /// The executable name, as looked up on `PATH`.
    pub fn program(self) -> &'static str {
        match self {
            Engine::Ripgrep => "rg",
            Engine::Grep => "grep",
        }
    }
}

/// A single matching line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LineMatch {
    pub line_number: u64,
    /// The line's text, without its trailing newline.
    pub text: String,
    /// Byte ranges within `text` that matched the pattern.
    pub ranges: Vec<Range<usize>>,
}

/// All matching lines within one file of a gist.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileMatches {
    pub filename: String,
    pub lines: Vec<LineMatch>,
}

/// All matches within one gist, plus what its metadata says about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GistMatches {
    /// Local folder name (relative to the output dir).
    pub folder: String,
    pub description: Option<String>,
    pub url: Option<String>,
    pub files: Vec<FileMatches>,
}

impl GistMatches {
    /// A human-readable title: the description, or the folder name if blank.
    pub fn title(&self) -> &str {
        match self.description.as_deref().map(str::trim) {
            Some(desc) if !desc.is_empty() => desc,
            _ => &self.folder,
        }
    }
}

/// The outcome of a search.
#[derive(Debug)]
pub struct SearchResults {
    pub engine: Engine,
    /// Matches grouped by gist, sorted by folder name.
    pub gists: Vec<GistMatches>,
    /// Diagnostics from a search that failed partway (e.g. an unreadable file)
    /// but still produced matches.
    pub warning: Option<String>,
}

/// A matching line in a file somewhere under the search root.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Hit {
    /// Path relative to the search root, as reported by the tool.
    path: String,
    line: LineMatch,
}

/// Search every gist under `dir` for `query`.
pub fn run(dir: &Path, query: &Query) -> Result<SearchResults> {
    if !dir.is_dir() {
        bail!(
            "no gists found at {} — run `grass` to download them first, or pass --output",
            dir.display()
        );
    }

    let (engine, output) = match execute(Engine::Ripgrep, dir, query) {
        Err(e) if e.kind() == ErrorKind::NotFound => match execute(Engine::Grep, dir, query) {
            Err(e) if e.kind() == ErrorKind::NotFound => {
                bail!("searching requires ripgrep (`rg`) or `grep` on your PATH")
            }
            result => (Engine::Grep, result.context("running grep")?),
        },
        result => (Engine::Ripgrep, result.context("running rg")?),
    };

    let hits = match engine {
        Engine::Ripgrep => parse_rg(&output.stdout),
        Engine::Grep => parse_grep(&output.stdout),
    };

    // Both tools exit 0 when something matched, 1 when nothing did, and 2 on
    // error — which can still come with matches if only some files failed.
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    let failed = !matches!(output.status.code(), Some(0 | 1));
    if failed && hits.is_empty() {
        if stderr.is_empty() {
            bail!("{} failed ({})", engine.program(), output.status);
        }
        bail!("{stderr}");
    }
    let warning = (failed && !stderr.is_empty()).then_some(stderr);

    Ok(SearchResults {
        engine,
        gists: group(dir, hits),
        warning,
    })
}

/// Run `engine` over `dir`. An `Err` of kind `NotFound` means the tool isn't
/// installed.
fn execute(engine: Engine, dir: &Path, query: &Query) -> std::io::Result<Output> {
    command(engine, query)
        .current_dir(dir)
        .stdin(Stdio::null())
        .output()
}

/// Build the command line for `engine`, searching the current directory.
fn command(engine: Engine, query: &Query) -> Command {
    let mut cmd = Command::new(engine.program());
    match engine {
        Engine::Ripgrep => {
            // --json gives exact match offsets. --no-config keeps a user's
            // ripgreprc from changing the output. --hidden and --no-ignore
            // are needed because gists can be dotfiles (`.bashrc`) or contain
            // a `.gitignore`, and the output dir may itself be git-ignored.
            cmd.args(["--json", "--no-config", "--hidden", "--no-ignore"]);
            cmd.arg("--glob").arg(format!("!{META_FILE}"));
            cmd.arg("--glob").arg(format!("!{STATE_FILE}*"));
            if query.fixed_strings {
                cmd.arg("--fixed-strings");
            }
        }
        Engine::Grep => {
            // grep has no structured output, so ask it to color matches and
            // recover their positions from the escape codes. GREP_COLORS turns
            // off every color but the match itself (GNU grep; BSD grep only
            // ever colors matches).
            cmd.args(["-r", "-n", "-I", "--null", "--color=always"]);
            cmd.arg(format!("--exclude={META_FILE}"));
            cmd.arg(format!("--exclude={STATE_FILE}*"));
            cmd.arg(if query.fixed_strings { "-F" } else { "-E" });
            cmd.env("GREP_COLORS", "mt=01;31:sl=:cx=:fn=:ln=:bn=:se=:ne");
            cmd.env_remove("GREP_COLOR");
        }
    }
    if query.ignore_case {
        cmd.arg("-i");
    }
    if query.word {
        cmd.arg("-w");
    }
    cmd.arg("-e").arg(&query.pattern).arg(".");
    cmd
}

/// One line of `rg --json` output. Only `match` messages matter here.
#[derive(Deserialize)]
struct RgMessage {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    data: serde_json::Value,
}

#[derive(Deserialize)]
struct RgMatch {
    path: RgData,
    lines: RgData,
    line_number: Option<u64>,
    #[serde(default)]
    submatches: Vec<RgSubmatch>,
}

/// rg reports text as UTF-8 `text` when it can, otherwise as base64 `bytes`.
#[derive(Deserialize)]
struct RgData {
    text: Option<String>,
    bytes: Option<String>,
}

impl RgData {
    /// The text, and whether it came through verbatim (so byte offsets from
    /// rg still line up with it).
    fn decode(self) -> (String, bool) {
        match (self.text, self.bytes) {
            (Some(text), _) => (text, true),
            (None, Some(b64)) => {
                let bytes = decode_base64(&b64);
                (String::from_utf8_lossy(&bytes).into_owned(), false)
            }
            (None, None) => (String::new(), false),
        }
    }
}

#[derive(Deserialize)]
struct RgSubmatch {
    start: usize,
    end: usize,
}

/// Parse the output of `rg --json`.
fn parse_rg(stdout: &[u8]) -> Vec<Hit> {
    stdout
        .split(|&b| b == b'\n')
        .filter_map(|line| serde_json::from_slice::<RgMessage>(line).ok())
        .filter(|msg| msg.kind == "match")
        .filter_map(|msg| serde_json::from_value::<RgMatch>(msg.data).ok())
        .map(|m| {
            let (path, _) = m.path.decode();
            let (text, verbatim) = m.lines.decode();
            let ranges = if verbatim {
                m.submatches.iter().map(|s| s.start..s.end).collect()
            } else {
                Vec::new()
            };
            Hit {
                path,
                line: line_match(m.line_number.unwrap_or(0), text, ranges),
            }
        })
        .collect()
}

/// Parse the output of `grep -rn --null --color=always`: one
/// `path\0line:text` record per line, with matches wrapped in color codes.
fn parse_grep(stdout: &[u8]) -> Vec<Hit> {
    stdout
        .split(|&b| b == b'\n')
        .filter(|line| !line.is_empty())
        .filter_map(|line| {
            let (plain, ranges) = strip_sgr(&String::from_utf8_lossy(line));
            let (path, rest) = plain.split_once('\0')?;
            let (number, text) = rest.split_once(':')?;
            let line_number = number.parse().ok()?;
            // Rebase the highlighted ranges from the whole record onto `text`.
            let offset = plain.len() - text.len();
            let ranges = ranges
                .into_iter()
                .filter(|r| r.start >= offset)
                .map(|r| r.start - offset..r.end - offset)
                .collect();
            Some(Hit {
                path: path.to_string(),
                line: line_match(line_number, text.to_string(), ranges),
            })
        })
        .collect()
}

/// Build a [`LineMatch`], dropping the line terminator and any ranges that
/// are empty or don't fall on character boundaries.
fn line_match(line_number: u64, mut text: String, ranges: Vec<Range<usize>>) -> LineMatch {
    text.truncate(text.trim_end_matches(['\n', '\r']).len());
    let mut ranges: Vec<Range<usize>> = ranges
        .into_iter()
        .map(|r| r.start..r.end.min(text.len()))
        .filter(|r| {
            r.start < r.end && text.is_char_boundary(r.start) && text.is_char_boundary(r.end)
        })
        .collect();
    ranges.sort_by_key(|r| r.start);
    LineMatch {
        line_number,
        text,
        ranges,
    }
}

/// Remove ANSI SGR (color) escape codes from `s`, returning the plain text and
/// the byte ranges of it that were colored.
fn strip_sgr(s: &str) -> (String, Vec<Range<usize>>) {
    let mut plain = String::with_capacity(s.len());
    let mut ranges = Vec::new();
    let mut open: Option<usize> = None;
    let mut rest = s;
    while let Some(i) = rest.find('\x1b') {
        plain.push_str(&rest[..i]);
        rest = &rest[i + 1..];
        // A CSI sequence is `ESC [`, parameter bytes, then a final byte in
        // `@`..=`~`; SGR is the one ending in `m`. Others (like grep's `ESC[K`)
        // are dropped.
        let Some(body) = rest.strip_prefix('[') else {
            plain.push('\x1b');
            continue;
        };
        let Some(end) = body.find(|c: char| ('@'..='~').contains(&c)) else {
            plain.push('\x1b');
            continue;
        };
        if body[end..].starts_with('m') {
            let reset = body[..end]
                .split(';')
                .all(|p| p.trim_start_matches('0').is_empty());
            if !reset {
                open.get_or_insert(plain.len());
            } else if let Some(start) = open.take() {
                ranges.push(start..plain.len());
            }
        }
        rest = &body[end + 1..];
    }
    plain.push_str(rest);
    if let Some(start) = open {
        ranges.push(start..plain.len());
    }
    (plain, ranges)
}

/// Decode standard (padded or unpadded) base64, skipping invalid characters.
fn decode_base64(input: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(input.len() * 3 / 4);
    let mut acc = 0u32;
    let mut bits = 0;
    for c in input.bytes() {
        let value = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => continue,
        };
        acc = (acc << 6) | u32::from(value);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    out
}

/// Metadata read back from a gist's `.grass-meta.json` sidecar.
#[derive(Default, Deserialize)]
struct GistInfo {
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    html_url: Option<String>,
}

impl GistInfo {
    /// Load the sidecar for `folder`, or an empty record if it's missing or
    /// unreadable (the gist is still searchable, just without a URL).
    fn load(dir: &Path, folder: &str) -> Self {
        std::fs::read(dir.join(folder).join(META_FILE))
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default()
    }
}

/// Group hits by gist folder and file, in sorted order, and attach each gist's
/// metadata. Hits outside a gist folder (files at the top of `dir`) are dropped.
fn group(dir: &Path, hits: Vec<Hit>) -> Vec<GistMatches> {
    let mut folders: BTreeMap<String, BTreeMap<String, Vec<LineMatch>>> = BTreeMap::new();
    for hit in hits {
        let Some((folder, filename)) = split_path(&hit.path) else {
            continue;
        };
        folders
            .entry(folder)
            .or_default()
            .entry(filename)
            .or_default()
            .push(hit.line);
    }

    folders
        .into_iter()
        .map(|(folder, files)| {
            let info = GistInfo::load(dir, &folder);
            let files = files
                .into_iter()
                .map(|(filename, mut lines)| {
                    lines.sort_by_key(|l| l.line_number);
                    FileMatches { filename, lines }
                })
                .collect();
            GistMatches {
                folder,
                description: info.description,
                url: info.html_url,
                files,
            }
        })
        .collect()
}

/// Split a tool-reported path like `./my-notes-a1b2c3d4/notes.md` into the
/// gist folder and the file path within it.
fn split_path(path: &str) -> Option<(String, String)> {
    let mut parts = Path::new(path).components().filter_map(|c| match c {
        Component::Normal(part) => Some(part.to_string_lossy()),
        _ => None,
    });
    let folder = parts.next()?.into_owned();
    let filename = parts.collect::<Vec<_>>().join("/");
    if filename.is_empty() {
        None
    } else {
        Some((folder, filename))
    }
}

#[cfg(test)]
// Match ranges are data here; a one-element Vec of them is intended.
#[allow(clippy::single_range_in_vec_init)]
mod tests {
    use super::*;

    fn query(pattern: &str) -> Query {
        Query {
            pattern: pattern.to_string(),
            ignore_case: false,
            fixed_strings: false,
            word: false,
        }
    }

    fn installed(engine: Engine) -> bool {
        Command::new(engine.program())
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok()
    }

    /// A gists dir with two gists, a dotfile, and the files grass writes
    /// itself (which must never show up in results).
    fn fixture() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join(STATE_FILE), "needle").unwrap();

        let notes = root.join("my-notes-aaa11111");
        std::fs::create_dir(&notes).unwrap();
        std::fs::write(
            notes.join(META_FILE),
            r#"{"id":"aaa111","description":"My notes","html_url":"https://gist.github.com/aaa111","files":[{"filename":"needle"}]}"#,
        )
        .unwrap();
        std::fs::write(notes.join("notes.md"), "first\na needle here\nlast\n").unwrap();
        std::fs::write(notes.join(".bashrc"), "export NEEDLE=1\n").unwrap();

        let script = root.join("deploy-bbb22222");
        std::fs::create_dir(&script).unwrap();
        std::fs::write(script.join("deploy.sh"), "needle needle\n").unwrap();
        dir
    }

    fn check_engine(engine: Engine) {
        if !installed(engine) {
            eprintln!("skipping: {} not installed", engine.program());
            return;
        }
        let dir = fixture();
        let output = execute(engine, dir.path(), &query("needle")).unwrap();
        let hits = match engine {
            Engine::Ripgrep => parse_rg(&output.stdout),
            Engine::Grep => parse_grep(&output.stdout),
        };
        let gists = group(dir.path(), hits);

        assert_eq!(gists.len(), 2);
        let deploy = &gists[0];
        assert_eq!(deploy.folder, "deploy-bbb22222");
        assert_eq!(deploy.title(), "deploy-bbb22222");
        assert_eq!(deploy.url, None);
        assert_eq!(
            deploy.files,
            vec![FileMatches {
                filename: "deploy.sh".to_string(),
                lines: vec![LineMatch {
                    line_number: 1,
                    text: "needle needle".to_string(),
                    ranges: vec![0..6, 7..13],
                }],
            }]
        );

        // The metadata sidecar and the (case-mismatched) dotfile don't match.
        let notes = &gists[1];
        assert_eq!(notes.title(), "My notes");
        assert_eq!(notes.url.as_deref(), Some("https://gist.github.com/aaa111"));
        assert_eq!(notes.files.len(), 1);
        assert_eq!(notes.files[0].filename, "notes.md");
        assert_eq!(notes.files[0].lines[0].line_number, 2);
        assert_eq!(notes.files[0].lines[0].ranges, vec![2..8]);

        // Dotfiles are searched too.
        let mut q = query("needle");
        q.ignore_case = true;
        let output = execute(engine, dir.path(), &q).unwrap();
        let hits = match engine {
            Engine::Ripgrep => parse_rg(&output.stdout),
            Engine::Grep => parse_grep(&output.stdout),
        };
        assert!(hits.iter().any(|h| h.path.ends_with(".bashrc")));
        assert!(!hits.iter().any(|h| h.path.ends_with(META_FILE)));
        assert!(!hits.iter().any(|h| h.path.ends_with(STATE_FILE)));
    }

    #[test]
    fn searches_with_ripgrep() {
        check_engine(Engine::Ripgrep);
    }

    #[test]
    fn searches_with_grep() {
        check_engine(Engine::Grep);
    }

    #[test]
    fn missing_dir_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let err = run(&dir.path().join("nope"), &query("x")).unwrap_err();
        assert!(err.to_string().contains("no gists found"));
    }

    #[test]
    fn parses_rg_json() {
        let out = br#"{"type":"begin","data":{"path":{"text":"./a-1/x.rs"}}}
{"type":"match","data":{"path":{"text":"./a-1/x.rs"},"lines":{"text":"let tokio = tokio;\r\n"},"line_number":3,"absolute_offset":0,"submatches":[{"match":{"text":"tokio"},"start":4,"end":9},{"match":{"text":"tokio"},"start":12,"end":17}]}}
{"type":"end","data":{"path":{"text":"./a-1/x.rs"},"binary_offset":null,"stats":{}}}
{"type":"summary","data":{"elapsed_total":{"secs":0,"nanos":1,"human":"0s"},"stats":{}}}
"#;
        assert_eq!(
            parse_rg(out),
            vec![Hit {
                path: "./a-1/x.rs".to_string(),
                line: LineMatch {
                    line_number: 3,
                    text: "let tokio = tokio;".to_string(),
                    ranges: vec![4..9, 12..17],
                },
            }]
        );
    }

    #[test]
    fn parses_rg_non_utf8_lines_without_highlights() {
        // "caf\xe9 ok\n" in base64.
        let out = br#"{"type":"match","data":{"path":{"text":"g-1/f"},"lines":{"bytes":"Y2Fm6SBvawo="},"line_number":1,"submatches":[{"match":{"text":"ok"},"start":5,"end":7}]}}"#;
        let hits = parse_rg(out);
        assert_eq!(hits[0].line.text, "caf\u{fffd} ok");
        assert!(hits[0].line.ranges.is_empty());
    }

    #[test]
    fn parses_grep_color_output() {
        let out = b"./a-1/x.rs\x003:let \x1b[01;31mtokio\x1b[m = \x1b[01;31mtokio\x1b[m;\n\
                    ./b-2/y:z.txt\x0012:a:b \x1b[01;31m\x1b[Kc\x1b[m\x1b[K\n";
        assert_eq!(
            parse_grep(out),
            vec![
                Hit {
                    path: "./a-1/x.rs".to_string(),
                    line: LineMatch {
                        line_number: 3,
                        text: "let tokio = tokio;".to_string(),
                        ranges: vec![4..9, 12..17],
                    },
                },
                Hit {
                    path: "./b-2/y:z.txt".to_string(),
                    line: LineMatch {
                        line_number: 12,
                        text: "a:b c".to_string(),
                        ranges: vec![4..5],
                    },
                },
            ]
        );
    }

    #[test]
    fn strip_sgr_ignores_colored_prefix() {
        // A grep that colors filenames anyway: only the match range survives
        // once rebased onto the line text.
        let out = b"\x1b[35m./a-1/x\x1b[0m\x001:hi \x1b[1;31mthere\x1b[0m\n";
        let hits = parse_grep(out);
        assert_eq!(hits[0].path, "./a-1/x");
        assert_eq!(hits[0].line.text, "hi there");
        assert_eq!(hits[0].line.ranges, vec![3..8]);
    }

    #[test]
    fn strip_sgr_handles_unterminated_and_stray_escapes() {
        assert_eq!(strip_sgr("a\x1b[31mb"), ("ab".to_string(), vec![1..2]));
        assert_eq!(strip_sgr("a\x1bb"), ("a\x1bb".to_string(), vec![]));
    }

    #[test]
    fn base64_roundtrip() {
        assert_eq!(decode_base64("aGVsbG8="), b"hello");
        assert_eq!(decode_base64("aGVsbG8"), b"hello");
        assert_eq!(decode_base64("Y2Fm6Q=="), b"caf\xe9");
        assert_eq!(decode_base64(""), b"");
    }

    #[test]
    fn line_match_drops_bad_ranges() {
        let m = line_match(
            1,
            "héllo\n".to_string(),
            vec![Range { start: 3, end: 1 }, 2..3, 0..1, 4..99],
        );
        assert_eq!(m.text, "héllo");
        assert_eq!(m.ranges, vec![0..1, 4..6]);
    }

    #[test]
    fn split_path_finds_folder_and_file() {
        assert_eq!(
            split_path("./notes-a1/notes.md"),
            Some(("notes-a1".to_string(), "notes.md".to_string()))
        );
        assert_eq!(
            split_path("notes-a1/.bashrc"),
            Some(("notes-a1".to_string(), ".bashrc".to_string()))
        );
        assert_eq!(split_path("./stray.txt"), None);
    }

    #[test]
    fn title_falls_back_to_folder() {
        let mut g = GistMatches {
            folder: "deploy-a1".to_string(),
            description: Some("  ".to_string()),
            url: None,
            files: vec![],
        };
        assert_eq!(g.title(), "deploy-a1");
        g.description = Some("Deploy script".to_string());
        assert_eq!(g.title(), "Deploy script");
    }
}
