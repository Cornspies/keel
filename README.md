# keel

A self-hosted git server focused on being easy to run and fast to use.
One binary, one data directory, no external services.

**Features:**

- **Git hosting:** clone, fetch and push over HTTP (password or access token)
  and over a built-in SSH server.
- **Code browsing:** trees, files with syntax highlighting, history, commits
  with diffs, branches and tags, and downloading any of them as `.zip` or
  `.tar.gz`.
- **Verified commits:** commits signed with SSH or GPG keys people added show
  as verified (or say why not).
- **Changing files in the browser:** edit, create, rename, upload and delete
  files, committed to the branch or to a new one with a pull request.
- **Releases:** notes and files for a tag, with drafts and pre-releases.
- **Wikis:** Markdown pages edited on the web or cloned and pushed with git,
  with `[[links]]`, page history, a sidebar and a footer.
- **Topics, stars and Explore:** tag repositories with topics, star the ones
  you like, and find them by topic, name or description, or by popularity.
- **Activity feed** on the home page (repositories you watch) and on
  profiles.
- **Insights:** contributor and commit activity graphs.
- **Code search** across all repositories you can see, or within one on any
  branch; substrings or regular expressions.
- **Issues and pull requests:** shared numbering, Markdown with `#123` and
  `@name` links, labels, assignees, milestones, and filtering by all of them;
  reactions (👍 ❤️ 🚀 …); issue and pull request templates (GitHub's
  `.github/ISSUE_TEMPLATE` work as they are); search with GitHub's
  qualifiers (`is:open label:bug author:@me`), also across repositories.
- **Project boards:** issues, pull requests and notes as cards in columns,
  moved by dragging (or with each card's menu); closing an issue moves its
  card to Done.
- **Code review:** pull requests from branches or forks, review requests,
  comments on lines of the diff (with replies, marked outdated once the line
  changes), suggested changes that can be applied with one click, approvals
  and change requests, and merging with a merge commit, squash or rebase,
  with conflict detection. Draft pull requests can't be merged until they're
  marked ready for review; auto-merge merges once checks pass and reviews are
  in.
- **Branch protection:** require pull requests, a number of approvals and
  passing checks, and refuse force pushes and deletion, for branches by name
  or pattern.
- **Commit statuses:** CI reports results through the API (GitHub's status
  API); they show on commits, branches and pull requests.
- **Forks** that are created instantly and share disk space with the original.
- **Archiving:** archived repositories are read-only (no pushes, issues,
  comments or merges) and stay out of the way on Explore.
- **Templates:** mark a repository as a template, and start new ones with a
  copy of its files (without its history) and its labels.
- **Mirrors:** pull mirrors of repositories hosted elsewhere (GitHub,
  GitLab, …), kept in sync on a schedule, and push mirrors that get every
  change, both with their Git LFS files.
- **Git LFS** for large files, over HTTP and SSH, with file locking.
- **Notifications:** an inbox for issues and pull requests you opened, commented
  on, are assigned to or mentioned in, or whose repository you watch; with
  per-thread subscribe/unsubscribe, and optionally by email.
- **Webhooks** for repositories and whole organizations: pushes, issues,
  comments, pull requests, reviews, releases, wiki edits and stars, in
  GitHub's payload format and signed, with retries and a delivery log.
- **Email** (optional, via any SMTP server): notification emails, address
  verification and "forgot password".
- **People:** accounts with optional two-factor authentication (authenticator
  apps and recovery codes), organizations (owners and members),
  per-repository collaborators (read/write/admin), private repositories.
- **Importing** from GitHub, Gitea or Forgejo: the code with Git LFS files,
  issues and pull requests (numbers kept), comments and reviews, labels,
  milestones, releases with their files, and the wiki.
- **Profiles** with avatars (uploaded, or generated patterns) and pinned
  repositories, for people and organizations.
- **Renaming and transferring** repositories (to you or your organizations);
  old links, clones and remotes keep working.
- **Administration:** a page for site administrators with statistics,
  accounts (create, disable, delete, grant admin), all repositories, closing
  sign-ups and a notice on every page.
- **Light and dark themes** that follow the system, or that people pick with
  the switch at the bottom of every page.
- **JSON API** in the style of GitHub's, with personal access tokens.

CI is not built in: connect one with a webhook, and let it report back with
commit statuses.

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
repositories), `lfs/` (Git LFS files), the generated SSH host key, and
`search/` (the code search index, which is rebuilt automatically if deleted).

