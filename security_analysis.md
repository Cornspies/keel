# Security analysis of keel 0.16, for an internet deployment

Scope: running `keel serve` on a server reachable from the internet, behind a
reverse proxy that terminates TLS. The code at `main` (0.16) was read with an
attacker's view: anonymous visitors, people who sign up, writers of a
repository, and someone who controls a remote that keel fetches from. Each
finding below was checked in the code; file references are given so it can be
fixed from this list.

**Bottom line:** the application code is in good shape. Authentication,
sessions, CSRF, XSS, SQL, path handling, command execution and outbound
request filtering are done carefully, and the authorization checks I traced
are all scoped correctly. The real risks for an internet deployment are
**operational**: the first-account-is-admin window, unlimited resource use by
anyone who can sign up, a cheap unauthenticated way to exhaust memory and CPU,
and git accepting objects without checking them. Most of them are closed by
deployment settings (section 1) plus a few small code changes (section 2).

## Summary

| ID | Finding | Who can do it | Severity | Fixed by |
|---|---|---|---|---|
| D-1 | The first account to sign up becomes administrator, even with `--closed-registration` | anyone, before you sign up | **Critical** (deployment window) | create your account before exposing it; code fix suggested |
| D-2 | No quotas: pushes, LFS, release files, imports and mirrors can fill the disk | anyone who can sign up (open by default) | **High** | `--closed-registration`; disk isolation; code fix |
| D-3 | Password checks run without a concurrency limit, about 19 MiB each | anonymous | **High** | proxy rate limit; code fix |
| D-4 | The app port listens on all interfaces by default | anyone, if no firewall | **Medium** | `--http-addr 127.0.0.1:3000` |
| G-1 | Pushed and fetched git objects aren't checked (`fsckObjects`) | writers, remotes of mirrors and imports | **Medium** | code fix (repository config) |
| G-2 | No size limit on a push | writers (anyone, with open sign-up) | **Medium** | code fix; covered by D-2 |
| S-1 | Issue and Explore searches have no length limit | anonymous | **Medium** | code fix; proxy limit on URL length |
| N-1 | Webhooks, mirrors and imports can reach the server's own public address | anyone who can sign up | **Medium–Low** | firewall; bind other services to localhost |
| A-1 | Anyone can lock an account's password sign-in, and there is no per-address limit | anonymous | **Low** | proxy rate limit; use SSH keys and tokens for git |
| A-2 | An administrator's access token works on every repository on the site | holder of the token | **Low** | separate admin and everyday accounts |
| A-3 | Secrets are stored in the database as they are; old data directories keep loose permissions | local users, backup thieves | **Low** | `chmod`; encrypted backups |
| P-1 | Images in Markdown load from anywhere (`img-src *`) | anyone who can comment | **Low** (privacy) | optional |
| P-2 | Reset and verification tokens are in URLs, so they end up in proxy logs | people who read logs | **Low** | log hygiene |
| P-3 | `/healthz` shows the version and uptime | anonymous | **Informational** | optional |
| G-3 | keel runs `git` and libgit2 on untrusted repositories | — | **Informational** | keep git and keel updated |

---

## 1. Deployment checklist

Do these before the server is reachable from the internet. They need no code
changes.

1. **Create your own account first** (D-1). Start keel bound to localhost,
   open it through an SSH tunnel (`ssh -L 3000:127.0.0.1:3000 server`), sign
   up, and only then put the reverse proxy in front of it.
2. **Bind to localhost** (D-4): `--http-addr 127.0.0.1:3000`. The default
   `0.0.0.0:3000` lets anyone reach keel directly, in plain HTTP, past the
   proxy's TLS and rate limits. Only the SSH port (`--ssh-addr`, default
   2222) should be public, or use `--no-ssh`.
3. **Set the public URL**: `--base-url https://git.example.com`. Session
   cookies are only marked `Secure` when it starts with `https://`, and clone
   URLs, email links and the CSRF check use it.
4. **Close sign-ups** unless strangers are meant to use the server (D-2):
   `--closed-registration`, then create accounts on the admin page. If
   sign-ups stay open, see section 2 for quotas, and put the data directory on
   its own filesystem so a full disk can't take down the rest of the server.
5. **Turn on two-factor authentication** for your account (Settings →
   Security). The admin account controls every repository and account.
6. **Use a separate everyday account**, and give CI tokens to that account,
   not the admin (A-2). An admin's write token can read and delete every
   private repository through the API.
