# keel

A self-hosted git server focused on being easy to run and fast to use.
One binary, one data directory, no external services.

**Features:**

- **Git hosting:** clone, fetch and push over HTTP (password or access token)
  and over a built-in SSH server. Deploy keys give servers and CI SSH access
  to one repository, read-only or with pushing.
- **Repository front page:** the files and README, beside a sidebar with
  the description, topics and license, the latest release, the languages
  used (by file size, as on GitHub: dependencies, minified files,
  documentation and data files don't count) and the top contributors.
  Watching and starring update in place, without reloading the page.
- **Code browsing:** trees, files with syntax highlighting, history, commits
  with diffs, branches and tags, and downloading any of them as `.zip` or
  `.tar.gz`. Files use the whole width, with the file tree beside them and a
  bar to switch between preview, code and blame, and to get the raw file,
  copy it, download it, edit or delete it. Authors in the history show their
  commits; those with an account link to their profile. Blame shows the commit that last changed each line, links to the
  blame before it, and skips the commits listed in `.git-blame-ignore-revs`
  (as on GitHub).
- **Verified commits:** commits signed with SSH or GPG keys people added show
  as verified (or say why not), and so do the commits keel makes itself
  (merges, squashes, rebases, edits in the browser), signed with its own key.
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
  card to Done. Boards of a repository, or of an organization with issues
  from all its repositories; changes show in the issues' timelines.
- **Code review:** pull requests from branches or forks, review requests,
  comments on lines of the diff (with replies, marked outdated once the line
  changes), suggested changes that can be applied with one click, approvals
  and change requests, and merging with a merge commit, squash or rebase,
  with conflict detection. Draft pull requests can't be merged until they're
  marked ready for review; auto-merge merges once checks pass and reviews are
  in.
- **Branch protection:** require pull requests, a number of approvals,
  passing checks and verified commits, and refuse force pushes and deletion,
  for branches by name or pattern.
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
  comments, pull requests, reviews, releases, wiki edits, stars, board
  changes and archiving, in GitHub's payload format and signed, with retries
  and a delivery log.
- **Email** (optional, via any SMTP server): notification emails, address
  verification and "forgot password".
- **People:** accounts with optional two-factor authentication (authenticator
  apps and recovery codes), single sign-on with an OpenID Connect provider
  (Keycloak, Authentik, Google, Microsoft Entra, …), organizations (owners and members),
  per-repository collaborators (read/write/admin), private repositories.
- **Importing** from GitHub, Gitea or Forgejo: the code with Git LFS files,
  issues and pull requests (numbers kept), comments and reviews, labels,
  milestones, releases with their files, and the wiki.
- **Profiles** in the style of GitHub's and Forgejo's: an avatar (uploaded,
  or a generated pattern), name, bio, location and website; pinned (or
  most-starred) repositories with their main language; a calendar of the
  year's contributions; and recent activity. Organizations have a name,
  description, location and website, with tabs for an overview (pinned or
  popular and recently changed repositories, people, top languages and
  topics), repositories, projects (for members) and people. Repository
  lists on profiles can be searched, narrowed (sources, forks, archived,
  templates) and sorted.
- **Renaming and transferring** repositories (to you or your organizations);
  old links, clones and remotes keep working.
- **Administration:** a page for site administrators with statistics,
  accounts (create, invite by email, disable, delete, grant admin), all
  repositories, closing sign-ups and a notice on every page, and a System
  page with what keel uses: memory, threads and async tasks, running git
  processes, caches, database connections and storage by kind.
- **Light and dark themes** that follow the system, or that people pick with
  the switch at the bottom of every page.
- **JSON API** in the style of GitHub's, with personal access tokens.

CI is not built in: connect one with a webhook, and let it report back with
commit statuses.

## Quick start

Requirements: a Rust toolchain (1.99.1+) to build, and `git` (2.38+, for
`merge-tree --write-tree`) on the `PATH` at runtime; keel won't start with
an older one.

```sh
cargo build --release
./target/release/keel serve
```

Open http://localhost:3000 and create an account. The first account becomes
the administrator, so signing up for it needs the setup code keel logs at
startup while there are no accounts:

```
WARN no accounts yet: sign up at http://localhost:3000/register with the setup code K7QD-M2XA-9TPW-RC4F to become the administrator
```

Whoever reaches a new server first can't take it over without it. The code
is new every start and stops working once the administrator exists. Then:

```sh
git push http://localhost:3000/<you>/<repo>.git main       # HTTP: username + password or access token
git push ssh://git@localhost:2222/<you>/<repo>.git main    # SSH: add your key under Settings
```

All state lives in `./data`: `keel.db` (SQLite), `repos/<id>.git` (bare
repositories), `lfs/` (Git LFS files), the generated SSH host key, the key
keel signs its commits with, `secret.key` (see below) and `search/` (the
code search index, which is rebuilt automatically if deleted).

Secrets keel has to read back (two-factor secrets, webhook secrets, mirror
passwords, import tokens) are stored encrypted (AES-256-GCM) with
`secret.key`, so a copy of the database alone doesn't give them away. To keep
the key out of the data directory, and so out of its backups, move it
elsewhere and point `--secret-key-file` at it. Keep a copy: without it,
keel won't start on a database with encrypted secrets, rather than make a
new key that can't read them. Secrets stored by older versions are encrypted
at the first start. The secret that signs Git LFS tokens, image proxy URLs
and single sign-on attempts is derived with `secret.key` too, so a copy of
the database can't forge those either.

### Backups and monitoring

```sh
./keel backup --data-dir data /backups/keel-2026-10-24
```

copies the data directory while the server keeps running: the database as a
consistent snapshot, then every repository and wiki, LFS file and release
file, the SSH host key, the key keel signs its commits with and
`secret.key` if it's in the data directory. The
copy is a data directory itself, so restoring is `keel serve --data-dir
/backups/keel-2026-10-24` (or copying it back). Archive it with `tar` or your
backup tool of choice. The database holds secrets (two-factor and webhook
secrets, mirror passwords), so the copy's directory is readable only by its
owner (`0700`, the database `0600`), like a new data directory; symlinks in
the data directory aren't followed.

`GET /healthz` answers `200 {"status":"ok"}` while the server can use its
database and `503` otherwise, for load balancers and uptime monitors. With
the metrics token (below) it also says keel's version and uptime; without
it, the version would tell strangers which known vulnerabilities to try.

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
| `--max-push-size` | `KEEL_MAX_PUSH_SIZE` | `4096` (MiB) per push |
| `--quota` | `KEEL_QUOTA` | no limit; MiB each account may store (git data, wikis, LFS and release files) |
| `--max-repos` | `KEEL_MAX_REPOS` | no limit; repositories per account (and organizations a person may own) |
| `--oidc-issuer`, `--oidc-client-id`, `--oidc-client-secret` | `KEEL_OIDC_ISSUER`, `KEEL_OIDC_CLIENT_ID`, `KEEL_OIDC_CLIENT_SECRET` | no single sign-on; see below |
| `--oidc-name` | `KEEL_OIDC_NAME` | `single sign-on`: the button says "Sign in with <name>" |
| `--oidc-allow-signup` | `KEEL_OIDC_ALLOW_SIGNUP` | off: the provider creates accounts only while sign-ups are open |
| `--oidc-link-by-email` | `KEEL_OIDC_LINK_BY_EMAIL` | off: existing accounts are connected in Settings → Security, not by matching addresses |
| `--secret-key-file` | `KEEL_SECRET_KEY_FILE` | `<data dir>/secret.key`; created if missing |
| `--block-outbound` | `KEEL_BLOCK_OUTBOUND` | none; more networks webhooks, mirrors and imports can't reach, e.g. `203.0.113.0/24,2001:db8::/32` |
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
clone URLs use it, and session cookies are marked `Secure`. Also listen on
localhost only (`--http-addr 127.0.0.1:3000`), so nobody can reach keel
past the proxy; keel warns at startup if it listens on all interfaces with
an `https://` base URL. With open sign-ups, set `--quota` and `--max-repos`
(keel warns otherwise). Add the server's own public addresses to
`--block-outbound`: they count as public, so without it webhooks and mirrors
can reach services listening on them.

### Single sign-on

People can sign in with an OpenID Connect provider. Register keel there as
a confidential client (with a client secret) using the authorization code
flow, with the redirect URI `https://git.example.com/login/oidc/callback`
and the scopes `openid email profile`. Then:

```sh
KEEL_OIDC_CLIENT_SECRET='…' ./keel serve --base-url https://git.example.com \
    --oidc-issuer https://id.example.com/realms/main --oidc-client-id keel --oidc-name "Example ID"
```

The sign-in page then offers "Sign in with Example ID". Which account that
signs in to:

1. the account it's connected to, if it is (Settings → Security →
   Connect, which asks for the password, or the first time it was used);