### Backups and monitoring

```sh
./keel backup --data-dir data /backups/keel-2026-10-24
```

copies the data directory while the server keeps running: the database as a
consistent snapshot, then every repository and wiki, LFS file, release file
and the SSH host key. The
copy is a data directory itself, so restoring is `keel serve --data-dir
/backups/keel-2026-10-24` (or copying it back). Archive it with `tar` or your
backup tool of choice. The database holds secrets (two-factor and webhook
secrets, mirror passwords), so the copy's directory is readable only by its
owner (`0700`, the database `0600`), like a new data directory; symlinks in
the data directory aren't followed.

`GET /healthz` answers `200 {"status":"ok",…}` while the server can use its
database and `503` otherwise, for load balancers and uptime monitors.

With `--metrics-token` (or `KEEL_METRICS_TOKEN`) set, `GET /metrics` serves
Prometheus metrics to requests with that bearer token: requests and their
durations by route, clones and pushes by transport, mirror runs, LFS traffic,
and totals (accounts, repositories, issues, queued webhooks and email, stored
LFS files). A scrape config:

```yaml
scrape_configs:
  - job_name: keel
    authorization: { credentials: "<the token>" }
    static_configs: [{ targets: ["git.example.com:3000"] }]
```

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
| `--closed-registration` | `KEEL_CLOSED_REGISTRATION` | open (can also be closed on the admin page) |
| `--site-name` | `KEEL_SITE_NAME` | `keel` |
| `--webhook-allow-private` | `KEEL_WEBHOOK_ALLOW_PRIVATE` | off: webhooks and mirrors can't reach loopback/private addresses |
| `--lfs-max-file-size` | `KEEL_LFS_MAX_FILE_SIZE` | `2048` (MiB); also limits release files |
| `--metrics-token` | `KEEL_METRICS_TOKEN` | no metrics |
| `--smtp-url` | `KEEL_SMTP_URL` | no email |
| `--mail-from` | `KEEL_MAIL_FROM` | `<site name> <noreply@<host of --base-url>>` |

To send email, point keel at an SMTP server; prefer the environment variable,
since it contains a password:

```sh
KEEL_SMTP_URL='smtps://user:password@smtp.example.com' ./keel serve              # TLS (port 465)
KEEL_SMTP_URL='smtp://user:password@smtp.example.com:587?tls=required' ./keel serve  # STARTTLS
KEEL_SMTP_URL='smtp://localhost:25' ./keel serve                                 # a local relay
```

keel checks the configuration at startup and logs whether it can reach the
server. Without email everything else works; only notification emails and
password resets are unavailable.

Behind a reverse proxy with TLS, set `--base-url https://git.example.com`:
clone URLs use it, and session cookies are marked `Secure`.

### JSON API

The API lives under `/api/v1` and follows GitHub's REST API where that's easy:
the same paths and field names, `page`/`per_page` with `Link` headers, and
`{"message": "…"}` errors. Authenticate with a personal access token (Settings
→ Access tokens); without one, public repositories can be read. Read-only
tokens can only read.

```sh
curl -H "Authorization: Bearer $TOKEN" http://localhost:3000/api/v1/user
curl -H "Authorization: Bearer $TOKEN" -d '{"title":"Crash on start","labels":["bug"]}' \
     http://localhost:3000/api/v1/repos/ada/engine/issues
curl -X PUT -H "Authorization: Bearer $TOKEN" -d '{"merge_method":"squash"}' \
     http://localhost:3000/api/v1/repos/ada/engine/pulls/7/merge
```