7. **Keep `--webhook-allow-private` off.** With it on, any user can make
   keel send requests to your private network.
8. **Run keel as its own unprivileged user**, with the data directory `0700`
   (A-3). New data directories already are; ones created by older versions
   aren't changed: `chmod 700 data && chmod 600 data/keel.db*`.
9. **Firewall**: inbound, only 80/443 (the proxy) and the SSH port. Outbound,
   consider blocking the keel user from your server's own public addresses
   (N-1). Make sure databases, Docker's API and admin panels on the host
   listen on localhost only.
10. **Keep git updated** (G-3): keel runs `git upload-pack`, `receive-pack`,
    `archive` and `merge-tree` on repositories anyone can push.
11. **Back up** with `keel backup` and store the copies encrypted: they hold
    two-factor secrets, webhook secrets and mirror passwords (A-3).

### Reverse proxy

The proxy must pass large, long requests (git pushes, LFS) and should add
the rate limits keel can't apply itself: behind a proxy, keel only sees the
proxy's address, and it doesn't trust `X-Forwarded-For` (correctly, since
that header can be spoofed when keel is reachable directly). An nginx
example:

```nginx
limit_req_zone $binary_remote_addr zone=keel_login:10m rate=10r/m;
limit_req_zone $binary_remote_addr zone=keel_all:10m rate=20r/s;

server {
    listen 443 ssl http2;
    server_name git.example.com;
    # ssl_certificate …; ssl_certificate_key …;

    add_header Strict-Transport-Security "max-age=31536000; includeSubDomains" always;

    client_max_body_size 0;            # pushes and LFS can be large; keel limits LFS files itself
    proxy_request_buffering off;       # stream pushes to keel
    proxy_buffering off;               # stream clones back
    proxy_read_timeout 3600s;
    proxy_send_timeout 3600s;
    large_client_header_buffers 4 16k; # caps URL length (S-1)

    # Password sign-in (D-3, A-1).
    location = /login       { limit_req zone=keel_login burst=5 nodelay; proxy_pass http://127.0.0.1:3000; }
    location = /login/2fa   { limit_req zone=keel_login burst=5 nodelay; proxy_pass http://127.0.0.1:3000; }
    location = /register    { limit_req zone=keel_login burst=5 nodelay; proxy_pass http://127.0.0.1:3000; }

    location / {
        limit_req zone=keel_all burst=100 nodelay;
        proxy_pass http://127.0.0.1:3000;
        proxy_set_header Host $host;
    }
}
server { listen 80; server_name git.example.com; return 301 https://$host$request_uri; }
```

Git over HTTP sends the password with every request (`/…/info/refs`,
`/…/git-upload-pack`, `/…/info/lfs/…`). The general limit covers those; a
stricter one there would slow down normal clones.

### systemd

```ini
[Service]
User=keel
Group=keel
ExecStart=/usr/local/bin/keel serve --data-dir /var/lib/keel --http-addr 127.0.0.1:3000 \
    --base-url https://git.example.com --closed-registration
Restart=on-failure
NoNewPrivileges=yes
ProtectSystem=strict
ProtectHome=yes
ReadWritePaths=/var/lib/keel
PrivateTmp=yes
PrivateDevices=yes
ProtectKernelTunables=yes
ProtectKernelModules=yes
ProtectControlGroups=yes
RestrictSUIDSGID=yes
LockPersonality=yes
MemoryMax=4G
TasksMax=512
LimitNOFILE=65536
```

`MemoryMax` and `TasksMax` turn the memory exhaustion of D-3 and many
parallel git processes into a restart of keel instead of trouble for the
whole server. If git's global config is needed (e.g. `safe.directory`), keel
reads it from the keel user's `HOME`; with `ProtectHome=yes`, give it a home
under `/var/lib/keel`.

---

## 2. Findings

### D-1: the first account becomes administrator. Critical during setup

`insert_user` ([users.rs](crates/keel-core/src/users.rs), `count == 0` binds
`is_admin`) makes the first account an administrator, and lets it be created
even when registration is closed (`count > 0 && !by_admin && !registration_open()`).
`--closed-registration` is documented as "Only allow the first account to
sign up".

**Risk:** if the server is reachable before you have signed up, whoever does
first owns it: every repository, every account, the admin page. Scanners
look for freshly started forges.