2. otherwise, with `--oidc-link-by-email`, the account with the same email
   address, if the provider says it verified the address and keel verified
   it too, so it's the same person; the two are then connected. Never a
   site administrator's account, nor one with two-factor authentication on
   (the connection would be made before the second step): they connect in
   Settings. Only turn it on
   for a provider whose `email_verified` you trust, since it then vouches
   for every account's address (some providers mark every address
   verified, or let people change theirs). Without it, signing in with the
   provider tells people with an account to sign in and connect it;
3. otherwise a new account, named after the provider's username, while
   sign-ups are open, or always with `--oidc-allow-signup` (for a provider
   that only has your own people; don't use it with a public provider like
   Google). The first account, the administrator, can't be made this way: it
   needs the setup code.

Accounts made this way have no password. Where keel asks for the password
(adding keys and tokens, changing the address), they're confirmed by having
signed in in the last ten minutes, in that browser; they can set a password
in Settings → Account. Two-factor authentication still applies after signing
in with the provider, if it's on. Disconnecting needs a password, so the
account can still be signed in to. Whenever an account gets connected to the
provider, keel emails its owner, so a connection they didn't make shows up.

keel uses the authorization code flow with PKCE, its state in a signed
cookie for ten minutes. It checks the ID token's issuer, audience, expiry and
nonce; it doesn't check its signature, which OpenID Connect allows because
keel gets it directly from the provider's token endpoint over HTTPS (Core
1.0, 3.1.3.7). So the issuer and its endpoints must be `https://` (or, for
trying it out, an issuer on `http://` to localhost, whose endpoints may be
too).