| Endpoint | |
| --- | --- |
| `GET /user`, `GET /user/repos`, `POST /user/repos` | you and your repositories; `mirror_url` (with `mirror_username`, `mirror_password`, `mirror_interval`) creates a mirror |
| `GET /users/{name}`, `GET /users/{name}/repos`, `POST /orgs/{org}/repos` | |
| `GET /user/keys`, `/user/gpg_keys`, `POST`, `GET`, `DELETE …/{id}` | your SSH keys (`title`, `key`) and GPG keys (`armored_public_key`) |
| `GET`, `PATCH`, `DELETE /repos/{owner}/{repo}` | `name` (renames), `description`, `private`, `is_template`, `archived` |
| `POST /repos/{owner}/{repo}/generate` | from a template: `owner`, `name`, `description`, `private`, `include_all_branches` |
| `GET`, `PUT /repos/{owner}/{repo}/topics` | `{"names": ["rust", "cli"]}` |
| `GET /search/repositories` | `q` (words, `topic:x` and `archived:false`), `sort` (`updated`, `stars`, `created`, `watchers`, `name`) |
| `GET /user/starred`, `GET /users/{name}/starred`, `GET /repos/{owner}/{repo}/stargazers` | |
| `GET`, `PUT`, `DELETE /user/starred/{owner}/{repo}` | `204` if starred (or done), `404` if not |
| `GET /search/issues` | `q` (as on the web), `sort` (`created`, `updated`, `comments`), `order` |
| `POST /repos/migrate` | like Gitea's: `clone_addr`, `auth_token`, `repo_owner`, `repo_name`, `private`, `issues`, `releases`, `wiki` |
| `GET /repos/{owner}/{repo}/import` | `status` (`queued`, `running`, `done`, `failed`), `progress`, `error` |
| `POST /repos/{owner}/{repo}/transfer` | `new_owner`: you or an organization you're in |
| `GET /repos/{owner}/{repo}/branches[/{branch}]`, `/tags` | with `protected` |
| `GET /repos/{owner}/{repo}/commits[/{sha}]` | `sha`, `path`; `commit.verification` says whether it's signed and verified |
| `POST /repos/{owner}/{repo}/statuses/{sha}` | `state` (`pending`, `success`, `failure`, `error`), `context`, `description`, `target_url` |
| `GET /repos/{owner}/{repo}/commits/{ref}/status`, `/statuses` | combined state, and each status |
| `GET /repos/{owner}/{repo}/contents/{path}` | `ref`; files base64-encoded |
| `GET /repos/{owner}/{repo}/labels`, `/milestones` | |
| `GET`, `POST /repos/{owner}/{repo}/issues` | `state`, `labels`, `assignee`, `milestone` |
| `GET`, `PATCH /repos/{owner}/{repo}/issues/{n}` | `title`, `body`, `state`, `labels`, `assignees`, `milestone` |
| `GET`, `POST /repos/{owner}/{repo}/issues/{n}/comments` | |
| `GET`, `POST /repos/{owner}/{repo}/issues/{n}/reactions`, `DELETE …/reactions/{id}` | `content`: `+1`, `-1`, `laugh`, `hooray`, `confused`, `heart`, `rocket`, `eyes` |
| `GET`, `POST /repos/{owner}/{repo}/issues/comments/{id}/reactions` (or `pulls/comments`), `DELETE …/reactions/{id}` | the same |
| `GET`, `POST /repos/{owner}/{repo}/pulls` | `title`, `body`, `head` (`branch` or `owner:branch`), `base`, `draft` |
| `GET /repos/{owner}/{repo}/pulls/{n}` | with `draft` and `auto_merge`; adds `approvals`, `required_approvals`, `merge_blocked_by` |
| `POST /repos/{owner}/{repo}/pulls/{n}/reviews` | `event`: `APPROVE`, `REQUEST_CHANGES` or `COMMENT` |
| `PUT /repos/{owner}/{repo}/pulls/{n}/merge` | `merge_method`: `merge`, `squash` or `rebase`; `delete_branch` |
| `GET /repos/{owner}/{repo}/projects[/{n}]` | boards; one board with its `columns` and their `cards` |
| `POST /repos/{owner}/{repo}/projects/{n}/cards`, `…/cards/{id}/moves` | `column_id` and `issue_number` or `note`; `column_id` and `position` |
| `GET`, `POST /repos/{owner}/{repo}/releases` | `tag_name`, `target_commitish`, `name`, `body`, `draft`, `prerelease` |
| `GET /repos/{owner}/{repo}/releases/latest`, `/releases/tags/{tag}` | |
| `GET`, `PATCH`, `DELETE /repos/{owner}/{repo}/releases/{id}` | `"draft": false` publishes |
| `GET`, `POST /repos/{owner}/{repo}/releases/{id}/assets` | `?name=<file>`, with the file as the body |
| `DELETE /repos/{owner}/{repo}/releases/assets/{id}` | |

Issue lists don't include pull requests (unlike GitHub's); use `/pulls`.
Bodies are JSON with or without a `Content-Type` header. Old names of
renamed or transferred repositories keep working.

A CI job reports its result like this:

