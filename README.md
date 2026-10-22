# keel

A self-hosted git server focused on being easy to run and fast to use.
One binary, one data directory, no external services.

**v1 scope:** accounts, SSH keys, public/private repositories, clone/fetch/push
over HTTP and SSH, and browsing code (trees, files, history, commits with
diffs, branches and tags). Issues, pull requests and CI are out of scope for now.

## Quick start

Requirements: a Rust toolchain (1.90+) to build, and `git` (2.30+) on the
`PATH` at runtime.

```sh
cargo build --release
./target/release/keel serve
```

Open http://localhost:3000 and create an account. The first account becomes
the administrator. Then:

```sh
git push http://localhost:3000/<you>/<repo>.git main       # HTTP (username + password)
git push ssh://git@localhost:2222/<you>/<repo>.git main    # SSH (add your key under Settings)
```

All state lives in `./data`: `keel.db` (SQLite), `repos/<id>.git` (bare
repositories) and the generated SSH host key. Back up that directory and you
have backed up everything.

### Configuration

Every flag can also be set as an environment variable.

| Flag | Env | Default |
| --- | --- | --- |
| `--data-dir` | `KEEL_DATA_DIR` | `data` |
| `--http-addr` | `KEEL_HTTP_ADDR` | `0.0.0.0:3000` |
| `--base-url` | `KEEL_BASE_URL` | `http://localhost:<http port>` |
| `--ssh-addr` | `KEEL_SSH_ADDR` | `0.0.0.0:2222` |
| `--no-ssh` | `KEEL_NO_SSH` | SSH enabled |
| `--ssh-host` / `--ssh-port` | `KEEL_SSH_HOST` / `KEEL_SSH_PORT` | host of `--base-url` / port of `--ssh-addr` |
| `--closed-registration` | `KEEL_CLOSED_REGISTRATION` | open |
| `--site-name` | `KEEL_SITE_NAME` | `keel` |

Behind a reverse proxy with TLS, set `--base-url https://git.example.com`:
clone URLs use it, and session cookies are marked `Secure`.

## Layout

```
crates/
  keel        the binary (CLI, wiring)
  keel-core   config, SQLite schema + migrations, users, sessions, SSH keys,
              repositories, permissions
  keel-git    reading repositories (libgit2 + git CLI), diffs, repo creation
              and maintenance, building the upload-pack/receive-pack commands
  keel-web    axum router: server-rendered pages (askama templates), smart
              HTTP git protocol, assets (compiled into the binary)
  keel-ssh    built-in SSH server (russh): public-key auth, git over SSH
```

## Design notes

**Pages are rendered on the server** with compile-time-checked askama
templates. There is no frontend build. The ~50 lines of JavaScript
only add conveniences (copy buttons, late-loaded commit info), so every page
works without them.

**git itself serves clones and pushes.** keel authenticates and
authorizes, then streams request and response bodies straight into and out of
`git upload-pack` / `git receive-pack`. Pushes and clones of any size use
constant memory (pushing all of git.git's 322 MB used about 56 MB server
memory). This also brings protocol v2, partial clone (`--filter`) and
bitmaps for free.

**Reads use libgit2, except where git is faster.** Trees, blobs, diffs and
plain history use libgit2. Path-limited history ("History" for a file or
directory) uses `git log`, because git can use the changed-path Bloom filters
keel writes into each repository's commit-graph after every push. On git.git
that is about 50 ms instead of 5 s.

**Last commit per file** (the column in tree views) is one streamed
`git log --name-only` walk per directory that stops once every entry is
resolved. Directories, and entries whose change arrived through a merge, are
confirmed with a Bloom-filtered `git log -1 -- <path>` so the result matches
git's history simplification. If the result isn't known within a small budget
(500 commits), the page is sent immediately and the browser fetches the
column from `/<owner>/<repo>/last-commits/...`.

**Caches are keyed by git object ids**, so they never need invalidation, only
a size bound: last commits per (commit, directory), highlighted files per
blob, rendered Markdown per blob.

**Rendering work is bounded:** syntax highlighting stops at 256 KiB, file
display at 2 MiB, and diffs at 300 files / 10,000 lines.

### Measured on git.git (82k commits), release build, warm OS cache

| Page | Time |
| --- | --- |
| Tree (cached last commits) | ~20 ms |
| Tree (first view; column loads afterwards) | ~60 ms + 0.1–3 s async |
| File view, highlighted (7,860 lines; first view) | ~30 ms (~350 ms) |
| Commit list | ~20 ms |
| File history | ~50 ms |
| Commit with diff | ~5 ms |

## Security

- Passwords: Argon2id. Session tokens are 256-bit random values, stored only
  as SHA-256 hashes, sent in `HttpOnly; SameSite=Lax` cookies.
- State-changing requests must come from the same origin (CSRF).
- Strict Content Security Policy. Raw files are served with `sandbox` and
  never as `text/html`, so user content can't run scripts on keel's origin.
- Private repositories return 404 (web) or 401 (git) to anyone who can't see
  them, so their names don't leak.
- Only `git-upload-pack` and `git-receive-pack` can be run over SSH.
  Repositories never run hooks, and `GIT_PROTOCOL` values are validated before
  being passed to git.

## Development

```sh
cargo test --workspace
cargo clippy --workspace --all-targets
RUST_LOG=debug cargo run -- serve --data-dir /tmp/keel-dev
```

## Ideas for after v1

- Persist the last-commit cache and update it incrementally after a push
  (walk only the new commits).
- Personal access tokens for HTTP (instead of passwords), collaborators and
  organizations.
- Code search (tantivy), issues and pull requests.
- PostgreSQL as an alternative to SQLite.