**Fix:** follow step 1 of the checklist. In code, the safer design is to
not hand out admin by sign-up order: e.g. `keel admin create <name>` on the
command line (it needs access to the data directory, which proves you're the
operator), or a one-time setup token printed to the log at the first start
and required on the first sign-up.

### D-2: no quotas. High with open sign-ups

Anyone who can sign up (the default) can create any number of repositories,
and fill them:

- **Pushes** are streamed into `git receive-pack` without a size limit
  (`githttp::write_body`, the SSH channel), and `receive.maxInputSize` isn't
  set (G-2).
- **Git LFS**: up to `--lfs-max-file-size` (2 GiB by default) per file, any
  number of files.
- **Release files**: up to the same size each, 100 per release, any number
  of releases.
- **Imports and pull mirrors** fetch whole repositories, with their LFS files
  and release files, from any public URL; mirrors do it again every hour.

**Risk:** the disk fills up; keel (and anything else on that filesystem) stops
working. Imports and mirrors also make your server download whatever someone
points it at.

**Fix:** with `--closed-registration`, only people you trust can do this.
For open sign-ups, keel needs quotas: e.g. a per-account limit on repositories
and on stored bytes (packs, LFS, release files), refusing pushes and uploads
past it, `receive.maxInputSize` per push, and imports and mirrors only for
accounts an admin trusts.

### D-3: unbounded password hashing. High

`hash_password` and `verify_password` ([auth.rs](crates/keel-core/src/auth.rs))
run Argon2id with the default parameters (19 MiB of memory, 2 passes) on
Tokio's blocking pool, which grows to 512 threads. Nothing limits how many run
at once. Every password check costs that:

- `POST /login`, also for unknown names (they're checked against a dummy
  hash, so the timing doesn't tell which accounts exist);
- every git-over-HTTP and LFS request with Basic credentials that aren't a
  token;
- sign-up, password changes and resets.

The wrong-password limit (0.15) stops guessing per account, but each attempt
is still hashed, and random names are all different accounts.

**Risk:** a few hundred parallel requests ask for up to ~10 GB of memory and
all CPU cores: the server slows to a crawl or the kernel kills processes.
No account is needed.

**Fix:** a semaphore around Argon2 in `auth.rs`, sized to the number of CPUs,
so excess attempts wait instead of piling up (callers keep working, just
slower), plus the proxy rate limits above. `MemoryMax` in systemd contains the
damage meanwhile.

### D-4: listening on all interfaces. Medium

`--http-addr` defaults to `0.0.0.0:3000` ([main.rs](crates/keel/src/main.rs)).
Behind a proxy, that port is a second, unencrypted way in that skips the
proxy's TLS and rate limits. Credentials sent there (git Basic auth, the
sign-in form) travel in clear text.

**Fix:** `--http-addr 127.0.0.1:3000` (checklist step 2). Changing the
default to `127.0.0.1` would be safer, but breaks setups without a proxy.

### G-1: git objects aren't checked. Medium

The repository config keel writes (`REPO_CONFIG` in
[store.rs](crates/keel-git/src/store.rs)) doesn't set `receive.fsckObjects`,
nor `fetch.fsckObjects` for mirrors and imports. Git then stores any objects a
push or a remote sends, including malformed trees and commits, and the
`.gitmodules` and path tricks behind several git client vulnerabilities
(e.g. CVE-2018-11235). Hosting services refuse those so their users'
clients aren't attacked through them. keel also parses these objects in
process with libgit2, which is C.

**Fix:** add `("receive.fsckObjects", "true")`, `("fetch.fsckObjects", "true")`
and `("transfer.fsckObjects", "true")` to `REPO_CONFIG` (existing repositories
pick it up at their next push or maintenance run, see `apply_config`).
Imports of old repositories with harmless quirks may then fail; git's
`fsck.<msg-id>=ignore` settings can relax single checks if that happens.

### G-2: no push size limit. Medium

Part of D-2: one push can be as big as the disk. Set `receive.maxInputSize`
(e.g. `2g`) in `REPO_CONFIG`; git refuses larger packs. LFS files are
separate and already limited per file.

### S-1: searches without a length limit. Medium

Code search is capped at 256 characters (0.15), but issue search (web
`issue_search.rs`, `IssueQuery::parse`, and `GET /api/v1/search/issues`) and
Explore aren't. Each word of an issue search becomes a `LIKE '%word%'` over
the title and body of every issue
(`issue_search_where!` in [issue_search.rs](crates/keel-core/src/repos/issue_search.rs)).
Anyone, without an account, can send a query with thousands of words (the URL
can be tens of kilobytes) and keep the database busy; the pool has 8
connections, so a few such requests stall the whole site.