```sh
curl -H "Authorization: Bearer $TOKEN" \
     -d '{"state":"success","context":"ci/build","target_url":"https://ci.example.com/builds/42"}' \
     http://localhost:3000/api/v1/repos/ada/engine/statuses/$GIT_COMMIT
```

## Layout

```
crates/
  keel        the binary (CLI, wiring)
  keel-core   config, SQLite schema + migrations, users, two-factor
              authentication, organizations, sessions, SSH keys, access
              tokens, repositories (forks, renames, transfers, mirrors),
              permissions, issues, labels, assignees, milestones, pull
              requests, reviews and suggestions, branch protection (and the
              pre-receive hook), commit statuses, notifications, webhook
              queue and payloads, push events, site administration, Git LFS
              storage, wikis, topics, stars, reactions, issue templates,
              commits from the browser, issue search, avatars, pins,
              imports, drafts and auto-merge, templates, archiving, commit
              counts for graphs, verifying commit signatures (SSH and
              OpenPGP), project boards, backups
  keel-git    reading repositories (libgit2 + git CLI), diffs, merging and
              rebasing,
              repo creation and maintenance, upload-pack/receive-pack commands
  keel-search trigram code search index (tantivy)
  keel-hooks  webhook delivery worker (reqwest)
  keel-mail   SMTP sender for the mail outbox (lettre)
  keel-mirror pull and push mirror worker, its Git LFS client, and
              importing from GitHub and Gitea
  keel-web    axum router: server-rendered pages (askama templates), the
              JSON API, smart HTTP git protocol, the Git LFS server, background
              cache warming and search indexing, assets (compiled into the
              binary)
  keel-ssh    built-in SSH server (russh): public-key auth, git over SSH,
              git-lfs-authenticate
scripts/
  smoke-test.sh   end-to-end test with real git, ssh and curl
```

## Design notes

**Pages are rendered on the server** with compile-time-checked askama
templates. There is no frontend build. The ~200 lines of JavaScript
only add conveniences (copy buttons, late-loaded commit info, opening the
line-comment form in place, starting a suggested change, reactions and the
theme switch without a reload, dragging cards on project boards), so every
page works without them.

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

Other branches aren't indexed. Searching one diffs it against the indexed
commit: unchanged files come from the index, and the files that differ (a few,
for a typical branch) are scanned directly. So results are exact for any
branch, and for the default branch right after a push. Regular expressions
(`fn \w+\(`, or a query written `/like this/`) use the trigrams of the
literal text every match must contain; one without any (like `\d+`) can only
search within a repository, by scanning it. The `regex` crate guarantees
linear-time matching, so no pattern can stall the server. Queries are at most
256 characters.

**Forks** are `git clone --bare --local` copies whose objects are hardlinked,
so forking git.git takes about 40 ms and almost no disk space, and a fork
never depends on its parent (deleting either is safe). Forks of private
repositories stay private.

**Pull requests** come from a branch of the same repository, or of a fork of
it (or its parent). Each one's head is kept as `refs/pull/<n>/head` in the base
repository (fetchable, protected from pushes), so it stays viewable after the
branch is deleted. For a fork, its commits are fetched into that ref when the
pull request is opened and whenever the fork's branch moves (about 40 ms on
git.git); the compare page before that reads the fork's objects through a
temporary libgit2 alternate instead of copying anything. Merging happens
without a working tree: `git merge-tree --write-tree`, `git commit-tree`, then
`git update-ref` with the expected old value, so a concurrent push is never
overwritten. Pushing a branch's commits into the base by hand marks the pull
request as merged. Commits of a private head repository (other than the base
itself) are only fetched into a private base, and only while the pull
request's author can still read the head; otherwise the head counts as
deleted. So someone who lost access to a private parent doesn't keep getting
its commits through a pull request into their fork, and making a fork private
keeps its new commits out of a public parent.

**Line comments** record the file, side, line number and the blob of that side
they were made on. While that side of the file is unchanged in the current
diff, the line still says the same thing, so the comment is shown there;
otherwise it's shown as outdated in the conversation, with the diff lines it
was made on (stored with it).

**Pull mirrors** are fetched by a background worker (`keel-mirror`) with
`git fetch --prune` (so branches and tags deleted upstream go too), every
1–24 hours or on "Sync now", with a 10-minute limit. Changes are handled like
a push: webhooks, search indexing and caches follow. Mirrors can't be pushed
to and don't take pull requests; "Stop mirroring" turns one into an ordinary
repository. **Push mirrors** get branches and tags pushed (`git push --prune`)
whenever they change, or on "Push now"; failures are retried after 1 min,
5 min, 30 min, 2 h, then every 6 h.