### Behind nginx

keel can't limit requests per address behind a proxy: it only sees the
proxy's address, and it doesn't trust `X-Forwarded-For`, which anyone can
send when keel is reachable directly. So the proxy should do it. keel limits
wrong passwords per account (ten in 15 minutes); a per-address limit stops one
client from guessing across many accounts, or from locking someone out by
sending wrong passwords for their account. A configuration:

```nginx
# Requests per client address. Signing in: 10 a minute, bursts of 5.
limit_req_zone $binary_remote_addr zone=keel_login:10m rate=10r/m;
# Git over HTTP with a password or token (Basic credentials): 60 a minute,
# bursts of 60, which a clone or push (2–4 requests) is far from.
map $http_authorization $keel_basic { "~^Basic " $binary_remote_addr; default ""; }
limit_req_zone $keel_basic zone=keel_git:10m rate=60r/m;
# Everything else: 20 a second, bursts of 100.
limit_req_zone $binary_remote_addr zone=keel_all:10m rate=20r/s;
limit_req_status 429;

server {
    listen 443 ssl http2;              # nginx 1.25.1+: `listen 443 ssl;` and `http2 on;`
    server_name git.example.com;
    ssl_certificate     /etc/letsencrypt/live/git.example.com/fullchain.pem;
    ssl_certificate_key /etc/letsencrypt/live/git.example.com/privkey.pem;
    add_header Strict-Transport-Security "max-age=31536000; includeSubDomains" always;

    client_max_body_size 0;            # keel limits pushes (--max-push-size) and LFS files itself
    proxy_request_buffering off;       # stream pushes to keel
    proxy_buffering off;               # stream clones back
    proxy_read_timeout 3600s;
    proxy_send_timeout 3600s;
    large_client_header_buffers 4 16k; # caps URL length
    proxy_set_header Host $host;

    # Where passwords and codes are checked: sign-in, its second step,
    # sign-up, and password resets.
    location ~ ^/(login|login/2fa|register|password/forgot|password/reset)$ {
        limit_req zone=keel_login burst=5 nodelay;
        limit_req zone=keel_all burst=100 nodelay;
        proxy_pass http://127.0.0.1:3000;
    }

    location / {
        limit_req zone=keel_git burst=60 nodelay;   # only requests with Basic credentials
        limit_req zone=keel_all burst=100 nodelay;
        proxy_pass http://127.0.0.1:3000;
    }
}
server { listen 80; server_name git.example.com; return 301 https://$host$request_uri; }
```