**Fix:** cap the query at 256 characters and the number of words at e.g. 10
in `IssueQuery::parse` (and the same for Explore), with a message like code
search's. The proxy's header buffer limit caps URL length meanwhile.

### N-1: outbound requests to your own server. Medium–Low

Webhooks, mirrors, imports and their LFS transfers refuse loopback, private,
link-local and other internal addresses, checked on the addresses actually
connected to (`keel-hooks`' resolver, git pinned with `curloptResolve`,
redirects checked again). That's thorough. What it can't know is your
server's own **public** address: `http://<server-ip>:<port>/` passes, and
reaches every service on the host that listens on all interfaces. A webhook's
delivery log shows the first 4 KiB of each response to whoever owns the hook.

**Fix:** keep other services on localhost (checklist step 9), and if possible
block the keel user from connecting to the server's own public addresses
(e.g. `iptables -A OUTPUT -m owner --uid-owner keel -d <public-ip> -j REJECT`).
In code, an option listing extra blocked addresses would do the same.

### A-1: account lockout and no per-address limit. Low

The wrong-password limit (10 per 15 minutes, per account) lets anyone stop
an account from signing in with its password, for as long as they keep
sending wrong ones. Existing sessions, SSH keys and access tokens keep
working, so this is an annoyance, not a takeover. keel can't limit by
address behind a proxy, since it sees only the proxy.

**Fix:** the proxy's rate limit on `/login`; use SSH keys or tokens for git.

### A-2: admin tokens are master keys. Low

Site administrators get admin access to every repository
(`granted_access` in [permissions.rs](crates/keel-core/src/permissions.rs)),
and their access tokens carry it. Tokens can only be limited to reading, not
to repositories. A leaked admin token from a CI job exposes every private
repository.

**Fix:** checklist step 6. Per-repository token scopes would be a larger
change.

### A-3: secrets at rest. Low

Two-factor secrets, webhook secrets, pull and push mirror passwords and
import tokens are stored as they are in `keel.db` (passwords, session and
access tokens and recovery codes are hashed). Anyone who reads the database
file or a backup gets them. New data directories are `0700` and backups
`0700`/`0600` since 0.15, but a data directory created by an older version
keeps its old permissions, and the SQLite `-wal`/`-shm` files get the default
umask.

**Fix:** `chmod` as in checklist step 8, `UMask=0077` in systemd, encrypted
backups. Encrypting these columns with a key outside the database would be
the code-level fix.

### P-1: images from anywhere. Low (privacy)

The CSP allows `img-src *` ([lib.rs](crates/keel-web/src/lib.rs),
`web_guard`), so an image in an issue or comment loads from any server: whoever
posts one learns the address and browser of everyone who views it. Nothing
runs, and GitHub has the same exposure without its image proxy. If it
matters, restrict `img-src` to `'self' data:` (external images then don't
show) or add an image proxy.

### P-2: tokens in URLs. Low

Password reset (`/password/reset?token=`) and email verification
(`/email/verify?token=`) links carry their tokens in the URL. keel's own
request log doesn't record URLs at the default level, but the proxy's access
log does. The tokens are single-use and expire (1 hour, 7 days). Keep proxy
logs private, or exclude the query string for those paths.

### P-3: `/healthz` shows the version. Informational

`/healthz` answers with keel's version and uptime to anyone. That helps an
attacker match known vulnerabilities to your server. Restrict it at the
proxy to your monitoring, if you like.

### G-3: git and libgit2 on untrusted data. Informational

keel delegates the protocol to the `git` binary and reads repositories with
libgit2 (vendored, 1.9.7 via `libgit2-sys` 0.18.8). Both process repositories
that anyone with write access shapes. keel does the right things around it:
arguments are passed as arrays with `--` before paths, revisions are parsed
into object ids before reaching git, repository hooks never run, replace refs
are ignored, `GIT_PROTOCOL` is validated, and the environment is cleared for
the transport commands. What remains is staying current: git releases fix
server-side bugs too (e.g. CVE-2022-41903 in `git archive`). `check_git` logs
git's version but doesn't refuse old ones; refusing versions below the one
keel needs (2.38) would be cheap.

---

## 3. What's already done well

These were checked and need nothing:

