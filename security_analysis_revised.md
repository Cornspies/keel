# Security analysis of keel, revised

This revises `security_analysis.md`. Every finding there was checked against the code at commit `7bbca89` (0.14), and the important ones were reproduced. This file says which claims hold, which don't, and what to change. It is meant as a work list for fixing them.

**Don't fix the claims marked "false" or "by design".** They're listed so nobody spends time on them.

## Conventions for the fixes

- **SQL:** only static SQL (`concat!`, `macro_rules!`). A migration that rebuilds a table uses `-- no-transaction` with `PRAGMA foreign_keys=OFF; BEGIN; … COMMIT; PRAGMA foreign_keys=ON`.
- **Tests:**
  - Run `cargo test --workspace`; plain `cargo test` only covers the root crate.
  - Also run `cargo clippy --workspace --all-targets` and `scripts/smoke-test.sh`.
  - Add a regression test for each fix: a unit test in the crate, plus a smoke-test check where the bug is visible over HTTP or git.
- **README:** update its design notes where behavior changes, e.g. branch protection, and what the editor and pushes allow.
- **Commits:** the subject is the version (the next one is `0.15`), then a short bullet list. Add no AI attribution or `Co-Authored-By` trailers. Commit and push only when the user asks.
- **Starting servers:** start background servers with `… & echo $! > file.pid` and stop them with `kill $(cat file.pid)`. Never use `pkill -f` or `ps | grep`, which can match the shell running them.
- **Port 2222** may already be in use on the dev machine. Use `--no-ssh` or `--ssh-addr` for scratch servers.

## Fix order

1. H-1: relative data dir disables branch protection
2. H-5: `refs/replace/*`
3. H-3: web editor commits onto protected branches
4. H-2: deleting other repositories' release files
5. H-4: glob matcher, and delete-then-recreate
6. M-4: gzip memory use
7. H-6.1: fork PRs keep fetching after access is revoked
8. M-3: open redirect
9. M-6: login hardening
10. The rest, in the order of this file

## Summary

| ID | Original severity | Verdict | Revised severity |
|---|---|---|---|
| H-1 | High | Confirmed, reproduced | **Critical**: protection is off by default |
| H-2 | High | Confirmed | High |
| H-3 | High | Confirmed (point 4 is by design) | High |
| H-4 | High | Confirmed, reproduced; impact smaller than stated | Medium |
| H-5 | High | Confirmed, reproduced; worse than stated | High |
| H-6 | High | 1 confirmed; 2 narrow; 3 weak | Medium |
| H-7 | High | True, but GitHub does the same | Low |
| M-1 | Medium | The pattern exists; hard to exploit | Low |
| M-2 | Medium | Mostly true | Medium |
| M-3 | Medium | 1 confirmed, reproduced; 2 false | Medium |
| M-4 | Medium | Confirmed | Medium |
| M-5 | Medium | Mostly true, low impact | Low |
| M-6 | Medium | Confirmed | Medium |
| M-7 | Medium | Technically true; `SameSite=Lax` covers it | Low |
| M-8 | Medium | 1 partly true; 2 false (as a security issue) | Low |
| M-9 | Medium | Partly true | Low |
| M-10 | Medium | 1 confirmed; 2 minor | Low |
| L-1 | Low | Mostly not exploitable | Hardening |
| L-2 | Low | Partly true; one claim is wrong | Hardening |
| L-3 | Low | True | Low |

---

## High

### H-1: a relative `--data-dir` silently disables branch protection. Critical, confirmed