Both carry Git LFS files: keel is an LFS client too. A pull mirror downloads
the files that new commits point to (content checked, and ones that fail are
retried on the next sync); a push mirror uploads the files the remote's
commits don't have yet, before pushing the commits.

Credentials are stored apart from the URL, never shown again, and passed to
git through its environment (not its command line). Like webhooks, mirrors
can't reach internal addresses unless `--webhook-allow-private` is set: for
git, keel resolves the host, checks every address, then pins git to the
checked one (`http.curloptResolve`), forbids redirects and allows only
http(s), so a rebinding DNS name or a redirect can't get around it; LFS
transfers check the addresses actually connected to, including after the
few redirects storage services use.

**Git LFS** lives where clients expect it, at `<repo>.git/info/lfs`, with the
batch API and basic transfers. Files are stored once by SHA-256 (under
`data/lfs`), and uploads are hashed while they stream to disk and refused if
the content doesn't match. Each repository has its own list of the files it
may serve, so knowing a file's hash isn't enough to read it from another
repository. Forks start with their parent's list, and merging a pull request
adds the files its changes point to. Unused files are deleted when
repositories are. Transfers are authorized with short-lived signed tokens,
which over SSH come from `git-lfs-authenticate`. File locking follows the
LFS locking API: `git lfs lock` takes a file, and git-lfs then refuses to push
others' changes to it (it checks with the server before every push); owners
unlock their files, repository admins can break anyone's lock (`--force`, or
in the settings). The web UI shows LFS files with their real size and who
locked them, and "Raw" serves their content.

**Downloads** (`/<owner>/<repo>/archive/<branch, tag or commit>.zip` or
`.tar.gz`) are streamed straight from `git archive`, with every file under
`<repo>-<name>/`. At most four are built at once, so a burst of downloads
can't take over the CPU. LFS files are included as pointers.

**Releases** belong to a tag. A draft is only visible to people with write
access; publishing it creates its tag (at the chosen branch) if the tag
doesn't exist yet. Files are streamed to `data/releases` and are always
served as downloads (`application/octet-stream`, attachment), never shown
inline. Deleting a release deletes its files but keeps the tag. Webhooks get
a `release` event.

**The activity feed** is a table of events recorded where notifications and
webhooks already come from: pushes (with their newest commits), new and
deleted branches and tags, issues and pull requests opened, closed and
merged, comments and reviews, releases, wiki edits, new repositories and
forks. Mirror
syncs aren't included. The home page shows the repositories you watch (and
your own actions), profiles what someone did, and both only include
repositories the viewer can read. Events are kept for a year.

**Wikis** work like GitHub's: each is a second git repository
(`data/repos/<id>.wiki.git`, cloned as `<repo>.wiki.git` over HTTP and SSH)
with one Markdown file per page, so `Getting-Started.md` is the page "Getting
Started" and existing GitHub wikis can be pushed as they are. It's created
with the first page or push. Pages are found by name in any directory,
ignoring case; `Home` is the front page and `_Sidebar` and `_Footer` are shown
around every page. `[[Page]]` and `[[text|Page]]` link to pages, relative
images come from the wiki. Anyone who can read the repository can read the
wiki and people with write access can change it (branch protection doesn't
apply). Web edits are commits by the editor; one based on an older version of
the page is refused instead of overwriting someone's changes. Pushes and web
edits show in the activity feed and send GitHub's `gollum` webhook.

**Topics** are lowercase words with hyphens, at most 20 per repository. The
Explore page lists the repositories the viewer can see, filtered by topic or
words in the name or description, with the most used topics beside them.
**Stars** are counted in `repositories.star_count`, kept up to date by
triggers (also when an account is deleted), so lists sort by them without
counting.

**Changing files in the browser** makes commits like a push would, by the
person doing it: they show in the feed, run webhooks and update pull
requests. The editor remembers the commit it was opened at; if someone
changed the same file since, the commit is refused (changes to other files
are fine). Protected branches that require pull requests only offer
committing to a new branch, which leads straight to opening the pull
request; a "new branch" named like the protected one is refused. A new branch
must not exist yet, but may match a protected pattern, as with pushes.
Line endings are kept (browsers send `\r\n`), as is the executable
bit when editing or renaming. Uploads are up to 25 MiB per file and 100 MiB
in all and go into git directly; push big files with Git LFS instead.