The `keel_git` zone only counts requests that carry Basic credentials, since
requests with an empty key aren't limited: that covers git over HTTP and
Git LFS with a password or token, without slowing down browsing. Giving
locations both zones applies both.

Check it from another machine: quick wrong sign-ins get `429` once the burst
of five is used up:

```sh
for i in $(seq 15); do
  curl -s -o /dev/null -w '%{http_code} ' -H 'Origin: https://git.example.com' \
    -d login=nobody -d password=wrong https://git.example.com/login
done; echo
# 200 200 200 200 200 200 429 429 …
```

People behind one address (an office, a university) share its limits; if
that's a problem, raise the rates or give their network its own zone with a
`geo` block. Prefer SSH keys or tokens for git: neither is limited per
account, and SSH doesn't go through nginx at all.

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
| `GET /users/{name}`, `GET /users/{name}/repos`, `GET /orgs/{org}`, `POST /orgs/{org}/repos` | an organization has a `description` where people have a `bio` |
| `GET /user/keys`, `/user/gpg_keys`, `POST`, `GET`, `DELETE …/{id}` | your SSH keys (`title`, `key`) and GPG keys (`armored_public_key`); adding and removing them takes a token created to manage keys |
| `GET`, `POST /repos/{owner}/{repo}/keys`, `GET`, `DELETE …/keys/{id}` | deploy keys, for repository admins: `title`, `key`, `read_only` (unlike GitHub's, `true` unless set to `false`) |
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
| `GET /orgs/{org}/projects[/{n}]`, `POST …/{n}/cards`, `…/cards/{id}/moves` | an organization's boards, for its members; cards also take `repository` (`name`) |
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
templates. There is no frontend build. The few hundred lines of JavaScript
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

**Repositories are kept packed.** git's own housekeeping is off (`gc.auto`:
the server decides when), so keel does it after pushes, in the background:

- Refs go into `packed-refs` once more than 32 are files of their own.
  Every clone, fetch and push starts by listing them: with a thousand loose
  ones that takes 14 ms, packed 2 ms.
