# keel

A self-hosted git server focused on being easy to run and fast to use.
One binary, one data directory, no external services.

**Features:**

- **Git hosting:** clone, fetch and push over HTTP (password or access token)
  and over a built-in SSH server.
- **Code browsing:** trees, files with syntax highlighting, history, commits
  with diffs, branches and tags.
- **Code search** across all repositories you can see, or within one.
- **Issues and pull requests:** shared numbering, Markdown with `#123` and
  `@name` links, and merging with a merge commit or squash, with conflict
  detection.
- **People:** accounts, organizations (owners and members), per-repository
  collaborators (read/write/admin), private repositories.

CI and webhooks are not built in.

## Quick start

Requirements: a Rust toolchain (1.91+) to build, and `git` (2.38+, for
`merge-tree --write-tree`) on the `PATH` at runtime.

```sh
cargo build --release
./target/release/keel serve
```

Open http://localhost:3000 and create an account. The first account becomes
the administrator. Then:

```sh
git push http://localhost:3000/<you>/<repo>.git main       # HTTP: username + password or access token
git push ssh://git@localhost:2222/<you>/<repo>.git main    # SSH: add your key under Settings
```

All state lives in `./data`: `keel.db` (SQLite), `repos/<id>.git` (bare
repositories), the generated SSH host key, and `search/` (the code search
index, which is rebuilt automatically if deleted). Back up that directory and
you have backed up everything.

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
  keel-core   config, SQLite schema + migrations, users, organizations,
              sessions, SSH keys, access tokens, repositories, permissions,
              issues, pull requests, push events
  keel-git    reading repositories (libgit2 + git CLI), diffs, merging,
              repo creation and maintenance, upload-pack/receive-pack commands
  keel-search trigram code search index (tantivy)
  keel-web    axum router: server-rendered pages (askama templates), smart
              HTTP git protocol, background cache warming and search indexing,
              assets (compiled into the binary)
  keel-ssh    built-in SSH server (russh): public-key auth, git over SSH
scripts/
  smoke-test.sh   end-to-end test with real git, ssh and curl
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

Results are stored in SQLite, so they survive restarts. After a push the
newest stored result for an ancestor commit is updated by walking only the new
commits (on git.git: 10 ms instead of 3 s), and the root directory of the
default branch is warmed in the background, so the first visitor after a push
gets a complete page.

**Code search** uses a trigram index of each repository's default branch:
the index narrows a query down to files containing all of its three-character
substrings, then those files are scanned to confirm matches and build
snippets. Any substring matches, including punctuation (`die(_(`), case
insensitively. After a push only changed files are reindexed. On git.git: the
initial index takes 1.7 s (11 MB), a push reindexes in about 25 ms, and queries
take 2–170 ms.

**Pull requests** are branch-to-branch within a repository. Each one's head is
kept as `refs/pull/<n>/head` (fetchable, protected from pushes), so it stays
viewable after the branch is deleted. Merging happens without a working tree:
`git merge-tree --write-tree`, `git commit-tree`, then `git update-ref` with the
expected old value, so a concurrent push is never overwritten. Pushing a
branch's commits into the base by hand marks the pull request as merged.

**Permissions:** a user's access to a repository is the highest of: admin for
the owner, organization owners and site admins; write for organization
members; the collaborator role; read for anyone if the repository is public.

**Background work** listens to push events from `keel-core` (after repository
maintenance): cache warming, search indexing and pull request updates.

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
- Personal access tokens are shown once and stored hashed; they can be
  read-only, expire, and be revoked individually. Changing a password signs
  out all other sessions.
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
cargo build && scripts/smoke-test.sh      # ~100 end-to-end checks against a real server
RUST_LOG=debug cargo run -- serve --data-dir /tmp/keel-dev
```

Database changes go in new files under `crates/keel-core/migrations/`; they
run automatically at startup.

## Ideas

- Pull requests from forks, inline review comments, labels and assignees.
- Webhooks and notifications (the push events are already there).
- An admin page (users, instance settings) and a JSON API (tokens are ready
  for it).
- Searching other branches, and regular-expression search.