**Templates** are read from the default branch where GitHub and Gitea look
for them: `.keel/`, `.github/`, `.gitea/`, the top level or `docs/`, with
names in any case. Several issue templates (`ISSUE_TEMPLATE/*.md`, with
`name`, `about`, `title`, `labels` and `assignees` front matter) are offered
to choose from; `blank_issues_enabled: false` in `ISSUE_TEMPLATE/config.yml`
hides "Open a blank issue". Labels and assignees from a template are applied
when the issue is opened, even by people who can't set them themselves.
`PULL_REQUEST_TEMPLATE.md` fills in new pull requests.

**Searching issues** parses GitHub's qualifiers (`is:`, `label:`/`-label:`,
`author:`, `assignee:`, `review-requested:`, `involves:`, `milestone:`, `no:`,
`repo:`, `user:`, `sort:`; `@me` is you; other words are looked for in titles
and descriptions) into one static SQL query that only ever sees repositories
the viewer can read. `/issues` and `/pulls` show what involves you across
repositories.

**Imports** run in the background, one at a time, through the same guarded
network access as mirrors (no private addresses unless allowed; git is
pinned to the checked address). git brings branches, tags and pull requests'
heads (`refs/pull/<n>/head`, where keel keeps them too), then Git LFS files;
the API brings the rest. Imported issues, comments and releases keep their
dates and are shown with their author's name, linked to their profile on the
source, never to a local account with the same name; nothing notifies
anyone. Line comments on pull requests become comments quoting the line.
The access token is only sent to the source's own scheme, host and port (not
to other URLs it names), and is forgotten when the import ends; an import
interrupted by a restart is marked as failed.

**Avatars** are served from `/avatars/<name>` with an ETag: an uploaded PNG,
JPEG, GIF or WebP (checked by its first bytes, at most 1 MiB, kept in
`data/avatars` and in backups), or else a pattern made from the name. There
is no Gravatar: it would tell another service who uses this server.