**Problem:**
- `keel serve` defaults to `--data-dir data` ([main.rs:40](crates/keel/src/main.rs#L40)).
- `Config::hooks_dir()` is then the relative path `data/hooks`, and `receive_pack_env` ([protection.rs:287](crates/keel-core/src/protection.rs#L287)) passes it as `core.hooksPath`.
- Git resolves a relative `core.hooksPath` from the repository it runs in. It finds no `pre-receive` there, so it skips the hook without any error.
- Result: with the default setup, writers can push to, force-push or delete any protected branch over HTTP or SSH.
- The tests never caught this because `Core::for_tests()` and the smoke test both use absolute paths.

**Reproduced** with git directly:
- `core.hooksPath=data/hooks` let a push through a hook that always fails.
- The absolute path ran the hook, and the hook refused the push.

**Fix:**
- In `Core::open` ([lib.rs](crates/keel-core/src/lib.rs)), right after `create_dir_all(&config.data_dir)`, replace `config.data_dir` with `std::fs::canonicalize(&config.data_dir)?`. That makes every path built from it absolute: hooks, repos, LFS, releases, avatars, the database and the SSH host key.
- Check that nothing uses `config.data_dir` before `Core::open`. The `keel backup` command works on its own `data_dir` argument; that's fine either way.

**Tests:**
- Unit: `Core::open` with a relative `data_dir` gives an absolute `core.config.data_dir`. Don't change the process's working directory in tests, because tests run in parallel. A path like `../<tempdir name>` relative to the current directory works without it.
- Smoke test: run the server from `$T` with `--data-dir data`, a relative path, so the existing branch-protection checks exercise the default setup.

### H-2: deleting release files of any repository. High, confirmed

**Problem:**
- `Core::delete_release_asset` ([keel-core releases.rs:349](crates/keel-core/src/releases.rs#L349)) runs `DELETE … WHERE release_id = ?1 AND id = ?2`, then calls `remove_file(release_asset_path(asset_id))` whether or not a row was deleted.
- The web handler ([keel-web releases.rs:471](crates/keel-web/src/releases.rs#L471)) passes the asset ID from the URL straight through.
- Any signed-in user can create a repository and a release, then post `/<self>/<repo>/releases/<own id>/assets/<n>/delete` for n = 1, 2, 3… and delete every repository's asset files.
- The API handler checks ownership through `release_of_asset`, so it isn't affected, but it shares the same core function.

**Fix:** in `delete_release_asset`, keep the `DELETE` result and only remove the file when `rows_affected() == 1`. Otherwise return `Error::NotFound`.

**Test:** a core test with two repositories, each with a release and an asset. Deleting B's asset through A's release must fail, and B's file must still exist on disk.

### H-3: the web editor commits onto protected branches. High, confirmed

**Problem:**
- In `Core::commit_file_edit` ([file_edits.rs:116-158](crates/keel-core/src/file_edits.rs#L116-L158)), the `Some(name)` path (the "commit to a new branch" option) only checks that the name is valid, and never checks the branch rule.
- The "branch already exists" check is skipped when `to == edit.branch`, and then `old = tip` commits straight onto that branch.
- So a writer can post `target=branch&new_branch=main` to the edit, new, upload or delete form on a protected `main` and commit directly, skipping required pull requests, approvals and checks.
- On protected branches `CommitFields::edit` ([edit.rs:144](crates/keel-web/src/edit.rs#L144)) always sets `new_branch`, so the protected path is the one that's exposed.

**Fix:**
- Before the `match`, treat `new_branch` equal to `edit.branch` (after trimming) as `None`, so it goes through the existing `may_update_branch` check.
- Also drop the `to != edit.branch &&` condition from the "already exists" check, so a real new branch must never exist yet.

**Not a bug:** the claim that new branches can be created even when they match a protected pattern like `release/*`. Pushes allow that too, as GitHub does by default. Leave it alone.

**Test:** on a repository with a `require_pull` rule for `main`, a `FileEdit` with `new_branch: Some("main")` must be refused and `main` must stay where it was.

### H-4: glob matcher panics and backtracks; delete-then-recreate. Medium, confirmed

**1. Panic on non-ASCII branch names.**
- `glob_match` ([protection.rs:66](crates/keel-core/src/protection.rs#L66)) slices `&name[i..]` at every byte offset.
- Any wildcard rule (`*`, `release/*`) panics on a branch like `release/v1-β`. Reproduced.
- The effect is smaller than the report says:
  - In the pre-receive hook, the panic makes the hook exit non-zero, so the push is refused.
  - In the server, the request's task panics and that one request fails; the process doesn't crash.
  - Either way it's a real bug.

**2. Exponential time.**
- The matcher tries every split for every `*`. Measured:
  - `a*` repeated 10 times then `b`, against 30 `a`s: 0.34 s.
  - 14 stars against 40 characters: over 2 minutes.
- Creating rules needs repository admin, but anyone can create their own repository.
- The matcher runs synchronously on async worker threads (`rule_for`, `branch_rule`, `may_update_branch`, merge checks), so a few such requests stall the server.

**Fix for 1 and 2:**
- `*` never matches `/`, so split the pattern and the name on `/`.
- The two must have the same number of segments.
- Match each pair of segments with the usual linear wildcard algorithm (two indexes plus the position of the last star), stepping by `char`, not by byte. That is linear time and needs no recursion.
- Keep the behavior: `*` is any run of characters other than `/`, and a pattern without `*` is an exact match.
- Tests:
  - `release/*` matches `release/v1-β`.
  - `*` matches `β`.
  - `*` doesn't match `a/b`.
  - The existing cases in the `tests` module still pass.
  - A pathological pattern such as `"a*".repeat(50) + "b"` against 200 characters finishes in milliseconds.

**3. Delete, then recreate.**
- In `check_update` ([protection.rs:324](crates/keel-core/src/protection.rs#L324)), with a rule that has both `require_pull` and `allow_force_push`:
  - Deleting the branch is allowed (`(Some, None)` with `allow_force_push`).
  - Recreating it is allowed (`(None, _)`).
- So two pushes replace a branch that requires pull requests with anything at all.

**Fix for 3:**
- Allow deletion only when `allow_force_push && !require_pull`.
- Make the same change for `delete == true` in `Core::may_update_branch` ([protection.rs:280](crates/keel-core/src/protection.rs#L280)), which handles branch deletion on the web and deleting the head branch after a merge ([pulls.rs:419](crates/keel-core/src/pulls.rs#L419)). Check that deleting head branches after a merge still works on unprotected branches.
- Keep `(None, _)` allowed: creating branches is by design.
- Update the README text on what "allow force pushes" means.
- Test: in `check_update` unit tests, a `require_pull` + `allow_force_push` rule refuses deletion.

### H-5: `refs/replace/*` swaps what gets merged and archived. High, confirmed and worse than stated

**Problem:**
- Pushes may create `refs/replace/*`. Only `refs/pull` is hidden from pushes ([store.rs:21](crates/keel-git/src/store.rs#L21)).
- The git command line honors replace refs, but libgit2 ignores them:
  - Diffs, file views and signature checks use libgit2, so they show the real commit.
  - `git merge-tree` (merging, the mergeability check, rebasing) and `git archive` (zip and tar.gz downloads) use the replacement.

**Reproduced:**
- The server accepted `refs/replace/<good>` pointing at an "evil" commit.
- `merge-tree --write-tree main <good>` gave the evil file content, and `git archive <good>` contained the evil file.
- With `GIT_NO_REPLACE_OBJECTS=1`, `merge-tree` gave the good content.

**Also reproduced (not in the original report):**
- `pre_receive` uses `git merge-base --is-ancestor old new` to tell fast-forwards from force-pushes ([protection.rs:371](crates/keel-core/src/protection.rs#L371)).
- A replace ref that gives `new` a fake parent `old` makes an unrelated rewrite count as a fast-forward.
- So a writer can push a replace ref, then force-push over a branch that forbids force pushes.

**Fix (do both):**
1. **Make every git command ignore replace refs.**
   - Set `GIT_NO_REPLACE_OBJECTS=1` on every `Command::new("git")` outside tests:
     - keel-git: `transport.rs` (`git_command`; it calls `env_clear()`, so set it after that), `merge.rs` (`git()`), `archive.rs`, `store.rs`, `mirror.rs`, `read.rs`, `stats.rs`.
     - keel-core: `protection.rs` (`pre_receive`'s `merge-base`; the hook runs in its own process, so set it on that command), and the non-test calls in `insights.rs`, `activity.rs`, `pulls.rs`, `auto_merge.rs`, `releases.rs`, `repos/templates.rs`.
     - `keel/src/main.rs`.
   - `grep -rn 'Command::new("git")' crates` lists them all; many hits are inside `#[cfg(test)]` and don't need it.
   - Consider one shared helper in keel-git that applies the standard environment, together with L-1.
2. **Refuse pushes to `refs/replace`.**
   - Hide `refs/replace` from pushes, as `refs/pull` already is. Git refuses pushes to refs hidden by `receive.hideRefs`.
   - `receive.hideRefs` takes several values, but `apply_config` ([store.rs:152](crates/keel-git/src/store.rs#L152)) uses `set_str`, which can hold only one value per key. Change it to keep both values, e.g. with `set_multivar` or by checking existing entries with `multivar`.
   - `apply_config` also runs in `run_maintenance`, so existing repositories pick up the change after their next push or maintenance run.
   - Optionally hide `refs/replace` from fetches too (`uploadpack.hideRefs`).
   - Don't do this in the pre-receive hook: `receive_pack_env` returns nothing for repositories without branch rules, so the hook doesn't run there.

**Tests:**
- keel-git: create a replace ref directly in a test repository (`git replace`); `mergeability` and `archive_command` must still use the original commit.
- Smoke test: `git push origin 'refs/replace/*:refs/replace/*'` must be refused.

### H-6: fork pull requests and private repositories. Medium

**H-6.1, confirmed.**
- `related()` ([pulls.rs:61](crates/keel-core/src/pulls.rs#L61)) allows a pull request whose base is a fork and whose head is the fork's parent, i.e. bringing upstream changes into the fork.
- `create_pull` checks the author's access to the head repository once.
- After that, nothing checks again:
  - `pull_tips` ([pulls.rs:216](crates/keel-core/src/pulls.rs#L216)) runs on every push to the head repository via `sync_pulls`, and again when the pull request is viewed or merged.
  - It fetches the head branch into the base repository's `refs/pull/N/head`.
  - LFS objects are shared to the base repository as well.
- Removing a collaborator doesn't touch their forks or pull requests. So after Bob loses access to `alice/secret`, an open pull request from `alice/secret` into `bob/secret` keeps copying Alice's new private commits into Bob's repository.

**H-6.2, narrow.** Making a repository private leaves its public forks public. New commits only flow into such a fork through an open pull request from the now-private parent into the fork, which is the same mechanism as H-6.1.

**H-6.3, weak; don't fix.** If a private parent is deleted, the fork owner can make the fork public. But the owner already had that snapshot and could publish it anywhere.

**Fix (H-6.1 and H-6.2):**
- In `pull_tips`, before fetching from another repository whose head repository is private:
  - Refuse when the base repository is public.
  - Otherwise require that the pull request's author still has at least `Access::Read` on the head repository. Use `access`, which caps archived repositories at Read; that is enough here.
  - If either check fails, treat the head as gone (`None`), like a deleted branch, so the pull request shows as having a missing head.
- `merge_pull` calls `pull_tips`, so it is covered too.
- The normal direction, where the head is a fork of the base, is the fork owner offering their own commits; it can stay as is. Still, make the check apply to any private head that isn't the base repository itself, rather than special-casing the direction.
- **Test:** a private parent, a fork owned by Bob, and a pull request from the parent into the fork. Remove Bob as collaborator, push to the parent: `refs/pull/N/head` in the fork must not move.

### H-7: verification checks the committer's email, not the author's. Low, by design

- True: `verify_commits` matches the signer's account against the committer's email, while the commit list shows the author.
- GitHub works the same way. Verification follows the committer, so commits a maintainer applies or cherry-picks stay verified.
- The badge tooltip already names the signer ("Signed with bob’s SSH key …").
- **Optional:** when the signer isn't the author shown, say so in the tooltip (e.g. "Committed and signed by bob"). Don't change what counts as verified.

---

## Medium

### M-1: import and LFS credentials. Low

1. **Import token.**
   - `Api::request` ([import.rs:32](crates/keel-mirror/src/import.rs#L32)) attaches the import token to every URL, including asset URLs from the API response, without comparing the URL's origin to the API base.
   - The response comes from the same server that holds the token, and reqwest drops `Authorization` on redirects to another host. So this only matters for a misconfigured or malicious source, which already has the token.
   - Cheap hardening: only attach the token when the URL's scheme, host and port match `self.base`'s. For GitHub, also allow `api.github.com`.
2. **LFS host check.**
   - `lfs::request` ([lfs.rs:77](crates/keel-mirror/src/lfs.rs#L77)) compares only the host, so an `https` remote sending an `http` action URL on the same host gets Basic credentials over plain HTTP.
   - Only the remote itself can cause this. Still, compare scheme, host and port (`port_or_known_default()`).

### M-2: email change without the password. Medium, mostly true

**True:**
- `POST /settings/account/email` ([settings.rs:84](crates/keel-web/src/settings.rs#L84)) changes the email without the current password, and a password reset can then go to the new address.
- So someone with a hijacked session can take over an account that has no 2FA. On accounts with 2FA, the reset still needs the second factor.

**Harmless; don't fix:**
- The reset sets a password on disabled accounts, but sessions of disabled accounts are rejected (`session_user` filters `disabled_at IS NULL`).
- Sending a reset to an unverified address is also fine, because using the link proves the inbox is yours.

**Fix:**
- Add a `password` field to the email form (template `settings/account` or wherever the form lives) and check it in `change_email`, the way the password form checks `current`.
- Look at which other account actions skip the password (disabling 2FA, creating tokens, adding SSH or GPG keys). Treat them the same way where it's cheap.
- **Test:** changing the email with a wrong password is refused.

### M-3: open redirect through a tab character. Medium

**1. Confirmed, reproduced.**
- `safe_next` ([session.rs:103](crates/keel-web/src/session.rs#L103)) lets `/\t/evil.example` through.
- `POST /login` with `next=/%09/evil.example` answered `Location: /\t/evil.example`. Browsers drop tabs and newlines from URLs, which makes that `//evil.example`.
- It affects `/login`, `/login/2fa` and `/theme` (which uses the `Referer`).

**2. False.**
- In axum 0.8.9, `Redirect::into_response` turns an invalid header value into a 500; it doesn't panic. A non-ASCII `next` was reproduced as a 500.
- `milestones::set_state` doesn't redirect to anything the user sends.

**Fix:**
- In `safe_next`, refuse any value containing control characters (`char::is_control`, which covers `\t`, `\r`, `\n` and `0x7f`), spaces, or a `\` anywhere.
- Percent-encode non-ASCII characters rather than rejecting them, so `next` can still point at repositories with non-ASCII names and doesn't cause the 500.
- Keep the existing `/`, `//` and `/\` rules.
- **Tests:** add cases to the `safe_next` tests: `"/\t/evil.example"`, `"/\n/evil.example"` and `"/ /x"` give `/`; `"/ada/é"` gives a percent-encoded path.

### M-4: gzip expansion holds up to 512 MiB per request. Medium, confirmed

**Problem:**
- `githttp::rpc` ([githttp.rs:239](crates/keel-web/src/githttp.rs#L239)) buffers up to 64 MiB of compressed body.
- It then decompresses up to 8× that, 512 MiB, into one `Vec`.
- No sign-in is needed for `git-upload-pack` on public repositories, and nothing limits how many requests run at once. A few ~500 KB gzip bodies of zeros use gigabytes of memory.

**Fix:**
- Decompress as a stream into git's stdin instead of buffering. Two options:
  - `async-compression` (tokio `GzipDecoder` over a `StreamReader` of the body), or
  - flate2 in a blocking task fed through a bounded channel.
- Cap the decompressed total at something like 64 MiB; negotiation requests are small.
- Optionally add a semaphore in `Core` limiting how many git processes run at once, shared with SSH.
- **Test:** a gzip body that decompresses past the cap is refused with a 4xx and memory stays bounded.

### M-5: GPG verification gaps. Low

**True:**
- Revoked subkeys, expired subkeys and subkeys without the signing flag are accepted:
  - `parse_gpg_key` ([signatures.rs:53](crates/keel-core/src/signatures.rs#L53)) only rejects revocations of the primary key, and computes expiry from user ID signatures only.
  - `check_gpg` uses any subkey.
- The email addresses in the key aren't compared with the commit's email. GitHub does require that match.
- Disabled accounts aren't filtered out in the `ssh_owners` and `gpg_keys` lookups, or in `commit_authors` ([insights.rs:129](crates/keel-core/src/insights.rs#L129)).

**Inherent or by design; don't fix:**
- A PGP signature's creation time is chosen by the signer, so a backdated signature can't be detected. GitHub has the same limit.
- Trusting unverified emails when the server can't send mail is deliberate and documented (`commit_authors`).

**Fix:**
- When parsing keys, skip subkeys that have a revocation signature, are past their binding signature's key expiration time, or lack the "sign" key flag. Don't record their IDs.
- When checking, report a signature from such a subkey as `invalid` (or `expired_key` when expired).
- When the committer's email isn't among the key's emails, report `bad_email`.
- Add `AND u.disabled_at IS NULL` to the three lookups.
- **Tests:** extend the `verifying` test, which needs `gpg` and is skipped without it. Cover a revoked subkey and a key whose UIDs don't include the committer's email.

### M-6: login hardening. Medium, confirmed

1. **No rate limiting.** There is none on password sign-in: `POST /login`, HTTP Basic with a password, and API Basic auth. Grep finds no limiter anywhere. Tokens are random and long, so only passwords need it.
2. **Failures are forgotten early.**
   - `start_login_challenge` ([totp.rs:229](crates/keel-core/src/totp.rs#L229)) deletes challenges once they expire (600 s).
   - The lockout sums attempts over 900 s (`LOCK_SECS`), so failures vanish after 600 s instead of 900 s.
3. **The attempt counter can be raced.**
   - `complete_login_challenge` reads `attempts`, verifies the code, and only then increments.
   - N parallel requests with one challenge token get N guesses, past both `CHALLENGE_ATTEMPTS` and `MAX_FAILURES`.
4. **Password changes keep pending challenges.** `set_password` ([users.rs:169](crates/keel-core/src/users.rs#L169)) deletes sessions but not `login_challenges`, so a pending 2FA challenge works after a password change.

Not a bug: `/login` revealing that the password was right by moving on to the 2FA step. Every 2FA flow does that.

**Fix:**
1. **Rate limiting:**
   - Add a small in-memory failure counter in `Core`, keyed by lowercased login name. For example, after 10 failures within 15 minutes, refuse with "Too many failed attempts; try again in N minutes" without checking the password.
   - Count only wrong passwords, and clear the counter on success.
   - Use it in `authenticate` so the web form, Basic auth and the API all go through it.
   - A per-IP limit is optional; behind a reverse proxy the peer address is the proxy's.
2. **Keep failures long enough:** in `start_login_challenge`, delete only rows with `created_at < now - LOCK_SECS`. The checks for a valid challenge already require `expires_at > now`.
3. **Atomic attempts:** before checking the code, run `UPDATE login_challenges SET attempts = attempts + 1 WHERE token_hash = ?1 AND expires_at > ?2 AND attempts < ?3 RETURNING user_id`. No row means expired or used up. Then check the lockout sum, then the code. On success, delete the challenge as now. With this, `attempts` counts tries rather than failures, so the lockout sum would count the successful try too; either subtract it or accept it.
4. **Password changes:** in `set_password`'s transaction, also `DELETE FROM login_challenges WHERE user_id = ?1`.
- **Tests:**
  - After two challenges of 5 wrong codes each, a new challenge started 601 s later is still locked. Insert rows with old timestamps directly rather than sleeping.
  - Changing the password invalidates a pending challenge.
  - The limiter refuses the 11th wrong password.

### M-7: the CSRF check allows requests without Origin or Referer. Low

- True as described: `same_origin` ([lib.rs:508](crates/keel-web/src/lib.rs#L508)) passes requests that have neither header, and its fallback ignores the scheme.
- But the session cookie is `SameSite=Lax`, so browsers don't send it on cross-site POSTs, and modern browsers always send `Origin` on POSTs.
- The http-vs-https case needs someone who can intercept traffic.
- **Optional hardening:** in the fallback, require the `Origin`'s scheme to match `base_url`'s when `base_url` is `https`. Leave the `(None, _)` arm alone; changing it would break clients that don't send the headers without adding real protection.

### M-8: SSRF filter. Low

1. **Partly true.** `is_private_ip` ([webhooks.rs:133](crates/keel-core/src/webhooks.rs#L133)) misses a few IPv6 ranges that can reach IPv4 or reserved space. They only matter on networks that route NAT64, 6to4 or Teredo. Cheap to add:
   - `::/96`, IPv4-compatible: check the embedded IPv4 address.
   - `64:ff9b::/96` and `64:ff9b:1::/48`, NAT64: check the embedded IPv4 address for `/96`.
   - `2002::/16`, 6to4: check the IPv4 address in segments 1–2.
   - `2001::/32`, Teredo.
   - `2001:db8::/32`, documentation.
   - `fec0::/10`, old site-local.
   - Test each one in the existing `is_private_ip` tests.
2. **False as a security issue.**
   - `parse_mirror_url` ([mirrors.rs:101](crates/keel-core/src/mirrors.rs#L101)) strips the brackets from IPv6 hosts, so `http.curloptResolve` ([mirror.rs:38](crates/keel-git/src/mirror.rs#L38)) gets an unbracketed host. That may break mirrors of `https://[v6]/…` URLs.
   - An IP literal can't be re-resolved to a different address, so the pin isn't needed there anyway.
   - Functional fix: skip the pin when the host is an IP literal.

### M-9: SSH resource limits. Low

- **Channels.** Nothing limits channels per connection. russh 0.64's `Config` has no `max_channels`, so the limit has to be counted in `Connection` and enforced in `channel_open_session`, e.g. 10 per connection.
- **Repeated exec.** A second `exec_request` on a channel that already runs git starts another git. Refuse it.
- **`GIT_PROTOCOL` values.** `env_request` stores any value. SSH packets cap it at 32 KB per channel, and `git_command` drops invalid values later. Refusing values of 256 bytes or more in `env_request` is still cheap.
- **Killing git on disconnect.** After a disconnect, the waiting task keeps the child, so `kill_on_drop` doesn't fire. Git usually dies on its next write to the closed pipe. Optional: kill the child when the channel closes.
- **Not a bug:** probing whether a public key is registered is standard SSH behavior.
- **Not a bug:** `user_by_ssh_fingerprint` updating `last_used_at` before filtering disabled users is harmless.
- You have to be signed in for all of this.

### M-10: what users see after losing access. Low

1. **Confirmed.**
   - `notifications()` and `unread_notifications()` ([notifications.rs:253](crates/keel-core/src/notifications.rs#L253)) join issues without checking access.
   - After a collaborator or org member is removed, their inbox keeps showing the live titles and states of that repository's issues.
   - Fix: filter rows by `self.access(repo, user) != Access::None`, computed once per repository in the result. For the unread count, a SQL filter on public repositories, owner, collaborators and org membership is the efficient option; otherwise filter per repository.
   - Optionally delete the user's notifications and watches for that repository when they are removed.
2. **Minor.** `pull_json` ([activity.rs:528](crates/keel-core/src/activity.rs#L528)) includes the head repository's metadata (name, description, visibility) in webhook payloads. Only the base repository's webhook owners receive it, and they can already see the pull request. Optional: leave out `head.repo` when the head repository is private and the base isn't.

---

## Low and hardening

### L-1: git command hardening

- **Missing `--`.**
  - `store::fork` and `store::backup` don't pass `--` before the paths. Those paths are built by keel or given by the operator, so nobody can inject options. Adding `--` is harmless.
  - `pre_receive` passes `old` and `new` from stdin to `git merge-base`. Those values come from git itself, not from the pusher, so this part of the report doesn't apply.
- **`~/.gitconfig`.** Several commands don't set `GIT_CONFIG_GLOBAL=/dev/null`, so the service account's `~/.gitconfig` applies. Hardening only; fold it into the shared environment helper from H-5. `transport::git_command` passes `HOME` on purpose; check why before removing anything.

### L-2: backups

- **File permissions.** True: `keel backup` creates its directory and `keel.db` with the default umask, the same as the live data directory. The database holds secrets: TOTP secrets, webhook secrets, mirror passwords, import tokens. Create the backup directory `0700` and the database file `0600` on Unix. Consider the same for a new data directory in `Core::open`.
- **Symlinks.** `copy_files` ([backup.rs:100](crates/keel-core/src/backup.rs#L100)) follows symlinks. Keel never creates any there, so it only matters if someone else wrote to the data directory. Skip symlinks (`file_type().is_symlink()`).
- **Wrong claim:** that tokens of queued imports should be cleared. Queued imports still have to run after a restart and need the token. Only running ones are cleared, and that's correct.
- **`lfs_path`.** Every caller already validates the OID ([keel-web lfs.rs](crates/keel-web/src/lfs.rs) with `valid_oid`, and mirror pointers via `parse_lfs_pointer`). At most add a `debug_assert!(valid_oid(oid))`.

### L-3: search and uploads

- **Search queries.** True: they have no length limit, and anyone can search public repositories. Each distinct trigram becomes a `Must` clause. Cap the query at e.g. 256 characters in the search handler ([search.rs](crates/keel-web/src/search.rs)) and explain the limit on the page.
- **Release uploads.** One request can carry up to `MAX_ASSETS` files. That's no worse than separate uploads; keel simply has no per-user disk quota. Leave it.
