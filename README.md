# grass

`grass` downloads **all** of your GitHub gists to a local directory and keeps
them in sync. Re-running it only downloads gists that are new or that changed
remotely — unchanged gists are left untouched.

## Features

- Downloads every gist you own (public **and** secret).
- True sync: new gists are added and changed gists are refreshed on each run.
- Human-readable folders: `<description-slug>-<short-id>`.
- Per-gist `.grass-meta.json` sidecar with id, description, URL, timestamps and
  file details.
- Concurrent downloads with a progress bar.
- `--prune` to remove local copies of gists deleted on GitHub.
- `--dry-run` to preview changes without writing anything.
- No `git` required — files are fetched via the GitHub API.

## Install

```sh
cargo build --release
# binary at ./target/release/grass
```

## Authentication

`grass` needs a GitHub token with the `gist` scope. It looks for one in this
order:

1. `--token <TOKEN>`
2. `GITHUB_TOKEN` environment variable
3. `GH_TOKEN` environment variable
4. `gh auth token` (the [GitHub CLI](https://cli.github.com/), if installed)

The simplest path is to `gh auth login` once and let `grass` pick up the token
automatically.

## Usage

```sh
# Sync all your gists into ./gists
grass

# Choose an output directory
grass --output ~/gists

# Preview what would happen
grass --dry-run

# Remove local folders for gists that no longer exist on GitHub
grass --prune

# Re-download everything, ignoring saved state
grass --force

# Show each gist as it is processed
grass --verbose
```

### Options

| Flag | Description | Default |
| --- | --- | --- |
| `-o, --output <DIR>` | Directory to store gists in | `./gists` |
| `--token <TOKEN>` | GitHub token (overrides env vars and `gh`) | — |
| `--prune` | Delete local folders for gists deleted on GitHub | off |
| `--dry-run` | Show what would happen without writing | off |
| `--force` | Re-download every gist, ignoring saved state | off |
| `--concurrency <N>` | Max gists downloaded concurrently (1–64) | `8` |
| `-v, --verbose` | Print per-gist actions | off |
| `-q, --quiet` | Suppress all output except errors | off |

## Storage layout

```
gists/
  .grass-state.json                 # sync state (do not edit)
  my-notes-a1b2c3d4/
    .grass-meta.json                # gist metadata
    notes.md
  deploy-script-f2eeaeee/
    .grass-meta.json
    deploy.sh
```

## How sync works

`grass` records each gist's `updated_at` timestamp in `.grass-state.json`. On
each run it lists your gists and compares timestamps:

- **New** gist (unknown id) → downloaded.
- **Changed** gist (`updated_at` differs, or `--force`) → re-downloaded; files
  removed from the gist are deleted locally, and the folder is renamed if the
  description changed.
- **Unchanged** gist → skipped.

Deletions are non-destructive unless you pass `--prune`.

## Development

```sh
cargo fmt
cargo clippy --all-targets -- -D warnings
cargo test
```

## License

MIT