**Branch protection** applies to everyone, administrators included. A rule
for an exact branch name wins over patterns; among patterns the longest
matching one applies. Merging checks the rule's required approvals: only
approvals from people with write access (other than the author) count, any of
them requesting changes blocks merging, and by default reviews only count for
the commit they were given on. Pushes are checked by a `pre-receive` hook:
keel points `git receive-pack` at its own hook directory (`data/hooks`) and
passes the repository's rules in the environment; the hook (`keel hook
pre-receive`) refuses updates to protected branches that aren't allowed, with
a message for the person pushing. Repositories without rules don't run it.
The data directory is made absolute when the server starts, since git looks
for a relative hook directory inside each repository and would silently skip
the hook. "Allow force pushes" also allows deleting the branch, except when the
rule requires pull requests: deleting it and pushing it anew would replace it
without one. Creating branches is always allowed, also ones matching a rule.
Patterns match segment by segment in linear time, on any characters.

Pushes can't create or change `refs/replace/*` (nor fetch them), and keel runs
every git command with replace refs off: libgit2, which shows diffs and files,
ignores them, so they could otherwise make merges, downloads or the
fast-forward check see different commits than people reviewed.

**Commit statuses** keep the latest state per context and commit (up to 100
contexts per commit). A commit's overall state is failure if any status
failed, pending if any is still running, and success otherwise. A branch rule
can list required contexts: merging waits until each has succeeded on the pull
request's latest commit; ones that haven't reported are shown as expected.

**Renames and transfers** only change database rows, since repositories are
stored by id on disk. The old owner and name are kept as a redirect to the
repository's id, so a chain of renames still leads to it. Browsers get
redirected; git (HTTP and SSH) and the API just serve the repository. A new
repository with the old name takes it over. Private repositories don't
redirect people who can't see them, so new names don't leak. Repositories can
be transferred to yourself and to organizations you're in, not to other
people.

**Suggested changes** are ```` ```suggestion ```` blocks in line comments,
shown as a diff of the commented line. Applying one commits the edit to the
pull request's branch without a working tree (a new blob, tree and commit
with libgit2, then a compare-and-swap ref update), as the person applying it,
if they can push there and the file hasn't changed since.

**Auto-merge** is turned on by someone with write access, and merges as
them, the way they chose, once nothing blocks it: it's checked again when a
status is reported, a review is given or a branch moves (in the background,
one merge at a time). If they lose write access it turns itself off; marking
the pull request a draft turns it off too. Like GitHub, it's only offered
while branch rules block merging; otherwise there's the merge button.

**Templates** are copied without history: the files of the template's
default branch (or every branch) are packed with libgit2 straight from the
template into the new repository, and each branch gets a single "Initial
commit" by whoever generated it. Labels and Git LFS files come along.

**Graphs** count the default branch's commits (without merges, names and
emails through `.mailmap`) per day and author with one `git log`, and keep
the counts in memory; after a push only the new commits are counted, unless
the branch was rewritten. Commits belong to an account by its verified
email (or any email, if the server can't send mail to verify them). The
charts are SVG drawn on the server, with a tooltip per bar, so they need no
JavaScript.

**The theme** is a `data-theme` attribute on the page, which the stylesheet's
color tokens (and the syntax highlighting) follow; without it the system
setting decides. The choice is kept in a cookie, so it works signed out, and
with the account, so it follows people to other devices. The script applies
it in place; without the script the form reloads the page.

**Rebase and merge** replays the pull request's commits one by one onto the
base branch (`git merge-tree --merge-base` for each, without a working tree),
keeping their authors, dates and messages, with the merger as committer. If
the branch already builds on the base, its commits are kept as they are,
signatures included. A commit that doesn't apply, or a merge commit in the
branch, stops it with a message saying which; the base is only moved
(compare-and-swap) once every commit is in.

**Archiving** caps everyone's access to an archived repository at read: the
check every push, merge and edit goes through. Issues, comments and
reactions, which readers can otherwise add, are refused where they're
written. Settings stay open to admins, so they can unarchive it; pull mirrors
stop syncing.

**Verified commits:** a commit's `gpgsig` header is checked when it's shown,
in pure Rust (SSH signatures with `ssh-key`, OpenPGP with `pgp`), against
the SSH and GPG keys people added. A commit is verified when the signature
is good, made by someone's key (and before it expired), and its committer
email is that person's verified email (and, for GPG, one of the key's). SSH
keys sign as well as authenticate. Uploaded GPG keys must carry valid
self-signatures, so nobody can attach someone else's address; subkeys are
looked up by their key IDs, but only subkeys that can sign and aren't revoked
or expired are recorded, and a subkey's expiry is checked against the
signature's time. Keys of disabled accounts don't verify anything.
Whether a signature matches a key can't change, so that result is kept in
memory (the cryptography is the slow part: about 0.4 ms for an RSA-3072
signature); who owns the key, and their email, are looked up every time.

**Project boards** belong to a repository and hold its issues, pull requests
and notes. Cards are moved with forms (each card's menu), or dragged; the
script posts the move and then reloads the board's markup. A column can be
the done column: closing or merging an issue moves its cards there, and
reopening it moves them back to the first column.

**Labels** come from a fixed palette of ten colors, so they read well in light
and dark mode without inline styles (which the Content Security Policy
forbids). New repositories start with six common labels.

**Notifications and webhooks** come from one place: each change to an issue
or pull request, and each push, is recorded once in `keel-core`, which updates
the inboxes of the people following it and queues a webhook delivery (with the
finished payload) for every hook that wants the event. Deliveries live in
SQLite, so they survive restarts; a background worker sends them with a 10 s
timeout and retries failures after 30 s, 2 min, 10 min and 1 h. The delivery
log keeps the last 100 per hook, with payload and response, and any of them
can be redelivered. Push payloads list what each ref update added: the
refs are snapshotted before `git receive-pack` runs and compared afterwards.

**Email** goes through an outbox table in SQLite, like webhooks, and a
background sender (retries after 1 min, 5 min, 30 min and 2 h; addresses the
server rejects permanently aren't retried). Notification emails are plain
text, quote the comment, and carry `References` headers per issue so mail
clients thread them. They only go to verified addresses (verified by a link
sent at sign-up or after changing the address, or by a password reset), so
nobody can sign someone else's address up for mail. By default people get
emails about threads they take part in, not about everything in watched
repositories; each person can change that.

**Organization webhooks** get the events of all of the organization's
repositories, in addition to each repository's own hooks.

Webhooks don't follow redirects or use proxies. Unless
`--webhook-allow-private` is set, they can't reach loopback, private, link-local
or other internal addresses: names are resolved by keel and checked on the
addresses actually connected to, so a hostname pointing at an internal address
doesn't get around it.

**Permissions:** a user's access to a repository is the highest of: admin for
the owner, organization owners and site admins; write for organization
members; the collaborator role; read for anyone if the repository is public.
Archived repositories cap it at read.

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
| Contributors graph (first view; after a push only new commits are counted) | ~7 ms (~375 ms) |
| Commit activity graph | ~2 ms |
| Commit list of 50 signed commits, all verified (first view, RSA-3072 GPG) | ~3.5 ms (~20 ms) |

## Security

- Passwords: Argon2id. Session tokens are 256-bit random values, stored only
  as SHA-256 hashes, sent in `HttpOnly; SameSite=Lax` cookies.
- After ten wrong passwords for an account (by name or email) within 15
  minutes, signing in with its password (on the web and over git HTTP) is
  refused for the rest of those 15 minutes. The count is kept in memory.
- Changing the email address, adding SSH or GPG keys and creating access
  tokens ask for the password, so a stolen session can't be turned into
  lasting access.
- State-changing requests must come from the same origin (CSRF); behind
  `https://`, an origin on plain HTTP doesn't count.
- Redirects after signing in only go to local paths, and never to anything
  with control characters, spaces or backslashes, which browsers could turn
  into another host.
- Password reset and verification links are single-use, expire (1 hour and
  7 days), are stored only as SHA-256 hashes and are sent at most once a
  minute per account. Asking for a reset says the same thing whether or not an
  account has that address. A reset signs out all sessions.
- Webhook deliveries are signed with HMAC-SHA256 when a secret is set
  (`X-Hub-Signature-256: sha256=…`), and can't target internal addresses by
  default (see above).
- Personal access tokens are shown once and stored hashed; they can be
  read-only, expire, and be revoked individually. Changing a password signs
  out all other sessions.
- Strict Content Security Policy. Raw files are served with `sandbox` and
  never as `text/html`, so user content can't run scripts on keel's origin.
- Private repositories return 404 (web) or 401 (git) to anyone who can't see
  them, so their names don't leak. That includes fork lists and the
  repositories offered on the compare page.
- Only `git-upload-pack` and `git-receive-pack` can be run over SSH, once
  per channel, with at most ten channels per connection; git stops when its
  channel or connection closes. Repositories never run their own hooks (only
  keel's branch protection hook), and `GIT_PROTOCOL` values are validated
  before being passed to git.
- Gzip-compressed git requests are decompressed as they stream into git, and
  refused (`413`) past 64 MiB.
- The API only accepts access tokens, never passwords or cookies, so it can't
  be used cross-site or to guess passwords.
- Administrators can disable accounts: sessions end at once, and passwords,
  tokens and SSH keys stop working.
- Mirrors only fetch from and push to http(s) URLs at public addresses by
  default (see above). `/metrics` is off unless a token is set, and compares
  it in constant time. Git LFS tokens are HMAC-signed, scoped to one repository and one
  operation, and expire after an hour.
- Two-factor authentication uses TOTP (RFC 6238; any authenticator app).
  Each code works once, five wrong codes end a sign-in attempt, and ten in 15
  minutes lock the second step for a while (also with parallel requests, and
  for expired attempts). Changing the password ends pending second steps. Ten single-use recovery codes are
  stored hashed. With it on, git over HTTP no longer accepts the password
  (use a token or SSH key), and a password reset still asks for a code.
  Administrators can turn it off for someone who lost their phone and codes.

## Development

```sh
cargo test --workspace
cargo clippy --workspace --all-targets
cargo build && scripts/smoke-test.sh      # ~640 end-to-end checks (needs git, ssh, curl, python3, openssl, sqlite3; git-lfs optional)
RUST_LOG=debug cargo run -- serve --data-dir /tmp/keel-dev
```

Database changes go in new files under `crates/keel-core/migrations/`; they
run automatically at startup.

## Ideas

- Sign the commits keel makes (merges, squashes, rebases, edits in the
  browser) with the server's key, so they show as verified too; and a branch
  rule that requires verified commits.
- Organization boards with issues from several repositories, and `project:`
  in issue search.
- Board changes in issue timelines and in webhooks, and webhooks for
  archiving.
- A container image registry (the OCI distribution API).