- A repository whose pack has no bitmap index (after a first push, an
  import or a mirror's first sync) is repacked into one pack that has. A
  full clone of git.git then costs the server 0.6 s of work instead of
  2.8 s. A pack shared with a fork is left alone: rewriting it would end
  the sharing, and the fork has the parent's bitmap anyway.
- Loose objects (small pushes, edits in the browser and merges leave them)
  are packed once there are more than 2,000; more than 16 packs become one.
- Objects nothing refers to any more go into a pack of their own and are
  deleted two weeks later, as `git gc` does, so nothing is taken from a
  push that's still under way.

One repository is repacked at a time (it uses every core), after everything
else a push sets off. Repositories from before keel did this, and those
nobody pushes to, get their turn when it starts.

**Reads use libgit2, except where git is faster.** Trees, blobs, diffs and
plain history use libgit2. Path-limited history ("History" for a file or
directory) uses `git log`, because git can use the changed-path Bloom filters
keel writes into each repository's commit-graph after every push. On git.git
that is about 50 ms instead of 5 s. History past its first thousand commits
(`?page=22` and on) comes from `git log` too: git reads the commit-graph
where libgit2 parses every commit it skips, so the last page of git.git's
history takes 25 ms instead of 600. libgit2 doesn't re-hash each object it
reads (its default; git doesn't either, and objects are checked as they're
pushed), which makes reading files and walking history 20–35% faster.

**Last commit per file** (the column in tree views) is one streamed
`git log --name-only` walk per directory that stops once every entry is
resolved. Directories, and entries whose change arrived through a merge, are
confirmed with a Bloom-filtered `git log -1 -- <path>` so the result matches
git's history simplification. If the result isn't known within a small budget
(500 commits), the page is sent immediately and the browser fetches the
column from `/<owner>/<repo>/last-commits/...`. The whole walk then runs in
the background, once per directory, whether or not a browser asks (crawlers
don't): pages don't walk again meanwhile, and the browser's request waits
for the same result.

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
search within a repository, by scanning it (a few such scans run at a time,
like blames). The `regex` crate guarantees
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
keeps its new commits out of a public parent. For the same reason a pull
request from a private repository into a public one can't be opened (the
compare page still shows the changes to those who can see both): it would
copy the private commits to where everyone can fetch them.

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
can't take over the CPU, and git stops when a download makes no progress
for a minute, so clients that stop reading don't keep a turn. LFS files are included as pointers.

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
in all and go into git directly; push big files with Git LFS instead. They're
held in memory until they're committed, so two are received at a time (others
wait their turn), each within ten minutes, and nothing is kept of one that's
too big or more than its owner may still store. File names that Windows or
macOS would take for `.git` (`.git.`, `git~1`, `.git` with invisible
characters) are refused, as git refuses them in pushes.

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
`repo:`, `user:`, `project:`, `sort:`; `@me` is you; other words are looked for in titles
and descriptions) into one static SQL query that only ever sees repositories
the viewer can read. `/issues` and `/pulls` show what involves you across
repositories.

**Imports** run in the background, one at a time, through the same guarded
network access as mirrors (no private addresses unless allowed; git is
pinned to the checked address). git brings branches, tags and pull requests'
heads (`refs/pull/<n>/head`, where keel keeps them too), then Git LFS files;
the API brings the rest (at most 32 MiB per response and 512 MiB in all,
so a source can't make keel hold more). Imported issues, comments and releases keep their
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
a message for the person pushing; without rules it does nothing. git never
runs a repository's own hooks.
A rule can require verified commits: the hook checks the commits a push
brings to the branch (from its old tip, or for a new branch those not on
another branch requiring them yet), reading keys from the database
read-only, and refuses unverified ones by name. Merging into such a branch
with a merge commit (or a fast-forward rebase) needs the pull request's own
commits verified too; a squash makes one new, signed commit.
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
stop syncing. Webhooks get GitHub's `repository` event (`archived`,
`unarchived`).

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
The commits keel makes (merge commits, squashes, rebased commits, edits in
the browser, applied suggestions, wiki edits, repositories from templates)
are signed with the server's own SSH key (`data/signing_ed25519_key`,
created on first start and kept in backups), and show as "made on" the
site; keel vouches for who made them, so the committer email isn't checked.
Its public key is at `/signing-key.pub`, so git can check them too:

```sh
echo "* namespaces=\"git\" $(curl -s https://git.example.com/signing-key.pub)" >> ~/.ssh/allowed_signers
git -c gpg.ssh.allowedSignersFile=~/.ssh/allowed_signers verify-commit HEAD
```
Whether a signature matches a key can't change, so that result is kept in
memory (the cryptography is the slow part: about 0.4 ms for an RSA-3072
signature); who owns the key, and their email, are looked up every time.