- **Sessions:** 256-bit random tokens stored as SHA-256 hashes, `HttpOnly;
  SameSite=Lax` cookies, `Secure` behind `https://`, server-side expiry,
  revoked on password changes, disabled accounts rejected everywhere
  (sessions, tokens, SSH keys, signatures).
- **Passwords and 2FA:** Argon2id; constant-time dummy check for unknown
  names; per-account failure limit; TOTP with replay protection and atomic
  attempt counting; recovery codes hashed; git HTTP refuses passwords for
  2FA accounts; the password is asked again for email changes, keys and tokens.
- **CSRF:** state changes need a same-origin `Origin`/`Referer` (with the
  scheme checked behind `https://`); nothing changes state on GET except
  following an emailed verification link, which needs its token; the API only
  takes tokens.
- **XSS:** askama escapes everything; every `|safe` in the templates comes
  from `ammonia`-sanitized Markdown, syntect's escaping highlighter, or HTML
  built with `escape()`; a strict CSP without inline scripts; raw files are
  never `text/html` and are served with `sandbox`; release files are always
  downloads; avatars are checked by their first bytes.
- **Redirects:** only to local paths, without control characters, spaces or
  backslashes.
- **SQL:** only static queries with bound parameters (no string-built SQL).
- **Authorization:** every route taking an id that I traced scopes it to the
  repository, hook or account in the URL (labels, milestones, comments,
  reactions, releases and their files, webhooks and deliveries, push mirrors,
  LFS locks, branch rules, board columns and cards, keys, tokens).
  Private repositories answer 404 to outsiders.
- **Files on disk:** repositories by id, LFS objects by validated hash (and
  uploads are checked against it), release files by id, upload paths cleaned,
  no symlinks followed in backups.
- **Outbound requests:** private addresses refused on the resolved address,
  redirects re-checked, git pinned to the checked address, credentials only to
  the same origin.
- **Git:** branch protection enforced by a hook that can't be skipped (absolute
  paths since 0.15), replace refs refused and ignored, pull request refs
  hidden from pushes, gzip requests streamed with a cap, archives limited to 4
  at a time, SSH channels and commands limited per connection.
- **Secrets on disk:** keys `0600`, new data directories and backups private.

---

## 4. Dependencies

`cargo audit` (RustSec database of 2026-10-25) reports five advisories and
three unmaintained crates. None of them is exploitable in how keel uses
them, but two are fixed by a plain `cargo update`:

| Crate | Advisory | Used by | Exposure |
|---|---|---|---|
| `ctutils` 0.4.3 | RUSTSEC-2026-0385, non-constant-time comparisons | the RustCrypto stack (SSH, signatures) | Low. **Fix:** `cargo update -p ctutils` (→ 0.4.5) |
| `crypto-bigint` 0.7.5 | RUSTSEC-2026-0387, timing variability | `russh`, `ssh-key` | Low: keel's own keys are Ed25519, which doesn't use it. **Fix:** `cargo update -p crypto-bigint@0.7.5` (→ 0.7.8) |
| `crypto-bigint` 0.5.5 | RUSTSEC-2026-0387 | `pgp` (through `cx448`) | None: keel only *verifies* GPG signatures, with public keys. Goes away with a `pgp` upgrade |
| `rsa` 0.9.10, 0.10.0-rc.18 | RUSTSEC-2023-0071 (Marvin), no fix released | `pgp`, `russh`, `ssh-key` | None: the attack recovers a *private* key from the timing of decryptions. keel uses RSA only to verify signatures and to check clients' public keys; its own host and signing keys are Ed25519 |
| `basic-toml`, `bincode`, `yaml-rust` | unmaintained | askama, syntect | None known; worth replacing when the crates using them move on |

Run `cargo audit` again before each release; the database changes.

---

## 5. Suggested code changes, in order

1. D-1: create the first admin from the command line (or with a setup token).
2. D-3: a semaphore around Argon2.
3. G-1, G-2: `fsckObjects` and `receive.maxInputSize` in `REPO_CONFIG`.
4. S-1: length and word limits for issue search and Explore.
5. D-2: per-account quotas, if sign-ups stay open.
6. D-4: consider `127.0.0.1` as the default address, or warn at startup when
   `--base-url` is `https://` but keel listens on all interfaces.
7. G-3: refuse git older than 2.38 at startup.
8. N-1: an option for extra blocked outbound addresses.
9. Dependencies: `cargo update -p ctutils -p crypto-bigint@0.7.5`.