**Project boards** belong to a repository and hold its issues, pull requests
and notes, or to an organization (`/org/<name>/projects`) and hold issues and
pull requests of any of its repositories (`repo#12` adds one). Organization
boards are for its members (and site administrators): others get a 404, and
don't see them on issues or in their timelines; cards of repositories that
left the organization since are hidden from those who can't read them. Cards
are moved with forms (each card's menu), or dragged; the script posts the
move and then reloads the board's markup. A column can be the done column:
closing or merging an issue moves its cards there, and reopening it moves them
back to the first column. Adding a card, moving it to another column and
removing it show in the issue's timeline. Webhooks get GitHub's (classic
projects) `project`, `project_column` and `project_card` events: a
repository's boards go to the repository's and its organization's hooks, an
organization's boards to the organization's hooks. `project:ada/engine/1`
or `project:acme/1` finds a board's issues.

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
repositories; each person can change that. A comment notifies the first 50
people it mentions, so one comment can't notify (and email) every account.

**Organization webhooks** get the events of all of the organization's
repositories, in addition to each repository's own hooks.

Webhooks don't follow redirects or use proxies. Unless
`--webhook-allow-private` is set, they can't reach loopback, private, link-local
or other internal addresses (nor the networks set aside for benchmarking,
documentation and protocol use, which aren't on the internet): names are
resolved by keel and checked on the addresses actually connected to, so a
hostname pointing at an internal address doesn't get around it.

**Deploy keys** are SSH keys of a repository rather than of a person, for
servers and CI: Settings → Deploy keys, or the API. A key reaches only its
repository (not its wiki), only reads unless it's allowed to push, and
pushes with it follow the branch protection rules like anyone's (pushes by
a deploy key have no pusher in the activity feed and webhooks, like a
mirror's). A key can be one repository's deploy key, or someone's SSH key,
but not both, so it always means one thing. Git LFS works with them over
SSH (`git-lfs-authenticate` hands out a token for the key); locking files
doesn't, since a lock belongs to a person, and to a deploy key every lock is
someone else's.

**Permissions:** a user's access to a repository is the highest of: admin for
the owner, organization owners and site admins; write for organization
members; the collaborator role; read for anyone if the repository is public.
Archived repositories cap it at read.

**Background work** listens to push events from `keel-core` (after repository
maintenance): cache warming, search indexing and pull request updates.

**Caches are keyed by git object ids**, so they never need invalidation, only
a size bound: last commits per (commit, directory), highlighted files per
blob, rendered Markdown per blob, blame per (commit, file). The branches and
tags in the switcher are ordered by their commits, which takes reading each
(15 ms for a thousand tags, on every page): the list is kept under a hash of
what the refs point at, which is cheap to list and changes when they do.

**Blame** runs `git blame --incremental`, which skips commits that didn't
touch the file using the commit-graph's changed-path Bloom filters, and
follows renames. It's the costliest page on a long history and anyone who can
read the repository can ask for it, so blames run at most one per two CPUs at
a time (others wait their turn), stop after 20 seconds (the page then says so
and links to the file's history), and links to blame pages are `nofollow`
for crawlers. Binary files, Git LFS files and files over 2 MiB aren't blamed.

**Loading pages:** pages are compressed with brotli at quality 4 (the
default, 11, is for compressing ahead of time and cost more than rendering
the page); the stylesheet, script and icon are compressed once at startup,
at the best quality, and cached by browsers for good. Only text is
compressed: archives, release files and other downloads are sent as they
are, with their length. Pages tell
Chromium-based browsers (Speculation Rules) to load a link while the pointer
rests on it, so the page is often there by the time of the click; only pages
that change nothing when loaded qualify (not signing in or out, downloads,
raw files, git or `nofollow` links). Debug builds run with optimized
dependencies (`[profile.dev.package."*"]`).

**Rendering work is bounded:** syntax highlighting stops at 256 KiB, file
display at 2 MiB, and diffs at 300 files / 10,000 lines (a file over 4 MiB
counts as binary there). Highlighted code is marked up where its colour
changes, with a class per colour, not with a span for every syntax scope:
an 80 KB C file makes a 0.7 MB page (68 KB compressed) instead of 2 MB,
in the same colours.

**Files are read as far as they're shown.** A file in a repository can be
gigabytes, and reading one holds all of it in memory. Its size comes from
the object's header first: pages, blame, diffs and the API leave a file
that's too large for them unread, a request that only asks whether a raw
file changed (`If-None-Match`) is answered from its id, and raw files over
1 MiB are streamed from `git cat-file`, which stops when the client does.

### Measured on git.git (82k commits), release build, warm OS cache

| Page | Time |
| --- | --- |
| Tree (cached last commits) | ~4 ms |
| Tree (first view; column loads afterwards) | ~30–130 ms + 0.1–3 s in the background |
| File view, highlighted (7,860 lines; first view) | ~6 ms (~340 ms) |
| Commit list | ~3 ms |
| Commit list, the last of 1,659 pages | ~25 ms |
| File history | ~10 ms |
| Blame (`diff.c`, 7,860 lines; first view) | ~9 ms (~0.4 s) |
| Commit with diff | ~3 ms |
| Code search for a common word, in one repository or all | ~15 ms |
| Contributors graph (first view; after a push only new commits are counted) | ~5 ms (~375 ms) |
| Commit activity graph | ~1 ms |
| Commit list of 50 signed commits, all verified (first view, RSA-3072 GPG) | ~3.5 ms (~20 ms) |

## Security

- Passwords: Argon2id. Session tokens are 256-bit random values, stored only
  as SHA-256 hashes, sent in `HttpOnly; SameSite=Lax` cookies. Behind
  `https://`, these and the cookies used while signing in are `__Host-`
  cookies, which other sites under the same domain can't set, so they can't
  plant a session of their own.
- After ten wrong passwords for an account (by name or email) within 15
  minutes, signing in with its password (on the web and over git HTTP) is
  refused for the rest of those 15 minutes. The count is kept in memory.
- Changing the email address, adding SSH or GPG keys and creating access
  tokens ask for the password, so a stolen session can't be turned into
  lasting access. Through the API, only a token created to manage keys
  (which takes the password) adds or removes them: any other token, leaked,
  could otherwise add a key that outlasts it, and that over SSH is its
  owner in full, an administrator included.
- Email addresses are plain addresses (`ada@example.com`, not `Ada
  <ada@example.com>`), so each belongs to one account, and nobody gets an
  account's mail sent to an address that has one already.
- State-changing requests must come from the same origin (CSRF), going by
  the `Sec-Fetch-Site`, `Origin` or `Referer` header the browser sends.
  Behind `https://`, an origin on plain HTTP doesn't count, and requests
  without any of these headers are refused (scripts use the API).
- Redirects after signing in only go to local paths, and never to anything
  with control characters, spaces or backslashes, which browsers could turn
  into another host.
- Password reset and verification links are single-use, expire (1 hour and
  7 days), are stored only as SHA-256 hashes and are sent at most once a
  minute per account. Their token is after the `#`, which browsers don't
  send, so it stays out of the proxy's and keel's request logs; the page
  moves it into its form. Verifying an address takes a click, so mail
  scanners that open links don't use the link up.
- Invitations (Admin → Accounts, needs email) make the account with a long
  random password nobody sees, and email a link to choose one, which works
  once, for a week. The email never holds a password, so none sits in
  mailboxes or in keel's outbox. Asking for a reset says the same thing whether or not an
  account has that address. A reset signs out all sessions.
- Webhook deliveries are signed with HMAC-SHA256 when a secret is set
  (`X-Hub-Signature-256: sha256=…`), and can't target internal addresses by
  default (see above).
- Personal access tokens are shown once and stored hashed; they can be
  read-only, expire, and be revoked individually. Changing a password signs
  out all other sessions. An administrator's token only acts as an
  administrator (every repository, the admin API) if it was created for
  site administration; otherwise it reaches what its owner could without
  being one, so a leaked CI token isn't a key to everything. (Tokens from
  before 0.19 are everyday tokens.) Likewise only a token created for it
  changes its owner's SSH and GPG keys.
- Strict Content Security Policy. Raw files are served with `sandbox` and
  never as `text/html`, so user content can't run scripts on keel's origin.
  Git LFS files, which anyone can fetch from a public repository, are
  served sandboxed too, and they and everything git answers say `nosniff`,
  so no browser takes what someone uploaded for a script or a page.
- Images from other servers in Markdown (issues, comments, READMEs,
  releases, wikis) load through keel's image proxy (`/_image`), like GitHub's
  camo, so whoever posts one can't see who viewed it; pages may only load
  keel's own images. Proxy URLs are signed (keel fetches only what its pages
  show), fetched like webhooks (no private or blocked addresses), limited to
  images up to 10 MiB, and served sandboxed. Links to keel's own images
  (on `--base-url`) aren't proxied: the viewer loads them directly, with
  their access to private repositories, and keel never fetches itself.
- Secrets that must be read back are encrypted in the database (see
  `secret.key` above). At startup, a data directory or database file that
  others can read is made private (`0700` and `0600`); SQLite gives its
  `-wal` and `-shm` files the database's permissions. Under systemd, also
  set `UMask=0077`.
- Private repositories return 404 (web) or 401 (git) to anyone who can't see
  them, so their names don't leak. That includes fork lists, the
  repositories offered on the compare page, and the parent a fork names
  (in its header and in the activity feed).
- Only `git-upload-pack` and `git-receive-pack` can be run over SSH, once
  per channel, with at most ten channels per connection; git stops when its
  channel or connection closes. Repositories never run their own hooks (only
  keel's branch protection hook), and `GIT_PROTOCOL` values are validated
  before being passed to git. A connection can stay open for days, so
  before every command keel checks that the key it signed in with is still
  there and its account not disabled, and otherwise ends the connection.
- Gzip-compressed git requests are decompressed as they stream into git, and
  refused (`413`) past 64 MiB, as are uncompressed fetch requests (which
  only list what the client has and wants). Pushes are limited to
  `--max-push-size` (`receive.maxInputSize`), and git checks every pushed
  and fetched object (`receive.fsckObjects`, `fetch.fsckObjects`), so
  malformed objects and trees with `.git` entries are refused.
- Quotas (`--quota`, `--max-repos`; off by default, and site administrators'
  accounts are exempt): repositories, forks, mirrors, imports and transfers
  past the repository limit are refused; pushes (over HTTP and SSH), LFS
  uploads, release files, forks, repositories from templates, changes in
  the browser (files, suggestions, wiki pages), mirror syncs and imports
  are refused once an account is out of storage. Uploads are checked as
  they're recorded, so several at once can't share the same room, and a
  mirror sync or import stops once git has fetched more than the room left.
  Git data is measured after each push, so a push can go over by at most
  `--max-push-size`. LFS files count once per account, however many of its
  repositories have them.
- At most as many password hashes (Argon2id, 19 MiB each) are computed at
  once as there are CPUs; other sign-ins wait their turn, so a flood of
  sign-in attempts can't exhaust memory. Issue and Explore searches are
  limited to 256 characters (10 words, 10 labels).
- The API only accepts access tokens, never passwords or cookies, so it can't
  be used cross-site or to guess passwords.
- Administrators can disable accounts: sessions end at once, and passwords,
  tokens and SSH keys stop working, also over SSH connections that were
  already open.
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
  Turning it off or making new recovery codes takes the password and a code
  (wrong ones count towards the same lockout), so a stolen password and a
  signed-in browser aren't enough to take the second factor away.
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
