#!/usr/bin/env bash
# End-to-end smoke test: starts keel in a temporary directory and drives it
# with real git, ssh and curl. Usage: scripts/smoke-test.sh [path/to/keel]
# Set SMOKE_KEEP=<dir> to keep the resulting data directory for inspection.
set -uo pipefail

BIN=${1:-target/debug/keel}
BIN=$(realpath "$BIN")
T=$(mktemp -d)
HTTP_PORT=$((20000 + RANDOM % 10000))
SSH_PORT=$((HTTP_PORT + 1))
B=http://127.0.0.1:$HTTP_PORT
O="Origin: $B"
FAILED=0

cleanup() {
    [ -n "${PID:-}" ] && kill "$PID" 2>/dev/null && wait "$PID" 2>/dev/null
    [ -n "${SMTP_PID:-}" ] && kill "$SMTP_PID" 2>/dev/null
    if [ -n "${SMOKE_KEEP:-}" ]; then
        rm -rf "$SMOKE_KEEP" && cp -r "$T/data" "$SMOKE_KEEP"
    fi
    rm -rf "$T"
}
trap cleanup EXIT

check() { # check <description> <command...>
    local desc=$1
    shift
    if "$@" >"$T/out" 2>&1; then
        echo "PASS  $desc"
    else
        echo "FAIL  $desc"
        sed 's/^/      /' "$T/out" | tail -5
        FAILED=1
    fi
}
status() { curl -s -o /dev/null -w '%{http_code}' "$@"; }
is() { [ "$1" = "$2" ] || { echo "expected '$2', got '$1'"; return 1; }; }
contains() { grep -q -- "$2" <<<"$1" || { echo "missing '$2'"; return 1; }; }

export GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_NOSYSTEM=1 GIT_TERMINAL_PROMPT=0
export GIT_AUTHOR_NAME=Ada GIT_AUTHOR_EMAIL=ada@example.com
export GIT_COMMITTER_NAME=Ada GIT_COMMITTER_EMAIL=ada@example.com
ssh-keygen -q -t ed25519 -N "" -C ada@test -f "$T/key"
export GIT_SSH_COMMAND="ssh -i $T/key -o IdentitiesOnly=yes -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR"

# An SMTP server that saves each message to $T/mail/<n>.eml.
SMTP_PORT=$((HTTP_PORT + 3))
mkdir -p "$T/mail"
python3 - "$SMTP_PORT" "$T/mail" <<'PY' &
import email, socketserver, sys, itertools
port, out = int(sys.argv[1]), sys.argv[2]
n = itertools.count()
class H(socketserver.StreamRequestHandler):
    def handle(self):
        say = lambda s: self.wfile.write(s.encode() + b"\r\n")
        say("220 smoke ESMTP")
        while line := self.rfile.readline().decode():
            cmd = line.strip().upper()
            if cmd.startswith("DATA"):
                say("354 go ahead")
                data = []
                while (l := self.rfile.readline().decode()).rstrip("\r\n") != ".":
                    data.append(l)
                # Save headers and the decoded body (lines may be quoted-printable).
                msg = email.message_from_string("".join(data))
                text = "".join(f"{k}: {v}\n" for k, v in msg.items()) + "\n"
                text += msg.get_payload(decode=True).decode()
                open(f"{out}/{next(n)}.eml", "w").write(text)
                say("250 queued")
            elif cmd.startswith("QUIT"):
                say("221 bye"); return
            else:
                say("250 ok")
socketserver.ThreadingTCPServer.allow_reuse_address = True
socketserver.ThreadingTCPServer(("127.0.0.1", port), H).serve_forever()
PY
SMTP_PID=$!
for _ in $(seq 1 50); do (exec 3<>/dev/tcp/127.0.0.1/$SMTP_PORT) 2>/dev/null && break; sleep 0.1; done

# A relative --data-dir, as by default: paths derived from it (such as the
# hooks that enforce branch protection) must still work.
(cd "$T" && exec "$BIN" serve --data-dir data --http-addr "127.0.0.1:$HTTP_PORT" \
    --ssh-addr "127.0.0.1:$SSH_PORT" --base-url "$B" --webhook-allow-private \
    --smtp-url "smtp://127.0.0.1:$SMTP_PORT" --mail-from "keel <git@example.com>" \
    --metrics-token smoke-metrics >"$T/server.log" 2>&1) &
PID=$!
for _ in $(seq 1 100); do curl -s -o /dev/null "$B/" && break; sleep 0.1; done

signup() { # signup <user> <cookie jar>
    curl -s -c "$2" -o /dev/null -H "$O" -d "username=$1&email=$1@example.com&password=password-$1" "$B/register"
}
post() { # post <jar> <path> <data...>
    local jar=$1 path=$2
    shift 2
    curl -s -b "$jar" -c "$jar" -o /dev/null -w '%{http_code}' -H "$O" "${@/#/--data-urlencode=}" "$B$path"
}

echo "== accounts"
signup ada "$T/ada"
signup bob "$T/bob"
check "signing in redirects to local paths only" is "$(curl -s -o /dev/null -w '%{redirect_url}' -H "$O" -d "login=bob&password=password-bob&next=/%09/evil.example" "$B/login")" "$B/"
check "non-ASCII ones are encoded" is "$(curl -s -o /dev/null -w '%{redirect_url}' -H "$O" --data-urlencode "login=bob" --data-urlencode "password=password-bob" --data-urlencode "next=/ada/é" "$B/login")" "$B/ada/%C3%A9"
check "cross-origin POST is rejected" is "$(curl -s -o /dev/null -w '%{http_code}' -X POST -H 'Origin: http://evil.example' -d x=1 "$B/new")" 403
check "create public repo" is "$(post "$T/ada" /new name=engine)" 303
check "create private repo" is "$(post "$T/ada" /new name=secret private=on)" 303
check "private repo is hidden" is "$(status -b "$T/bob" "$B/ada/secret")" 404

echo "== git over HTTP"
W=$T/work
git init -q -b main "$W"
mkdir -p "$W/src" && printf 'fn main() {}\n' >"$W/src/main.rs" && printf '# Engine\n' >"$W/README.md"
git -C "$W" add -A && git -C "$W" commit -qm "Initial commit"
check "push with password" git -C "$W" push -q "http://ada:password-ada@127.0.0.1:$HTTP_PORT/ada/engine.git" main
check "anonymous clone" git clone -q "$B/ada/engine.git" "$T/c1"
check "anonymous push is refused" bash -c "! git -C '$W' push -q '$B/ada/engine.git' main"
python3 -c 'import gzip, sys; sys.stdout.buffer.write(gzip.compress(b"0" * (65 << 20)))' >"$T/bomb.gz"
check "oversized gzip requests are refused" is "$(status -H 'Content-Encoding: gzip' -H 'Content-Type: application/x-git-upload-pack-request' --data-binary "@$T/bomb.gz" "$B/ada/engine.git/git-upload-pack")" 413
check "small ones aren't" is "$(printf 0000 | gzip | status -H 'Content-Encoding: gzip' -H 'Content-Type: application/x-git-upload-pack-request' --data-binary @- "$B/ada/engine.git/git-upload-pack")" 200
check "bob cannot push" bash -c "! git -C '$W' push -q 'http://bob:password-bob@127.0.0.1:$HTTP_PORT/ada/engine.git' main"

echo "== access tokens"
page=$(curl -s -b "$T/ada" -H "$O" --data-urlencode "name=ci" --data-urlencode "expires=30" --data-urlencode "password=password-ada" "$B/settings/tokens")
RO=$(grep -o 'value="keel_[A-Za-z0-9_-]*"' <<<"$page" | cut -d'"' -f2)
page=$(curl -s -b "$T/ada" -H "$O" --data-urlencode "name=laptop" --data-urlencode "write=on" --data-urlencode "password=password-ada" "$B/settings/tokens")
RW=$(grep -o 'value="keel_[A-Za-z0-9_-]*"' <<<"$page" | cut -d'"' -f2)
check "creating tokens needs the password" contains "$(curl -s -b "$T/ada" -H "$O" -d name=x "$B/settings/tokens")" "password is incorrect"
check "so does changing the email" contains "$(curl -s -b "$T/ada" -H "$O" -d email=evil@example.com -d password=wrong "$B/settings/account/email")" "password is incorrect"
check "tokens are shown once" bash -c "[ -n '$RO' ] && [ -n '$RW' ] && ! curl -s -b '$T/ada' '$B/settings/tokens' | grep -q '$RW'"
check "read-only token clones private repo" git clone -q "http://ada:$RO@127.0.0.1:$HTTP_PORT/ada/secret.git" "$T/c2"
echo change >>"$W/README.md" && git -C "$W" commit -qam "Second"
check "read-only token cannot push" bash -c "! git -C '$W' push -q 'http://ada:$RO@127.0.0.1:$HTTP_PORT/ada/engine.git' main"
check "write token pushes (token as username)" git -C "$W" push -q "http://$RW:x@127.0.0.1:$HTTP_PORT/ada/engine.git" main
check "wrong token is refused" bash -c "! git ls-remote -q 'http://ada:keel_nope@127.0.0.1:$HTTP_PORT/ada/secret.git'"

echo "== git over SSH"
check "adding SSH keys needs the password" contains "$(curl -s -b "$T/ada" -H "$O" --data-urlencode "key=$(cat "$T/key.pub")" --data-urlencode password=wrong "$B/settings/keys")" "password is incorrect"
check "add SSH key" is "$(post "$T/ada" /settings/keys "key=$(cat "$T/key.pub")" password=password-ada)" 303
echo ssh >>"$W/README.md" && git -C "$W" commit -qam "Over SSH"
check "push over SSH" git -C "$W" push -q "ssh://git@127.0.0.1:$SSH_PORT/ada/engine.git" main
check "clone over SSH" git clone -q "ssh://git@127.0.0.1:$SSH_PORT/ada/engine.git" "$T/c3"

echo "== collaborators"
signup eve "$T/eve"
check "bob can't see ada/secret" is "$(status -b "$T/bob" "$B/ada/secret")" 404
check "bob can't open settings" is "$(status -b "$T/bob" "$B/ada/engine/settings")" 403
check "add bob as reader" is "$(post "$T/ada" /ada/secret/collaborators username=bob role=read)" 303
check "unknown collaborator is rejected" contains "$(curl -s -b "$T/ada" -H "$O" -d "username=nobody&role=read" "$B/ada/secret/collaborators")" "no user named"
check "reader sees private repo" is "$(status -b "$T/bob" "$B/ada/secret")" 200
check "reader clones private repo" git clone -q "http://bob:password-bob@127.0.0.1:$HTTP_PORT/ada/secret.git" "$T/c4"
check "reader can't push" bash -c "! git -C '$W' push -q 'http://bob:password-bob@127.0.0.1:$HTTP_PORT/ada/secret.git' main"
check "make bob a writer" is "$(post "$T/ada" /ada/secret/collaborators username=bob role=write)" 303
check "writer pushes" git -C "$W" push -q "http://bob:password-bob@127.0.0.1:$HTTP_PORT/ada/secret.git" main
check "writer can't open settings" is "$(status -b "$T/bob" "$B/ada/secret/settings")" 403
check "eve still can't see it" is "$(status -b "$T/eve" "$B/ada/secret")" 404

echo "== organizations"
check "create org" is "$(post "$T/ada" /org/new name=acme)" 303
check "org page" contains "$(curl -s "$B/acme")" "Organization"
check "org name is taken for users" contains "$(curl -s -H "$O" -d "username=acme&email=x@example.com&password=password-x" "$B/register")" "taken"
check "org can't sign in" contains "$(curl -s -H "$O" -d "login=acme&password=" "$B/login")" "Incorrect"
check "create private org repo" is "$(post "$T/ada" /new owner=acme name=tool private=on)" 303
check "non-member can't create org repos" is "$(post "$T/eve" /new owner=acme name=evil)" 403
check "non-member can't see private org repo" is "$(status -b "$T/eve" "$B/acme/tool")" 404
check "add eve as member" is "$(post "$T/ada" /org/acme/members username=eve role=member)" 303
check "member sees org repo" is "$(status -b "$T/eve" "$B/acme/tool")" 200
check "member pushes to org repo" git -C "$W" push -q "http://eve:password-eve@127.0.0.1:$HTTP_PORT/acme/tool.git" main
check "member can't administer org" is "$(status -b "$T/eve" "$B/org/acme/settings")" 403
check "member can't change repo settings" is "$(status -b "$T/eve" "$B/acme/tool/settings")" 403
check "last owner can't leave" contains "$(curl -s -b "$T/ada" -H "$O" -X POST "$B/org/acme/members/1/remove")" "at least one owner"
check "org with repos can't be deleted" contains "$(curl -s -b "$T/ada" -H "$O" -X POST "$B/org/acme/delete")" "repositories first"
check "home lists org repos for members" contains "$(curl -s -b "$T/eve" "$B/")" "acme/<strong>tool"

echo "== issues"
check "anonymous can't open issues" is "$(status "$B/ada/engine/issues/new")" 303
check "bob opens an issue" is "$(post "$T/bob" /ada/engine/issues title="It crashes" body="Steps: run it. cc @ada")" 303
check "issue page" contains "$(curl -s "$B/ada/engine/issues/1")" "It crashes"
check "mentions are linked" contains "$(curl -s "$B/ada/engine/issues/1")" 'href="/ada"'
check "ada comments referencing it" is "$(post "$T/ada" /ada/engine/issues/1/comments body="Duplicate of #1?" action=comment)" 303
check "references are linked" contains "$(curl -s "$B/ada/engine/issues/1")" 'href="/ada/engine/issues/1"'
check "eve can't close bob's issue" is "$(post "$T/eve" /ada/engine/issues/1/comments action=close)" 403
check "eve can't edit it" is "$(status -b "$T/eve" "$B/ada/engine/issues/1/edit")" 403
check "author closes with a comment" is "$(post "$T/bob" /ada/engine/issues/1/comments body=Fixed action=close)" 303
check "closed issue is listed as closed" contains "$(curl -s "$B/ada/engine/issues?state=closed")" "It crashes"
check "timeline shows the close event" contains "$(curl -s "$B/ada/engine/issues/1")" "closed this"
check "owner reopens" is "$(post "$T/ada" /ada/engine/issues/1/comments action=reopen)" 303
check "author edits title" is "$(post "$T/bob" /ada/engine/issues/1/edit title="It crashes on start" body=Steps)" 303
check "open count in tab" contains "$(curl -s "$B/ada/engine")" '<span class="count">1</span>'
check "issues of private repos stay hidden" is "$(status -b "$T/eve" "$B/ada/secret/issues")" 404

echo "== pull requests"
ADA_HTTP="http://ada:password-ada@127.0.0.1:$HTTP_PORT/ada/engine.git"
git -C "$W" pull -q "$ADA_HTTP" main
git -C "$W" checkout -q -b feature/fast
echo "fast" >"$W/fast.txt" && git -C "$W" add -A && git -C "$W" commit -qm "Add fast path"
echo "faster" >>"$W/fast.txt" && git -C "$W" commit -qam "Make it faster"
git -C "$W" push -q "$ADA_HTTP" feature/fast
check "compare shows commits" contains "$(curl -s "$B/ada/engine/compare/main...feature/fast")" "Make it faster"
check "bob opens a pull request" is "$(post "$T/bob" /ada/engine/pulls base=main head=feature/fast title="Fast path" body="Speeds things up")" 303
PR=$(curl -s "$B/ada/engine/pulls" | grep -o 'href="/ada/engine/pull/[0-9]*"' | head -1 | grep -o '[0-9]*')
check "pull request page" contains "$(curl -s "$B/ada/engine/pull/$PR")" "wants to merge 2 commits"
check "files tab" contains "$(curl -s "$B/ada/engine/pull/$PR/files")" "fast.txt"
check "duplicate pull request is refused" contains "$(curl -s -b "$T/bob" -H "$O" -d "base=main&head=feature/fast&title=x" "$B/ada/engine/pulls")" "already open"
check "issue URL redirects to pull" is "$(status "$B/ada/engine/issues/$PR")" 303
check "pull ref is fetchable" git -C "$T/c1" fetch -q origin "refs/pull/$PR/head"
check "pushing to pull refs is refused" bash -c "! git -C '$W' push -q '$ADA_HTTP' HEAD:refs/pull/$PR/head"
git -C "$W" replace "$(git -C "$W" rev-parse HEAD~1)" HEAD
check "pushing replace refs is refused" bash -c "! git -C '$W' push -q '$ADA_HTTP' 'refs/replace/*:refs/replace/*'"
git -C "$W" replace -d "$(git -C "$W" rev-parse HEAD~1)"
check "merge box offers merging" contains "$(curl -s -b "$T/ada" "$B/ada/engine/pull/$PR")" "no conflicts"
check "bob can't merge" is "$(post "$T/bob" /ada/engine/pull/$PR/merge style=merge)" 403
check "ada squash-merges" is "$(post "$T/ada" /ada/engine/pull/$PR/merge style=squash delete_branch=on)" 303
check "pull request is merged" contains "$(curl -s "$B/ada/engine/pull/$PR")" "merged 2 commits into"
check "main has the change" is "$(curl -s "$B/ada/engine/raw/main/fast.txt" | tail -1)" faster
check "branch was deleted" is "$(git ls-remote "$B/ada/engine.git" refs/heads/feature/fast | wc -l)" 0
check "merged files tab still works" contains "$(curl -s "$B/ada/engine/pull/$PR/files")" "fast.txt"

git -C "$W" checkout -q main && git -C "$W" pull -q "$ADA_HTTP" main
git -C "$W" checkout -q -b conflict && echo "theirs" >"$W/README.md" && git -C "$W" commit -qam "Rewrite readme"
git -C "$W" push -q "$ADA_HTTP" conflict
git -C "$W" checkout -q main && echo "ours" >"$W/README.md" && git -C "$W" commit -qam "Also rewrite readme"
git -C "$W" push -q "$ADA_HTTP" main
check "open conflicting pull request" is "$(post "$T/ada" /ada/engine/pulls base=main head=conflict title=Conflict)" 303
PR2=$((PR + 1))
check "conflicts are reported" contains "$(curl -s -b "$T/ada" "$B/ada/engine/pull/$PR2")" "README.md"
check "conflicting merge is refused" contains "$(curl -s -b "$T/ada" -H "$O" -d style=merge "$B/ada/engine/pull/$PR2/merge")" "Merging failed"

git -C "$W" checkout -q -b manual && echo "manual" >"$W/manual.txt" && git -C "$W" add -A && git -C "$W" commit -qm "Manual"
git -C "$W" push -q "$ADA_HTTP" manual
check "open pull request to merge by hand" is "$(post "$T/ada" /ada/engine/pulls base=main head=manual title=Manual)" 303
PR3=$((PR + 2))
git -C "$W" push -q "$ADA_HTTP" manual:main
merged() { for _ in $(seq 1 50); do curl -s "$B/ada/engine/pull/$PR3" | grep -q "merged 1 commit" && return 0; sleep 0.1; done; return 1; }
check "merging with git push is detected" merged

echo "== code search"
eventually_contains() { # <url> <text>: retries for up to 5s (indexing is asynchronous)
    for _ in $(seq 1 50); do curl -s "$1" | grep -q -- "$2" && return 0; sleep 0.1; done
    echo "missing '$2' in $1"
    return 1
}
check "repo search finds content" eventually_contains "$B/ada/engine/search?q=faster" "fast.txt"
check "matches are highlighted" contains "$(curl -s "$B/ada/engine/search?q=faster")" "<mark>faster</mark>"
check "search is case-insensitive" contains "$(curl -s "$B/ada/engine/search?q=FN+MAIN")" "src/main.rs"
check "global search finds public code" contains "$(curl -s -b "$T/eve" "$B/search?q=fn+main")" "ada/engine"
check "long searches are refused" contains "$(curl -s "$B/search?q=$(printf 'abc%.0s' $(seq 100))")" "at most 256 characters"
check "global search hides private repos" bash -c "! curl -s -b '$T/eve' '$B/search?q=fn+main' | grep -q 'ada/secret'"
check "collaborators find private code" contains "$(curl -s -b "$T/bob" "$B/search?q=fn+main")" "ada/secret"
check "short queries are explained" contains "$(curl -s "$B/search?q=fn")" "at least 3"
git -C "$W" checkout -q -b searchable main
printf 'let zebra_marker = 42;\n' >"$W/zoo.rs" && git -C "$W" add -A && git -C "$W" commit -qm "Add zoo"
git -C "$W" push -q "$ADA_HTTP" searchable && git -C "$W" checkout -q main
check "other branches are searchable" contains "$(curl -s "$B/ada/engine/search?q=zebra_marker&ref=searchable")" "zoo.rs"
check "the default branch doesn't have it" contains "$(curl -s "$B/ada/engine/search?q=zebra_marker")" "No code matches"
check "and still finds the rest on the branch" contains "$(curl -s "$B/ada/engine/search?q=fn+main&ref=searchable")" "src/main.rs"
check "regular expressions" contains "$(curl -s "$B/ada/engine/search?q=zebra_%5Cw%2B+%3D+%5Cd%2B&regex=1&ref=searchable")" "<mark>zebra_marker = 42</mark>"
check "written /like this/" contains "$(curl -s "$B/ada/engine/search?q=%2Ffn%5Cs%2Bmain%2F")" "src/main.rs"
check "without literal text in one repository" contains "$(curl -s "$B/ada/engine/search?q=%5Cd%7B2%7D&regex=1&ref=searchable")" "zoo.rs"
check "invalid expressions are explained" contains "$(curl -s "$B/ada/engine/search?q=(&regex=1")" "valid regular expression"
check "global regular expressions" contains "$(curl -s "$B/search?q=fn%5Cs%2Bmain&regex=1")" "src/main.rs"
check "need literal text" contains "$(curl -s "$B/search?q=%5Cd%2B&regex=1")" "literal text"

echo "== forks"
check "bob forks ada/engine" is "$(post "$T/bob" /ada/engine/fork owner=bob name=engine)" 303
check "fork names its parent" contains "$(curl -s "$B/bob/engine")" "forked from"
check "forks are listed" contains "$(curl -s "$B/ada/engine/forks")" "bob/<strong>engine"
check "can't fork into the same account" contains "$(curl -s -b "$T/bob" -H "$O" -d "owner=bob&name=engine2" "$B/bob/engine/fork")" "forked into the account"
check "fork a private repo" is "$(post "$T/bob" /ada/secret/fork owner=bob name=secret)" 303
check "forks of private repos are private" is "$(status -b "$T/eve" "$B/bob/secret")" 404
check "and stay private" contains "$(curl -s -b "$T/bob" -H "$O" -d "description=" "$B/bob/secret/settings")" "stay private"
BOB_FORK="http://bob:password-bob@127.0.0.1:$HTTP_PORT/bob/engine.git"
BOB_GIT="git -C $T/fork -c user.name=Bob -c user.email=bob@example.com"
check "clone the fork" git clone -q "$BOB_FORK" "$T/fork"
echo "from bob" >"$T/fork/bob.txt" && $BOB_GIT add -A && $BOB_GIT commit -qm "Add bob.txt"
check "push a branch to the fork" $BOB_GIT push -q origin HEAD:topic
check "compare across forks" contains "$(curl -s "$B/ada/engine/compare/main...bob/engine:topic")" "Add bob.txt"
check "compare form picks the fork" is "$(curl -s -o /dev/null -w '%{redirect_url}' "$B/ada/engine/compare?base=main&head_repo=bob/engine&head=topic")" "$B/ada/engine/compare/main...bob/engine:topic"
check "open a pull request from the fork" is "$(post "$T/bob" /ada/engine/pulls base=main head=bob/engine:topic title="From the fork")" 303
FPR=$(curl -s "$B/ada/engine/pulls" | grep -o 'href="/ada/engine/pull/[0-9]*"' | head -1 | grep -o '[0-9]*')
check "pull request names the fork's branch" contains "$(curl -s "$B/ada/engine/pull/$FPR")" "bob:topic"
echo "more" >>"$T/fork/bob.txt" && $BOB_GIT commit -qam "More from bob"
$BOB_GIT push -q origin HEAD:topic
check "pushing to the fork updates the pull request" eventually_contains "$B/ada/engine/pull/$FPR/commits" "More from bob"
check "the fork's commits are fetchable from the base" git -C "$T/c1" fetch -q origin "refs/pull/$FPR/head"

echo "== labels and assignees"
check "repos start with labels" contains "$(curl -s "$B/ada/engine/labels")" "good first issue"
check "readers can't create labels" is "$(post "$T/bob" /ada/engine/labels name=perf color=orange)" 403
check "create a label" is "$(post "$T/ada" /ada/engine/labels name=perf color=orange description=Speed)" 303
check "unknown colors are rejected" contains "$(curl -s -b "$T/ada" -H "$O" -d "name=x&color=chartreuse" "$B/ada/engine/labels")" "Choose one of the colors"
LBUG=$(curl -s -b "$T/ada" "$B/ada/engine/labels" | grep -o 'labels/[0-9]*/delete' | head -1 | grep -o '[0-9]*')
check "readers can't label issues" is "$(post "$T/bob" /ada/engine/issues/1/labels "label=$LBUG")" 403
check "label an issue" is "$(post "$T/ada" /ada/engine/issues/1/labels "label=$LBUG")" 303
check "label shows on the issue" contains "$(curl -s "$B/ada/engine/issues/1")" 'class="label label-red"'
check "labeling is in the timeline" contains "$(curl -s "$B/ada/engine/issues/1")" "added"
check "filter by label" contains "$(curl -s "$B/ada/engine/issues?state=open&label=bug")" "It crashes on start"
check "filter leaves others out" bash -c "! curl -s '$B/ada/engine/issues?label=question' | grep -q 'It crashes'"
ADA_ID=$(curl -s -b "$T/ada" "$B/ada/engine/issues/1" | grep -o 'name="user" value="[0-9]*"' | head -1 | grep -o '[0-9]*')
check "ada self-assigns" is "$(post "$T/ada" /ada/engine/issues/1/assignees "user=$ADA_ID")" 303
check "assignment is in the timeline" contains "$(curl -s "$B/ada/engine/issues/1")" "self-assigned this"
check "filter by assignee" contains "$(curl -s "$B/ada/engine/issues?assignee=ada")" "It crashes on start"

echo "== code review"
FILES=$(curl -s -b "$T/ada" "$B/ada/engine/pull/$FPR/files")
HEAD=$(grep -o 'name="head" value="[0-9a-f]*"' <<<"$FILES" | head -1 | cut -d'"' -f4)
check "diff lines offer comments" contains "$FILES" 'class="add-comment"'
check "comment on a line" is "$(post "$T/ada" /ada/engine/pull/$FPR/comments path=bob.txt side=new line=1 "head=$HEAD" body="Why bob?")" 303
check "comment shows in the diff" contains "$(curl -s "$B/ada/engine/pull/$FPR/files")" "Why bob?"
check "and in the conversation, with context" contains "$(curl -s "$B/ada/engine/pull/$FPR")" "from bob"
check "comments on a stale diff are refused" is "$(post "$T/ada" /ada/engine/pull/$FPR/comments path=bob.txt side=new line=1 head=0000000000000000000000000000000000000000 body=x)" 400
check "comments outside the diff are refused" is "$(post "$T/ada" /ada/engine/pull/$FPR/comments path=README.md side=new line=1 "head=$HEAD" body=x)" 400
THREAD=$(curl -s "$B/ada/engine/pull/$FPR" | grep -o 'id="thread-[0-9]*"' | head -1 | grep -o '[0-9]*')
check "reply in the thread" is "$(post "$T/bob" /ada/engine/pull/$FPR/comments/$THREAD/reply body="Because." back=files)" 303
check "reply shows in the diff" contains "$(curl -s "$B/ada/engine/pull/$FPR/files")" "Because."
check "authors can't approve their own pull request" is "$(post "$T/bob" /ada/engine/pull/$FPR/review verdict=approve)" 400
check "request changes" is "$(post "$T/ada" /ada/engine/pull/$FPR/review verdict=request_changes body="Needs a test")" 303
check "review is in the conversation" contains "$(curl -s "$B/ada/engine/pull/$FPR")" "requested changes"
echo "changed" >"$T/fork/bob.txt" && $BOB_GIT commit -qam "Change bob.txt"
$BOB_GIT push -q origin HEAD:topic
check "changing the line outdates the comment" eventually_contains "$B/ada/engine/pull/$FPR" "Outdated"
check "approve" is "$(post "$T/ada" /ada/engine/pull/$FPR/review verdict=approve)" 303
check "approval shows in the merge box" contains "$(curl -s "$B/ada/engine/pull/$FPR")" "approved these changes"
check "merge the fork's pull request" is "$(post "$T/ada" /ada/engine/pull/$FPR/merge style=merge delete_branch=on)" 303
check "it is merged" contains "$(curl -s "$B/ada/engine/pull/$FPR")" "merged 3 commits into"
check "main has the fork's change" is "$(curl -s "$B/ada/engine/raw/main/bob.txt")" changed
# ada is the site admin (the first account), so may delete the fork's branch.
check "fork branch was deleted" is "$(git ls-remote "$BOB_FORK" refs/heads/topic 2>/dev/null | wc -l)" 0

echo "== webhooks"
# A receiver that saves each request's event, signature and body.
HOOK_PORT=$((HTTP_PORT + 2))
mkdir -p "$T/hooks"
python3 - "$HOOK_PORT" "$T/hooks" <<'PY' &
import http.server, sys, itertools
port, out = int(sys.argv[1]), sys.argv[2]
n = itertools.count()
class H(http.server.BaseHTTPRequestHandler):
    def do_POST(self):
        body = self.rfile.read(int(self.headers["Content-Length"]))
        i = next(n)
        open(f"{out}/{i}.json", "wb").write(body)
        open(f"{out}/{i}.hdr", "w").write(f"{self.headers['X-GitHub-Event']} {self.headers.get('X-Hub-Signature-256', '')}\n")
        self.send_response(200); self.end_headers(); self.wfile.write(b"thanks")
    def log_message(self, *a): pass
http.server.HTTPServer(("127.0.0.1", port), H).serve_forever()
PY
HOOK_PID=$!
for _ in $(seq 1 50); do curl -s -o /dev/null "http://127.0.0.1:$HOOK_PORT/" && break; sleep 0.1; done
received() { # received <event> <text>: waits up to 5s for a delivery of <event> containing <text>
    for _ in $(seq 1 50); do
        for h in "$T"/hooks/*.hdr; do
            [ -e "$h" ] || continue
            if grep -q "^$1 " "$h" && grep -q -- "$2" "${h%.hdr}.json"; then echo "${h%.hdr}"; return 0; fi
        done
        sleep 0.1
    done
    echo "no $1 delivery containing '$2'"
    return 1
}
check "readers can't manage webhooks" is "$(status -b "$T/bob" "$B/ada/engine/settings/hooks")" 403
check "add a webhook" is "$(post "$T/ada" /ada/engine/settings/hooks "url=http://127.0.0.1:$HOOK_PORT/hook" secret=s3cret which=all active=on)" 303
check "it gets pinged" received ping '"zen"'
git -C "$W" checkout -q main && git -C "$W" fetch -q "$ADA_HTTP" main && git -C "$W" reset -q --hard FETCH_HEAD
echo "hooked" >>"$W/README.md" && git -C "$W" commit -qam "Hooked commit"
git -C "$W" push -q "$ADA_HTTP" main
check "push is delivered" received push '"message":"Hooked commit"'
D=$(received push '"message":"Hooked commit"')
check "deliveries are signed" is "$(cut -d' ' -f2 "$D.hdr")" "sha256=$(openssl dgst -sha256 -hmac s3cret -r "$D.json" | cut -d' ' -f1)"
check "opening an issue is delivered" is "$(post "$T/bob" /ada/engine/issues title="Hook me" body="cc @eve")" 303
check "issues event" received issues '"title":"Hook me"'
HOOK=$(curl -s -b "$T/ada" "$B/ada/engine/settings/hooks" | grep -o 'settings/hooks/[0-9]*' | head -1 | grep -o '[0-9]*$')
logged() { for _ in $(seq 1 50); do curl -s -b "$T/ada" "$B/ada/engine/settings/hooks/$HOOK" | grep -q "HTTP 200" && return 0; sleep 0.1; done; return 1; }
check "delivery log shows the response" logged
DELIVERY=$(curl -s -b "$T/ada" "$B/ada/engine/settings/hooks/$HOOK" | grep -o 'deliveries/[0-9]*/redeliver' | head -1 | grep -o '[0-9]*')
BEFORE=$(ls "$T"/hooks/*.json | wc -l)
check "redeliver" is "$(post "$T/ada" "/ada/engine/settings/hooks/$HOOK/deliveries/$DELIVERY/redeliver" x=1)" 303
redelivered() { for _ in $(seq 1 50); do [ "$(ls "$T"/hooks/*.json | wc -l)" -gt "$BEFORE" ] && return 0; sleep 0.1; done; return 1; }
check "redelivery arrives" redelivered
check "invalid URLs are rejected" contains "$(curl -s -b "$T/ada" -H "$O" -d "url=ftp://x&which=push" "$B/ada/engine/settings/hooks")" "start with http"
check "org owners add organization webhooks" is "$(post "$T/ada" /org/acme/settings/hooks "url=http://127.0.0.1:$HOOK_PORT/org" which=push active=on)" 303
check "members can't" is "$(status -b "$T/eve" "$B/org/acme/settings/hooks")" 403
git -C "$W" push -q -f "http://ada:password-ada@127.0.0.1:$HTTP_PORT/acme/tool.git" main
check "pushes to its repositories reach it" received push '"full_name":"acme/tool"'
check "wiki edits are delivered" is "$(post "$T/ada" /ada/engine/wiki title=Home "text=Welcome to the [[Getting Started]] guide.")" 303
check "as gollum events" received gollum '"page_name":"Home"'
check "stars are delivered" is "$(post "$T/bob" /ada/engine/star to=on)" 303
check "as star events" received star '"action":"created"'
check "unstar" is "$(post "$T/bob" /ada/engine/star to=off)" 303
kill "$HOOK_PID" 2>/dev/null

echo "== notifications"
HOOKED=$(curl -s "$B/ada/engine/issues" | grep -o 'href="/ada/engine/issues/[0-9]*">Hook me' | grep -o '[0-9]*')
check "watchers hear about new issues" contains "$(curl -s -b "$T/ada" "$B/notifications")" "Hook me"
check "unread count in the header" contains "$(curl -s -b "$T/ada" "$B/")" 'class="unread-count"'
check "mentioned people hear too" contains "$(curl -s -b "$T/eve" "$B/notifications")" "Mentioned"
check "viewing the issue marks it read" bash -c "curl -s -b '$T/ada' '$B/ada/engine/issues/$HOOKED' >/dev/null; ! curl -s -b '$T/ada' '$B/notifications' | grep -q 'Hook me'"
check "ada comments" is "$(post "$T/ada" "/ada/engine/issues/$HOOKED/comments" body="On it" action=comment)" 303
check "the author hears about comments" contains "$(curl -s -b "$T/bob" "$B/notifications")" "ada commented"
check "eve unsubscribes" is "$(post "$T/eve" "/ada/engine/issues/$HOOKED/subscription" to=off)" 303
check "mark all read" is "$(post "$T/eve" /notifications/read x=1)" 303
check "ada comments again" is "$(post "$T/ada" "/ada/engine/issues/$HOOKED/comments" body="Done" action=comment)" 303
check "unsubscribed people don't" contains "$(curl -s -b "$T/eve" "$B/notifications")" "All caught up"
check "eve watches the repository" is "$(post "$T/eve" /ada/engine/watch to=on)" 303
check "watch button reflects it" contains "$(curl -s -b "$T/eve" "$B/ada/engine")" "Unwatch"
check "private repos don't notify outsiders" bash -c "! curl -s -b '$T/eve' '$B/notifications?all=1' | grep -q 'ada/secret'"

echo "== account"
check "wrong current password is rejected" contains "$(curl -s -b "$T/bob" -H "$O" -d "current=nope&new=password-new&confirm=password-new" "$B/settings/account/password")" "incorrect"
check "change password" is "$(post "$T/bob" /settings/account/password current=password-bob new=password-new confirm=password-new)" 303
check "still signed in after password change" contains "$(curl -s -b "$T/bob" "$B/settings/account")" "bob@example.com"

echo "== email"
mail_to() { # mail_to <address> <text>: waits up to 5s for a message to <address> containing <text>
    for _ in $(seq 1 50); do
        for m in "$T"/mail/*.eml; do
            [ -e "$m" ] || continue
            if grep -qi "^To: .*$1" "$m" && grep -q -- "$2" "$m"; then echo "$m"; return 0; fi
        done
        sleep 0.1
    done
    echo "no mail to $1 containing '$2'"
    return 1
}
check "signing up sends a verification link" mail_to ada@example.com "/email/verify?token="
VERIFY=$(grep -o '/email/verify?token=[A-Za-z0-9_-]*' "$(mail_to ada@example.com /email/verify)" | head -1)
check "the link verifies the address" contains "$(curl -s "$B$VERIFY")" "is verified"
check "account settings show it" contains "$(curl -s -b "$T/ada" "$B/settings/account")" "Verified"
check "email everything" is "$(post "$T/ada" /settings/notifications email=all)" 303
check "bob opens an issue" is "$(post "$T/bob" /ada/engine/issues title="Mail me" body="Watch this")" 303
check "watchers get notification emails" mail_to ada@example.com "Subject: \[ada/engine\] Mail me"
check "threaded by issue" bash -c "grep -q '^References: <ada/engine/issues/' '$(mail_to ada@example.com 'Mail me')'"
check "unverified addresses get none" bash -c "! grep -l 'Mail me' '$T'/mail/*.eml | xargs grep -qi '^To: .*eve@'"
check "login offers password resets" contains "$(curl -s "$B/login")" "Forgot your password?"
check "unknown addresses look the same" contains "$(curl -s -H "$O" -d "email=nobody@example.com" "$B/password/forgot")" "Check your email"
check "request a reset" contains "$(curl -s -H "$O" -d "email=eve@example.com" "$B/password/forgot")" "Check your email"
RESET=$(grep -o 'token=[A-Za-z0-9_-]*' "$(mail_to eve@example.com /password/reset)" | head -1 | cut -d= -f2)
check "reset form" is "$(status "$B/password/reset?token=$RESET")" 200
check "set a new password" is "$(curl -s -o /dev/null -w '%{http_code}' -c "$T/eve2" -H "$O" --data-urlencode "token=$RESET" -d "new=password-reset&confirm=password-reset" "$B/password/reset")" 303
check "signed in afterwards" contains "$(curl -s -b "$T/eve2" "$B/settings/account")" "eve@example.com"
check "old sessions are signed out" is "$(status -b "$T/eve" "$B/settings/account")" 303
check "the new password works" contains "$(curl -s -H "$O" -d "login=eve&password=password-reset" -o /dev/null -w '%{http_code}' "$B/login")" 303
check "links work once" contains "$(curl -s "$B/password/reset?token=$RESET")" "expired"

echo "== branch protection"
check "readers can't manage branch rules" is "$(status -b "$T/eve2" "$B/ada/engine/settings/branches")" 403
check "protect main" is "$(post "$T/ada" /ada/engine/settings/branches pattern=main require_pull=on required_approvals=1 dismiss_stale=on)" 303
check "bad patterns are rejected" contains "$(curl -s -b "$T/ada" -H "$O" -d "pattern=refs/heads/x&required_approvals=0" "$B/ada/engine/settings/branches")" "pattern like"
check "branches page marks it" contains "$(curl -s "$B/ada/engine/branches")" "badge-protected"
git -C "$W" checkout -q main && git -C "$W" fetch -q "$ADA_HTTP" main && git -C "$W" reset -q --hard FETCH_HEAD
MAIN=$(git -C "$W" rev-parse HEAD)
printf 'speed = 1\n' >"$W/config.txt" && git -C "$W" add -A && git -C "$W" commit -qm "Add config"
check "direct pushes are refused" bash -c "git -C '$W' push '$ADA_HTTP' main 2>&1 | grep -q 'must be made through a pull request'"
check "over SSH too" bash -c "git -C '$W' push 'ssh://git@127.0.0.1:$SSH_PORT/ada/engine.git' main 2>&1 | grep -q 'protected'"
check "main didn't move" is "$(git ls-remote "$B/ada/engine.git" refs/heads/main | cut -f1)" "$MAIN"
check "deleting it is refused" bash -c "git -C '$W' push '$ADA_HTTP' :main 2>&1 | grep -q \"can't be deleted\""
check "other branches can be pushed" git -C "$W" push -q "$ADA_HTTP" main:tune
check "open a pull request into it" is "$(post "$T/ada" /ada/engine/pulls base=main head=tune title="Tune speed")" 303
PP=$(curl -s "$B/ada/engine/pulls" | grep -o 'href="/ada/engine/pull/[0-9]*"' | head -1 | grep -o '[0-9]*')
check "merge box explains the rule" contains "$(curl -s -b "$T/ada" "$B/ada/engine/pull/$PP")" "Merging is blocked"
check "merging without approval is refused" contains "$(curl -s -b "$T/ada" -H "$O" -d style=merge "$B/ada/engine/pull/$PP/merge")" "main is protected"

echo "== review requests and suggestions"
check "make bob a writer" is "$(post "$T/ada" /ada/engine/collaborators username=bob role=write)" 303
BOB_ID=$(curl -s -b "$T/ada" "$B/ada/engine/pull/$PP" | grep -A3 '/reviewers"' | grep -o 'name="user" value="[0-9]*"' | head -1 | grep -o '[0-9]*')
check "eve can't request reviews" is "$(post "$T/eve2" /ada/engine/issues/$PP/reviewers "user=$BOB_ID")" 403
check "ask bob to review" is "$(post "$T/ada" /ada/engine/issues/$PP/reviewers "user=$BOB_ID")" 303
check "the request is in the timeline" contains "$(curl -s "$B/ada/engine/pull/$PP")" "requested a review from"
check "bob is notified" contains "$(curl -s -b "$T/bob" "$B/notifications")" "Review requested"
check "filter by review requests" contains "$(curl -s "$B/ada/engine/pulls?review_requested=bob")" "Tune speed"
FILES=$(curl -s -b "$T/bob" "$B/ada/engine/pull/$PP/files")
HEAD=$(grep -o 'name="head" value="[0-9a-f]*"' <<<"$FILES" | head -1 | cut -d'"' -f4)
check "lines offer suggestions" contains "$FILES" "Suggest a change"
check "bob suggests a change" is "$(post "$T/bob" /ada/engine/pull/$PP/comments path=config.txt side=new line=1 "head=$HEAD" body=$'Faster:\n```suggestion\nspeed = 2\n```')" 303
check "it shows as a change" contains "$(curl -s "$B/ada/engine/pull/$PP/files")" "Suggested change"
check "readers can't apply it" bash -c "! curl -s -b '$T/eve2' '$B/ada/engine/pull/$PP/files' | grep -q 'Apply suggestion'"
APPLY=$(curl -s -b "$T/ada" "$B/ada/engine/pull/$PP/files" | grep -o 'pull/[0-9]*/comments/[0-9]*/apply' | head -1)
check "ada applies it" is "$(post "$T/ada" "/ada/engine/$APPLY" x=1)" 303
check "the branch has the change" is "$(curl -s "$B/ada/engine/raw/tune/config.txt")" "speed = 2"
check "applying twice is refused" is "$(post "$T/ada" "/ada/engine/$APPLY" x=1)" 400
check "bob approves" is "$(post "$T/bob" /ada/engine/pull/$PP/review verdict=approve)" 303
check "the request is done" bash -c "! curl -s '$B/ada/engine/pulls?review_requested=bob' | grep -q 'Tune speed'"
check "merge box counts the approval" contains "$(curl -s -b "$T/ada" "$B/ada/engine/pull/$PP")" "1 approving review"
check "merge it" is "$(post "$T/ada" /ada/engine/pull/$PP/merge style=squash delete_branch=on)" 303
check "main has the suggestion" is "$(curl -s "$B/ada/engine/raw/main/config.txt")" "speed = 2"

echo "== milestones"
check "readers can't create milestones" is "$(post "$T/eve2" /ada/engine/milestones title=v1)" 403
check "create a milestone" is "$(post "$T/ada" /ada/engine/milestones title=v1.0 due_on=2030-01-01 description="First release")" 303
check "bad dates are rejected" contains "$(curl -s -b "$T/ada" -H "$O" -d "title=x&due_on=soon" "$B/ada/engine/milestones")" "YYYY-MM-DD"
MS=$(curl -s -b "$T/ada" "$B/ada/engine/milestones" | grep -o 'milestones/[0-9]*/edit' | head -1 | grep -o '[0-9]*')
check "milestones are listed" contains "$(curl -s "$B/ada/engine/milestones")" "Due by 2030-01-01"
check "add an issue to it" is "$(post "$T/ada" /ada/engine/issues/1/milestone "milestone=$MS")" 303
check "the issue shows it" contains "$(curl -s "$B/ada/engine/issues/1")" "added this to the"
check "progress counts it" contains "$(curl -s "$B/ada/engine/milestones")" "1 open"
check "filter by milestone" contains "$(curl -s "$B/ada/engine/issues?milestone=v1.0")" "It crashes on start"
check "close the issue" is "$(post "$T/ada" /ada/engine/issues/1/comments action=close)" 303
check "progress moves" contains "$(curl -s "$B/ada/engine/milestones")" "<strong>100%</strong>"
check "close the milestone" is "$(post "$T/ada" /ada/engine/milestones/$MS/state to=closed)" 303
check "closed milestones are listed apart" contains "$(curl -s "$B/ada/engine/milestones?state=closed")" "v1.0"

echo "== API"
API=$B/api/v1
api() { # api <token> <method> <path> [curl args...]: prints the body
    local tok=$1 m=$2 p=$3
    shift 3
    curl -s -X "$m" -H "Authorization: Bearer $tok" -H "Content-Type: application/json" "$@" "$API$p"
}
api_status() { # api_status <token> <method> <path> [curl args...]
    local tok=$1 m=$2 p=$3
    shift 3
    curl -s -o /dev/null -w '%{http_code}' -X "$m" -H "Authorization: Bearer $tok" -H "Content-Type: application/json" "$@" "$API$p"
}
field() { python3 -c "import json,sys; print(json.load(sys.stdin)$1)"; }
check "version" contains "$(curl -s "$API/version")" '"version"'
check "anonymous /user needs a token" is "$(status "$API/user")" 401
check "bad tokens are refused" contains "$(api keel_nope GET /user)" "Bad credentials"
check "passwords don't work" is "$(curl -s -o /dev/null -w '%{http_code}' -u ada:password-ada "$API/user")" 401
check "the token's user" is "$(api "$RW" GET /user | field '["login"]')" ada
check "repository" is "$(curl -s "$API/repos/ada/engine" | field '["default_branch"]')" main
check "private repositories are hidden" is "$(status "$API/repos/ada/secret")" 404
check "read-only tokens read them" is "$(api_status "$RO" GET /repos/ada/secret)" 200
check "but can't write" contains "$(api "$RO" POST /repos/ada/engine/issues -d '{"title":"x"}')" "read-only"
check "create an issue" is "$(api_status "$RW" POST /repos/ada/engine/issues -d '{"title":"From the API","body":"Hi","labels":["bug"]}')" 201
AI=$(api "$RW" GET "/repos/ada/engine/issues?labels=bug" | field '[0]["number"]')
check "list issues by label" is "$(api "$RW" GET "/repos/ada/engine/issues?labels=bug" | field '[0]["title"]')" "From the API"
check "comment on it" is "$(api_status "$RW" POST "/repos/ada/engine/issues/$AI/comments" -d '{"body":"Via curl"}')" 201
check "comments are listed" is "$(curl -s "$API/repos/ada/engine/issues/$AI/comments" | field '[0]["body"]')" "Via curl"
check "close it" is "$(api "$RW" PATCH "/repos/ada/engine/issues/$AI" -d '{"state":"closed","milestone":null}' | field '["state"]')" closed
check "file contents" is "$(curl -s "$API/repos/ada/engine/contents/config.txt" | field '["content"]' | base64 -d)" "speed = 2"
check "directory listings" contains "$(curl -s "$API/repos/ada/engine/contents")" '"README.md"'
check "branches say what's protected" is "$(curl -s "$API/repos/ada/engine/branches/main" | field '["protected"]')" True
check "commits are paged with Link headers" contains "$(curl -s -D - -o /dev/null "$API/repos/ada/engine/commits?per_page=1")" 'rel="next"'
check "JSON without a content type is fine" is "$(curl -s -o /dev/null -w '%{http_code}' -H "Authorization: Bearer $RW" -d '{"body":"No header"}' "$API/repos/ada/engine/issues/$AI/comments")" 201
check "invalid JSON is explained" contains "$(api "$RW" POST "/repos/ada/engine/issues/$AI/comments" -d '{"body":')" "Invalid JSON"
check "unknown endpoints are JSON 404s" contains "$(curl -s "$API/nope")" '"message"'
git -C "$W" checkout -q -b api-change main~1 2>/dev/null || git -C "$W" checkout -q -b api-change
git -C "$W" fetch -q "$ADA_HTTP" main && git -C "$W" reset -q --hard FETCH_HEAD
echo "api" >"$W/api.txt" && git -C "$W" add -A && git -C "$W" commit -qm "Add api.txt"
git -C "$W" push -q "$ADA_HTTP" api-change
check "open a pull request" is "$(api_status "$RW" POST /repos/ada/engine/pulls -d '{"title":"API change","head":"api-change","base":"main"}')" 201
AP=$(api "$RW" GET /repos/ada/engine/pulls | field '[0]["number"]')
check "it says why it can't be merged yet" contains "$(curl -s "$API/repos/ada/engine/pulls/$AP" | field '["merge_blocked_by"]')" "approving review"
check "merging is refused" is "$(api_status "$RW" PUT "/repos/ada/engine/pulls/$AP/merge" -d '{}')" 405
page=$(curl -s -b "$T/bob" -H "$O" --data-urlencode "name=api" --data-urlencode "write=on" --data-urlencode "password=password-new" "$B/settings/tokens")
BOBTOK=$(grep -o 'value="keel_[A-Za-z0-9_-]*"' <<<"$page" | cut -d'"' -f2)
check "bob approves through the API" is "$(api "$BOBTOK" POST "/repos/ada/engine/pulls/$AP/reviews" -d '{"event":"APPROVE"}' | field '["state"]')" APPROVED
check "merge through the API" is "$(api "$RW" PUT "/repos/ada/engine/pulls/$AP/merge" -d '{"merge_method":"squash","delete_branch":true}' | field '["merged"]')" True
check "main has it" is "$(curl -s "$B/ada/engine/raw/main/api.txt")" api
check "create a repository" is "$(api_status "$RW" POST /user/repos -d '{"name":"from-api","private":true}')" 201
check "it's private" is "$(status "$API/repos/ada/from-api")" 404
check "delete it" is "$(api_status "$RW" DELETE /repos/ada/from-api)" 204

echo "== administration"
check "others don't see it" is "$(status -b "$T/bob" "$B/admin")" 404
check "overview" contains "$(curl -s -b "$T/ada" "$B/admin")" "accounts"
check "close sign-ups and post a notice" is "$(post "$T/ada" /admin/settings registration_closed=on banner="Maintenance tonight")" 303
check "the notice shows everywhere" contains "$(curl -s "$B/ada/engine")" "Maintenance tonight"
check "sign-ups are closed" contains "$(curl -s -H "$O" -d "username=yan&email=yan@example.com&password=password-yan" "$B/register")" "closed"
check "admins still create accounts" is "$(post "$T/ada" /admin/users username=zed email=zed@example.com password=password-zed)" 303
check "the new account signs in" is "$(curl -s -o /dev/null -w '%{http_code}' -c "$T/zed" -H "$O" -d "login=zed&password=password-zed" "$B/login")" 303
Z=$(curl -s -b "$T/ada" "$B/admin/users?q=zed" | grep -o 'admin/users/[0-9]*/disable' | head -1 | grep -o '[0-9]*')
check "disable it" is "$(post "$T/ada" "/admin/users/$Z/disable" to=on)" 303
check "which signs it out" is "$(status -b "$T/zed" "$B/settings/account")" 303
check "and keeps it out" contains "$(curl -s -H "$O" -d "login=zed&password=password-zed" "$B/login")" "Incorrect"
check "the last admin stays" contains "$(curl -s -b "$T/ada" -H "$O" -d to=off "$B/admin/users/1/admin")" "at least one administrator"
check "delete the account" is "$(post "$T/ada" "/admin/users/$Z/delete" x=1)" 303
check "it's gone" is "$(status "$B/zed")" 404
check "reopen sign-ups" is "$(post "$T/ada" /admin/settings banner=)" 303
check "the notice is gone" bash -c "! curl -s '$B/' | grep -q 'Maintenance tonight'"

echo "== commit statuses"
MAIN=$(git ls-remote "$B/ada/engine.git" refs/heads/main | cut -f1)
check "read-only tokens can't set statuses" is "$(api_status "$RO" POST "/repos/ada/engine/statuses/$MAIN" -d '{"state":"success"}')" 403
check "unknown states are refused" is "$(api_status "$RW" POST "/repos/ada/engine/statuses/$MAIN" -d '{"state":"great"}')" 422
check "CI reports a build" is "$(api_status "$RW" POST "/repos/ada/engine/statuses/$MAIN" -d '{"state":"pending","context":"ci/build","target_url":"https://ci.example.com/1","description":"Building"}')" 201
check "the combined state" is "$(curl -s "$API/repos/ada/engine/commits/main/status" | field '["state"]')" pending
check "and its result" is "$(api_status "$RW" POST "/repos/ada/engine/statuses/$MAIN" -d '{"state":"success","context":"ci/build","target_url":"https://ci.example.com/1"}')" 201
check "commit lists show it" contains "$(curl -s "$B/ada/engine/commits/main")" "ci-success"
check "so do branches" contains "$(curl -s "$B/ada/engine/branches")" "ci-success"
check "the commit lists its checks" contains "$(curl -s "$B/ada/engine/commit/$MAIN")" "https://ci.example.com/1"
RULE=$(curl -s -b "$T/ada" "$B/ada/engine/settings/branches" | grep -o 'settings/branches/[0-9]*/delete' | head -1 | grep -o '[0-9]*')
check "require the check on main" is "$(post "$T/ada" "/ada/engine/settings/branches/$RULE" pattern=main require_pull=on required_approvals=0 required_checks=ci/build)" 303
git -C "$W" checkout -q -b checked main && git -C "$W" fetch -q "$ADA_HTTP" main && git -C "$W" reset -q --hard FETCH_HEAD
echo "checked" >"$W/checked.txt" && git -C "$W" add -A && git -C "$W" commit -qm "Add checked.txt"
git -C "$W" push -q "$ADA_HTTP" checked
CHECKED=$(git -C "$W" rev-parse HEAD)
check "open a pull request" is "$(api_status "$RW" POST /repos/ada/engine/pulls -d '{"title":"Checked","head":"checked","base":"main"}')" 201
CP=$(api "$RW" GET /repos/ada/engine/pulls | field '[0]["number"]')
check "it waits for the check" contains "$(curl -s -b "$T/ada" "$B/ada/engine/pull/$CP")" "ci/build hasn&#39;t reported"
check "merging is refused" is "$(api_status "$RW" PUT "/repos/ada/engine/pulls/$CP/merge")" 405
check "CI fails it" is "$(api_status "$RW" POST "/repos/ada/engine/statuses/$CHECKED" -d '{"state":"failure","context":"ci/build"}')" 201
check "the list shows the failure" contains "$(curl -s "$B/ada/engine/pulls")" "ci-failure"
check "the merge box too" contains "$(curl -s -b "$T/ada" "$B/ada/engine/pull/$CP")" "ci/build failed"
check "CI passes it" is "$(api_status "$RW" POST "/repos/ada/engine/statuses/$CHECKED" -d '{"state":"success","context":"ci/build"}')" 201
check "now it merges" is "$(api "$RW" PUT "/repos/ada/engine/pulls/$CP/merge" | field '["merged"]')" True

echo "== two-factor authentication"
signup tia "$T/tia"
check "set it up" is "$(post "$T/tia" /settings/security/setup x=1)" 303
SECRET=$(curl -s -b "$T/tia" "$B/settings/security" | grep -o 'class="mono secret">[A-Z2-7 ]*' | cut -d'>' -f2 | tr -d ' ')
totp() { # totp <steps from now>: the code for $SECRET
    python3 -c 'import base64,hmac,hashlib,struct,sys,time
k = base64.b32decode(sys.argv[1] + "=" * (-len(sys.argv[1]) % 8))
h = hmac.new(k, struct.pack(">Q", int(time.time()) // 30 + int(sys.argv[2])), hashlib.sha1).digest()
o = h[19] & 15
print("%06d" % ((struct.unpack(">I", h[o:o + 4])[0] & 0x7fffffff) % 1000000))' "$SECRET" "${1:-0}"
}
check "the page shows a key" is "${#SECRET}" 32
check "a wrong code doesn't turn it on" contains "$(curl -s -b "$T/tia" -H "$O" -d code=00000x "$B/settings/security/enable")" "isn"
page=$(curl -s -b "$T/tia" -H "$O" -d "code=$(totp 0)" "$B/settings/security/enable")
check "the right one does" contains "$page" "Your recovery codes"
RCODE=$(grep -o '<li>[a-z2-7]\{5\}-[a-z2-7]\{5\}</li>' <<<"$page" | head -1 | sed 's/<[^>]*>//g')
check "signing in asks for a code" is "$(curl -s -o /dev/null -w '%{redirect_url}' -c "$T/tia2" -H "$O" -d "login=tia&password=password-tia" "$B/login")" "$B/login/2fa?next=%2F"
check "without one there's no session" is "$(status -b "$T/tia2" "$B/settings/account")" 303
check "wrong codes are refused" contains "$(curl -s -b "$T/tia2" -H "$O" -d code=000000 "$B/login/2fa")" "isn"
check "the next code signs in" is "$(curl -s -b "$T/tia2" -c "$T/tia2" -o /dev/null -w '%{http_code}' -H "$O" -d "code=$(totp 1)" "$B/login/2fa")" 303
check "signed in" contains "$(curl -s -b "$T/tia2" "$B/settings/account")" "tia@example.com"
curl -s -o /dev/null -c "$T/tia3" -H "$O" -d "login=tia&password=password-tia" "$B/login"
check "so does a recovery code" is "$(curl -s -b "$T/tia3" -c "$T/tia3" -o /dev/null -w '%{http_code}' -H "$O" -d "code=$RCODE" "$B/login/2fa")" 303
check "once" bash -c "curl -s -o /dev/null -c '$T/tia4' -H '$O' -d 'login=tia&password=password-tia' '$B/login'; curl -s -b '$T/tia4' -H '$O' -d 'code=$RCODE' '$B/login/2fa' | grep -q 'isn'"
check "create a private repository" is "$(post "$T/tia2" /new name=box private=on)" 303
check "git refuses the password" bash -c "! git ls-remote -q 'http://tia:password-tia@127.0.0.1:$HTTP_PORT/tia/box.git' 2>/dev/null"
TIA=$(curl -s -b "$T/ada" "$B/admin/users?q=tia" | grep -o 'admin/users/[0-9]*/2fa' | head -1 | cut -d/ -f3)
check "admins can turn it off" is "$(post "$T/ada" "/admin/users/$TIA/2fa" x=1)" 303
check "then the password is enough" is "$(curl -s -o /dev/null -w '%{redirect_url}' -H "$O" -d "login=tia&password=password-tia" "$B/login")" "$B/"

echo "== mirrors"
check "only http(s) URLs" contains "$(curl -s -b "$T/ada" -H "$O" --data-urlencode "name=m0" --data-urlencode "mirror_url=file:///etc" "$B/new")" "http://"
check "mirror a repository" is "$(post "$T/ada" /new name=engine-mirror "mirror_url=$B/ada/engine.git" mirror_interval=3600)" 303
synced() { # synced <repo> <ref> <commit>: waits up to 10s for the mirror to have it
    for _ in $(seq 1 100); do [ "$(git ls-remote "$B/ada/$1.git" "$2" | cut -f1)" = "$3" ] && return 0; sleep 0.1; done
    echo "$1 doesn't have $2 at $3"; return 1
}
SRC_MAIN=$(git ls-remote "$B/ada/engine.git" refs/heads/main | cut -f1)
check "it copies the branches" synced engine-mirror refs/heads/main "$SRC_MAIN"
check "the page says so" contains "$(curl -s "$B/ada/engine-mirror")" "mirror of"
check "and the default branch" is "$(curl -s "$API/repos/ada/engine-mirror" | field '["default_branch"]')" main
check "pushing to it is refused" bash -c "git -C '$W' push 'http://ada:password-ada@127.0.0.1:$HTTP_PORT/ada/engine-mirror.git' HEAD:refs/heads/x 2>&1 | grep -q mirror"
check "so are pull requests into it" contains "$(curl -s -b "$T/ada" -H "$O" -d "base=main&head=checked&title=x" "$B/ada/engine-mirror/pulls")" "mirror"
git -C "$W" checkout -q -b later main && echo later >"$W/later.txt" && git -C "$W" add -A && git -C "$W" commit -qm Later
git -C "$W" push -q "$ADA_HTTP" later
check "sync now" is "$(post "$T/ada" /ada/engine-mirror/mirror/sync x=1)" 303
check "brings new branches" synced engine-mirror refs/heads/later "$(git -C "$W" rev-parse HEAD)"
git -C "$W" push -q "$ADA_HTTP" :later
check "and deletions" bash -c "post() { :; }; curl -s -o /dev/null -b '$T/ada' -H '$O' -d x=1 '$B/ada/engine-mirror/mirror/sync'; for _ in \$(seq 1 100); do [ -z \"\$(git ls-remote '$B/ada/engine-mirror.git' refs/heads/later)\" ] && exit 0; sleep 0.1; done; exit 1"
check "private sources need credentials" is "$(post "$T/ada" /new name=vault-mirror private=on "mirror_url=$B/ada/secret.git")" 303
failed() { for _ in $(seq 1 100); do curl -s -b "$T/ada" "$B/ada/vault-mirror" | grep -q "Last sync failed" && return 0; sleep 0.1; done; return 1; }
check "without them it fails, and says why" failed
check "add a token" is "$(post "$T/ada" /ada/vault-mirror/settings/mirror "url=$B/ada/secret.git" username=ada "password=$RO" interval=3600)" 303
check "then it syncs" bash -c "curl -s -o /dev/null -b '$T/ada' -H '$O' -d x=1 '$B/ada/vault-mirror/mirror/sync'; for _ in \$(seq 1 100); do git ls-remote 'http://ada:$RO@127.0.0.1:$HTTP_PORT/ada/vault-mirror.git' refs/heads/main | grep -q . && exit 0; sleep 0.1; done; exit 1"
check "the token isn't shown" bash -c "! curl -s -b '$T/ada' '$B/ada/vault-mirror/settings' | grep -q '$RO'"
check "stop mirroring" is "$(post "$T/ada" /ada/engine-mirror/settings/mirror/stop x=1)" 303
check "then it takes pushes" git -C "$W" push -q "http://ada:password-ada@127.0.0.1:$HTTP_PORT/ada/engine-mirror.git" HEAD:refs/heads/x

echo "== Git LFS"
LFS_URL="$B/ada/engine.git/info/lfs"
batch() { # batch <token or ""> <repo> <json>
    local auth=()
    [ -n "$1" ] && auth=(-u "ada:$1")
    curl -s "${auth[@]}" -H "Accept: application/vnd.git-lfs+json" -H "Content-Type: application/vnd.git-lfs+json" -d "$3" "$B/ada/$2.git/info/lfs/objects/batch"
}
printf 'large\n' >"$T/large.txt"
OID=$(sha256sum "$T/large.txt" | cut -d' ' -f1)
REQ='{"operation":"upload","transfers":["basic"],"objects":[{"oid":"'$OID'","size":6}]}'
check "anonymous uploads are refused" contains "$(batch "" engine "$REQ")" "Authentication required"
check "read-only tokens can't upload" contains "$(batch "$RO" engine "$REQ")" "read-only"
UP=$(batch "$RW" engine "$REQ")
check "uploads are offered" contains "$UP" '"upload"'
HREF=$(field '["objects"][0]["actions"]["upload"]["href"]' <<<"$UP")
AUTH=$(field '["objects"][0]["actions"]["upload"]["header"]["Authorization"]' <<<"$UP")
check "content that doesn't match is refused" is "$(curl -s -o /dev/null -w '%{http_code}' -X PUT -H "Authorization: $AUTH" --data-binary 'other' "$HREF")" 422
check "upload with the batch's token" is "$(curl -s -o /dev/null -w '%{http_code}' -X PUT -H "Authorization: $AUTH" --data-binary @"$T/large.txt" "$HREF")" 200
check "then it's already there" bash -c "! grep -q '\"actions\"' <<<'$(batch "$RW" engine "$REQ")'"
DOWN=$(batch "" engine '{"operation":"download","objects":[{"oid":"'$OID'","size":6}]}')
check "public repositories' files download anonymously" is "$(curl -s "$(field '["objects"][0]["actions"]["download"]["href"]' <<<"$DOWN")")" large
check "other repositories can't read it" contains "$(batch "$RW" engine-mirror '{"operation":"download","objects":[{"oid":"'$OID'","size":6}]}')" "does not exist"
check "forged tokens are refused" is "$(curl -s -o /dev/null -w '%{http_code}' -H "Authorization: Bearer lfs1.1.1.1.9999999999.abc" "$HREF")" 401
check "locks are answered" contains "$(curl -s -u "ada:$RW" -H "Content-Type: application/vnd.git-lfs+json" -d '{}' "$LFS_URL/locks/verify")" '"ours"'
if command -v git-lfs >/dev/null; then
    L=$T/lfs-work
    git clone -q "http://ada:$RW@127.0.0.1:$HTTP_PORT/ada/engine.git" "$L"
    git -C "$L" lfs install --local >/dev/null
    git -C "$L" lfs track "*.bin" >/dev/null
    head -c 3000000 /dev/urandom >"$L/data.bin"
    BIN_SUM=$(sha256sum "$L/data.bin" | cut -d' ' -f1)
    git -C "$L" add -A && git -C "$L" commit -qm "Add data.bin"
    check "git lfs push" bash -c "git -C '$L' push -q origin HEAD:refs/heads/lfs 2>&1 | grep -v -i 'uploading' | grep -qi 'error' && exit 1 || exit 0"
    check "the pointer is in the repository" contains "$(curl -s "$B/ada/engine/blob/lfs/data.bin")" "Stored with Git LFS"
    check "raw serves the file" is "$(curl -s "$B/ada/engine/raw/lfs/data.bin" | sha256sum | cut -d' ' -f1)" "$BIN_SUM"
    check "settings count it" contains "$(curl -s -b "$T/ada" "$B/ada/engine/settings")" ">2 files, "
    GIT_LFS_SKIP_SMUDGE=1 git clone -q -b lfs "http://ada:$RW@127.0.0.1:$HTTP_PORT/ada/engine.git" "$T/lfs-clone"
    check "git lfs pull over HTTP" bash -c "cd '$T/lfs-clone' && git lfs install --local >/dev/null && git lfs pull >/dev/null 2>&1 && [ \"\$(sha256sum data.bin | cut -d' ' -f1)\" = '$BIN_SUM' ]"
    GIT_LFS_SKIP_SMUDGE=1 git clone -q -b lfs "ssh://git@127.0.0.1:$SSH_PORT/ada/engine.git" "$T/lfs-ssh"
    check "and over SSH" bash -c "cd '$T/lfs-ssh' && git lfs install --local >/dev/null && git lfs pull >/dev/null 2>&1 && [ \"\$(sha256sum data.bin | cut -d' ' -f1)\" = '$BIN_SUM' ]"
else
    echo "SKIP  git lfs client checks (git-lfs isn't installed)"
fi

echo "== LFS locks and mirrors"
locks() { curl -s -u "$1" -H "Accept: application/vnd.git-lfs+json" -H "Content-Type: application/vnd.git-lfs+json" "${@:2}"; }
check "lock a file" is "$(locks "ada:$RW" -o /dev/null -w '%{http_code}' -d '{"path":"large.txt"}' "$LFS_URL/locks")" 201
check "others can't lock it too" contains "$(locks "bob:$BOBTOK" -d '{"path":"large.txt"}' "$LFS_URL/locks")" "already created lock"
check "locks are listed" contains "$(locks "bob:$BOBTOK" "$LFS_URL/locks?path=large.txt")" '"name":"ada"'
check "verify says whose they are" is "$(locks "bob:$BOBTOK" -d '{}' "$LFS_URL/locks/verify" | field '["theirs"][0]["path"]')" large.txt
LOCK=$(locks "ada:$RW" "$LFS_URL/locks" | field '["locks"][0]["id"]')
check "others can't unlock it" is "$(locks "bob:$BOBTOK" -o /dev/null -w '%{http_code}' -d '{}' "$LFS_URL/locks/$LOCK/unlock")" 403
check "the owner can" is "$(locks "ada:$RW" -o /dev/null -w '%{http_code}' -d '{}' "$LFS_URL/locks/$LOCK/unlock")" 200
if command -v git-lfs >/dev/null; then
    check "git lfs lock" bash -c "cd '$L' && git lfs lock data.bin >/dev/null 2>&1 && git lfs locks | grep -q 'data.bin.*ada'"
    check "shows on the file" contains "$(curl -s "$B/ada/engine/blob/lfs/data.bin")" "locked by ada"
    check "and in settings" contains "$(curl -s -b "$T/ada" "$B/ada/engine/settings")" "Locked files"
    GIT_LFS_SKIP_SMUDGE=1 git clone -q -b lfs "http://bob:$BOBTOK@127.0.0.1:$HTTP_PORT/ada/engine.git" "$T/bob-lfs"
    (cd "$T/bob-lfs" && git lfs install --local >/dev/null && git lfs pull >/dev/null 2>&1)
    head -c 1000 /dev/urandom >"$T/bob-lfs/data.bin"
    git -C "$T/bob-lfs" -c user.name=Bob -c user.email=bob@example.com commit -qam "Change data.bin"
    check "git lfs refuses to push changes to others' locked files" bash -c "git -C '$T/bob-lfs' push origin HEAD:refs/heads/bob-change 2>&1 | grep -qi 'locked'"
    check "git lfs unlock" bash -c "cd '$L' && git lfs unlock data.bin >/dev/null 2>&1 && ! git lfs locks | grep -q data.bin"
fi
# A commit pointing to the LFS file uploaded above, without needing git-lfs.
git -C "$W" checkout -q -b lfs-pointer main
printf 'version https://git-lfs.github.com/spec/v1\noid sha256:%s\nsize 6\n' "$OID" >"$W/large.txt"
git -C "$W" add -A && git -C "$W" commit -qm "Add large.txt with LFS"
git -C "$W" push -q "$ADA_HTTP" lfs-pointer
check "pull mirrors bring LFS files" is "$(post "$T/ada" /new name=lfs-mirror "mirror_url=$B/ada/engine.git" mirror_interval=3600)" 303
lfs_arrived() { for _ in $(seq 1 100); do [ "$(curl -s "$B/ada/$1/raw/lfs-pointer/large.txt")" = large ] && return 0; sleep 0.1; done; echo "no LFS file in $1"; return 1; }
check "the file is there" lfs_arrived lfs-mirror
check "create a repository to push to" is "$(post "$T/ada" /new name=engine-backup)" 303
check "add a push mirror" is "$(post "$T/ada" /ada/engine/settings/push-mirrors "url=$B/ada/engine-backup.git" username=ada "password=$RW")" 303
check "it gets the branches" synced engine-backup refs/heads/lfs-pointer "$(git -C "$W" rev-parse HEAD)"
check "and the LFS files" lfs_arrived engine-backup
check "the settings show it" contains "$(curl -s -b "$T/ada" "$B/ada/engine/settings")" "pushed"
git -C "$W" checkout -q -b pushed-later lfs-pointer && echo later >"$W/later2.txt" && git -C "$W" add -A && git -C "$W" commit -qm "Later"
git -C "$W" push -q "$ADA_HTTP" pushed-later
check "new pushes follow by themselves" synced engine-backup refs/heads/pushed-later "$(git -C "$W" rev-parse HEAD)"

echo "== backups and health"
check "health" is "$(curl -s "$B/healthz" | field '["status"]')" ok
check "backup a running server" bash -c "'$BIN' backup --data-dir '$T/data' '$T/backup' | grep -q 'Backed up'"
check "the copy has the repositories" git clone -q "$T/backup/repos/1.git" "$T/restored"
check "with their branches" is "$(git -C "$T/restored" rev-parse HEAD 2>/dev/null)" "$(git --git-dir "$T/data/repos/1.git" rev-parse HEAD)"
check "and the database" is "$(sqlite3 "$T/backup/keel.db" "SELECT COUNT(*) FROM users WHERE username = 'ada'")" 1
check "and the large files" bash -c "[ -f '$T/backup/lfs/${OID:0:2}/${OID:2:2}/$OID' ]"
check "and the wikis" is "$(git --git-dir "$T/backup/repos/1.wiki.git" show HEAD:Home.md 2>/dev/null)" "Welcome to the [[Getting Started]] guide."
check "metrics need the token" is "$(status "$B/metrics")" 401
METRICS=$(curl -s -H "Authorization: Bearer smoke-metrics" "$B/metrics")
check "metrics count requests by route" contains "$METRICS" 'keel_http_requests_total{method="GET",route="/{owner}/{repo}",status="200"}'
check "and git over SSH" contains "$METRICS" 'keel_git_operations_total{service="upload-pack",transport="ssh"}'
check "and mirror runs" contains "$METRICS" 'keel_mirror_runs_total{kind="push",outcome="ok"}'
check "and stored LFS files" contains "$METRICS" "keel_lfs_files "
check "it won't overwrite" bash -c "! '$BIN' backup --data-dir '$T/data' '$T/backup' 2>/dev/null"

echo "== downloads, releases and activity"
zipped() { python3 -c 'import io,sys,zipfile; print("\n".join(zipfile.ZipFile(io.BytesIO(sys.stdin.buffer.read())).namelist()))'; }
check "download a branch as .zip" contains "$(curl -s "$B/ada/engine/archive/main.zip" | zipped)" "engine-main/README.md"
check "and as .tar.gz" contains "$(curl -s "$B/ada/engine/archive/main.tar.gz" | tar -tz)" "engine-main/README.md"
check "named after repository and branch" contains "$(curl -s -D - -o /dev/null "$B/ada/engine/archive/main.zip")" 'filename="engine-main.zip"'
check "private repositories need access" is "$(status "$B/ada/secret/archive/main.zip")" 404
check "unknown revisions are 404" is "$(status "$B/ada/engine/archive/nope.zip")" 404
check "the clone menu offers downloads" contains "$(curl -s "$B/ada/engine")" "archive/main.zip"
check "readers can't create releases" is "$(post "$T/eve2" /ada/engine/releases tag=v9 target=main action=publish)" 403
REL_URL=$(curl -s -o /dev/null -w '%{redirect_url}' -b "$T/ada" -H "$O" --data-urlencode tag=v1.0 --data-urlencode target=main --data-urlencode "title=First release" --data-urlencode "notes=**Hello**" -d action=draft "$B/ada/engine/releases")
REL=$(grep -o 'releases/[0-9]*/edit' <<<"$REL_URL" | grep -o '[0-9]*')
check "draft a release" bash -c "[ -n '$REL' ]"
check "drafts are hidden" bash -c "! curl -s '$B/ada/engine/releases' | grep -q 'First release'"
check "the tag waits for publishing" is "$(git ls-remote "$B/ada/engine.git" refs/tags/v1.0 | wc -l)" 0
check "attach a file" is "$(curl -s -o /dev/null -w '%{http_code}' -b "$T/ada" -H "$O" -F "file=@$T/large.txt;filename=engine-linux.bin" "$B/ada/engine/releases/$REL/assets")" 303
check "the same name twice is refused" contains "$(curl -s -b "$T/ada" -H "$O" -F "file=@$T/large.txt;filename=engine-linux.bin" "$B/ada/engine/releases/$REL/assets")" "already has a file"
check "publish it" is "$(post "$T/ada" "/ada/engine/releases/$REL/edit" "title=First release" "notes=**Hello**" action=publish)" 303
check "which creates the tag" is "$(git ls-remote "$B/ada/engine.git" refs/tags/v1.0 | wc -l)" 1
RELPAGE=$(curl -s "$B/ada/engine/releases/tag/v1.0")
check "the release page" contains "$RELPAGE" "<strong>Hello</strong>"
check "lists the file" contains "$RELPAGE" "engine-linux.bin"
check "download the file" is "$(curl -s "$B/ada/engine/releases/download/v1.0/engine-linux.bin")" large
check "downloads are counted" contains "$(curl -s -b "$T/ada" "$B/ada/engine/releases/$REL/edit")" "1 download"
check "source code of the tag" contains "$(curl -s "$B/ada/engine/archive/v1.0.tar.gz" | tar -tz)" "engine-v1.0/README.md"
check "releases through the API" is "$(api_status "$RW" POST /repos/ada/engine/releases -d '{"tag_name":"v1.1","name":"Next","body":"Fixes"}')" 201
API_REL=$(api "$RW" GET /repos/ada/engine/releases/latest | field '["id"]')
check "the latest release" is "$(api "$RW" GET /repos/ada/engine/releases/latest | field '["tag_name"]')" v1.1
check "upload a file through the API" is "$(api "$RW" POST "/repos/ada/engine/releases/$API_REL/assets?name=notes.txt" --data-binary "release notes" | field '["size"]')" 13
check "it downloads" is "$(curl -s "$B/ada/engine/releases/download/v1.1/notes.txt")" "release notes"
ASSET=$(api "$RW" GET "/repos/ada/engine/releases/$API_REL" | field '["assets"][0]["id"]')
SREL=$(curl -s -o /dev/null -w '%{redirect_url}' -b "$T/ada" -H "$O" -d tag=s1 -d target=main -d action=draft "$B/ada/secret/releases" | grep -o 'releases/[0-9]*/edit' | grep -o '[0-9]*')
check "other repositories' release files can't be deleted" is "$(post "$T/ada" "/ada/secret/releases/$SREL/assets/$ASSET/delete" x=1)" 404
check "so the file is still there" is "$(curl -s "$B/ada/engine/releases/download/v1.1/notes.txt")" "release notes"
check "the tab counts releases" contains "$(curl -s "$B/ada/engine")" 'Releases <span class="count">2</span>'
HOME_FEED=$(curl -s -b "$T/ada" "$B/")
check "the home page shows activity" contains "$HOME_FEED" "released"
check "including pushes" contains "$HOME_FEED" "pushed"
feed_has() { # feed_has <text>: in ada's feed, following "Older" links
    local url="/"
    for _ in $(seq 1 20); do
        local page
        page=$(curl -s -b "$T/ada" "$B$url")
        grep -q -- "$1" <<<"$page" && return 0
        url=$(grep -o 'href="/?before=[0-9]*"' <<<"$page" | head -1 | cut -d'"' -f2)
        [ -z "$url" ] && break
    done
    echo "no '$1' in the feed"; return 1
}
check "and issues" feed_has "opened issue"
check "and merged pull requests" feed_has "merged pull request"
check "profiles show recent activity" contains "$(curl -s "$B/ada")" "Recent activity"
check "private activity stays private" bash -c "! curl -s -b '$T/eve2' '$B/ada' | grep -q 'ada/secret'"
check "older activity pages" contains "$(curl -s -b "$T/ada" "$B/?before=999999")" "Activity"

echo "== topics and explore"
check "set topics" is "$(post "$T/ada" /ada/engine/settings "description=A small engine" "topics=Rust, git cli")" 303
check "the header shows them" contains "$(curl -s "$B/ada/engine")" 'href="/explore?topic=rust"'
check "bad topics are refused" contains "$(curl -s -b "$T/ada" -H "$O" -d "topics=-bad" "$B/ada/engine/settings")" "valid topic"
check "explore by topic" contains "$(curl -s "$B/explore?topic=git")" 'href="/ada/engine"'
check "only those with it" bash -c "! curl -s '$B/explore?topic=git' | grep -q 'href=\"/bob/engine\"'"
check "explore everything" contains "$(curl -s "$B/explore?q=bob/engine")" 'href="/bob/engine"'
check "but not private repositories" bash -c "! curl -s '$B/explore' | grep -q 'href=\"/ada/secret\"'"
check "unless you can see them" contains "$(curl -s -b "$T/ada" "$B/explore")" 'href="/ada/secret"'
check "search" contains "$(curl -s "$B/explore?q=small+eng")" 'href="/ada/engine"'
check "popular topics" contains "$(curl -s "$B/explore")" 'topic-count'
check "topics in the API" is "$(curl -s "$API/repos/ada/engine/topics" | field '["names"]')" "['cli', 'git', 'rust']"
check "set them with the API" is "$(api "$RW" PUT /repos/ada/engine/topics -d '{"names":["rust","engine"]}' | field '["names"]')" "['engine', 'rust']"
check "search repositories with the API" is "$(curl -s "$API/search/repositories?q=topic:engine" | field '["items"][0]["full_name"]')" ada/engine

echo "== wikis"
check "empty wikis explain themselves" contains "$(curl -s -b "$T/ada" "$B/ada/engine-backup/wiki")" "Create the first page"
check "readers only see wikis that exist" bash -c "! curl -s -b '$T/eve2' '$B/ada/engine-backup' | grep -q 'engine-backup/wiki\"'"
check "the wiki tab" contains "$(curl -s -b "$T/eve2" "$B/ada/engine")" '/ada/engine/wiki"'
WIKI=$(curl -s "$B/ada/engine/wiki")
check "the front page" contains "$WIKI" "Welcome to the"
check "wiki links" contains "$WIKI" 'href="/ada/engine/wiki/Getting-Started"'
check "writers are offered to create missing pages" is "$(curl -s -o /dev/null -w '%{redirect_url}' -b "$T/ada" "$B/ada/engine/wiki/Getting-Started")" "$B/ada/engine/wiki/_new?title=Getting+Started"
check "others get a 404" is "$(status "$B/ada/engine/wiki/Getting-Started")" 404
check "readers can't edit" is "$(post "$T/eve2" /ada/engine/wiki title=Nope text=x)" 403
check "create a page" is "$(post "$T/ada" /ada/engine/wiki "title=Getting Started" "text=Run **cargo build**. See [[Home]].")" 303
check "it renders" contains "$(curl -s "$B/ada/engine/wiki/Getting-Started")" "<strong>cargo build</strong>"
check "names ignore case" is "$(curl -s -o /dev/null -w '%{redirect_url}' "$B/ada/engine/wiki/getting-started")" "$B/ada/engine/wiki/Getting-Started"
check "titles are unique" contains "$(curl -s -b "$T/ada" -H "$O" --data-urlencode "title=getting started" -d text=x "$B/ada/engine/wiki")" "already is a page"
check "and checked" contains "$(curl -s -b "$T/ada" -H "$O" -d "title=_pages&text=x" "$B/ada/engine/wiki")" "Only _Sidebar"
WBASE=$(curl -s -b "$T/ada" "$B/ada/engine/wiki/Getting-Started/edit" | grep -o 'name="base" value="[0-9a-f]*"' | cut -d'"' -f4)
check "edit it" is "$(post "$T/ada" /ada/engine/wiki/Getting-Started/edit "title=Getting Started" "text=Run **cargo test** too." "base=$WBASE")" 303
check "edits based on an old version are caught" contains "$(curl -s -b "$T/ada" -H "$O" --data-urlencode "title=Getting Started" -d "text=Mine&base=$WBASE" "$B/ada/engine/wiki/Getting-Started/edit")" "Someone else changed this page"
WHIST=$(curl -s "$B/ada/engine/wiki/Getting-Started/history")
check "history" contains "$WHIST" "Update Getting Started"
WOLD=$(grep -o 'rev=[0-9a-f]*' <<<"$WHIST" | tail -1 | cut -d= -f2)
check "old versions" contains "$(curl -s "$B/ada/engine/wiki/Getting-Started?rev=$WOLD")" "cargo build"
check "rename it" is "$(post "$T/ada" /ada/engine/wiki/Getting-Started/edit "title=Setup" "text=Run **cargo test** too.")" 303
check "the old name is gone" is "$(status "$B/ada/engine/wiki/Getting-Started")" 404
check "add a sidebar" is "$(post "$T/ada" /ada/engine/wiki title=_Sidebar "text=[[Setup]]")" 303
check "it's shown with pages" contains "$(curl -s "$B/ada/engine/wiki")" 'wiki-sidebar'
check "list pages" contains "$(curl -s "$B/ada/engine/wiki/_pages")" '>Setup</a>'
check "clone the wiki" git clone -q "http://ada:password-ada@127.0.0.1:$HTTP_PORT/ada/engine.wiki.git" "$T/wiki"
check "pages are Markdown files" is "$(cat "$T/wiki/Setup.md" 2>/dev/null)" "Run **cargo test** too."
mkdir -p "$T/wiki/guides" "$T/wiki/images"
printf '# Deploying\n\n![diagram](images/d.svg)\n' >"$T/wiki/guides/Deploying.md"
echo '<svg xmlns="http://www.w3.org/2000/svg"/>' >"$T/wiki/images/d.svg"
git -C "$T/wiki" add -A && git -C "$T/wiki" commit -qm "Add a deploying guide"
check "push to it" git -C "$T/wiki" push -q origin HEAD
WDEPLOY=$(curl -s "$B/ada/engine/wiki/Deploying")
check "pushed pages appear" contains "$WDEPLOY" "<h1"
check "with their images" contains "$WDEPLOY" 'src="/ada/engine/wiki/_raw/images/d.svg"'
check "which are sandboxed" contains "$(curl -s -D - -o /dev/null "$B/ada/engine/wiki/_raw/images/d.svg")" "sandbox"
check "pushes show in the feed" feed_has 'wiki/Deploying">Deploying'
check "clone over SSH" git clone -q "ssh://git@127.0.0.1:$SSH_PORT/ada/engine.wiki.git" "$T/wiki-ssh"
check "readers can't push" bash -c "! git -C '$T/wiki' push -q 'http://eve:password-reset@127.0.0.1:$HTTP_PORT/ada/engine.wiki.git' HEAD:refs/heads/x 2>/dev/null"
check "missing wikis can't be cloned" bash -c "! git clone -q '$B/ada/engine-backup.wiki.git' '$T/nowiki' 2>/dev/null"
check "private wikis stay private" is "$(status -b "$T/eve2" "$B/ada/secret/wiki")" 404
check "delete a page" is "$(post "$T/ada" /ada/engine/wiki/Setup/delete x=1)" 303
check "it's gone" is "$(status "$B/ada/engine/wiki/Setup")" 404
check "from clones too" bash -c "git -C '$T/wiki' pull -q && [ ! -e '$T/wiki/Setup.md' ]"

echo "== changing files in the browser"
check "create a repository" is "$(post "$T/ada" /new name=tracker)" 303
check "empty repositories offer adding files" contains "$(curl -s -b "$T/ada" "$B/ada/tracker")" '/ada/tracker/new/main'
check "create the first file" is "$(post "$T/ada" /ada/tracker/new/main path=README.md "text=# Tracker" summary=)" 303
check "it's committed" is "$(curl -s "$B/ada/tracker/raw/main/README.md")" "# Tracker"
check "with a default message" contains "$(curl -s "$B/ada/tracker/commits/main")" "Create README.md"
check "files offer editing" contains "$(curl -s -b "$T/ada" "$B/ada/tracker/blob/main/README.md")" '/ada/tracker/edit/main/README.md'
check "but not to readers" bash -c "! curl -s -b '$T/eve2' '$B/ada/tracker/blob/main/README.md' | grep -q '/edit/main'"
check "readers can't edit" is "$(status -b "$T/eve2" "$B/ada/tracker/edit/main/README.md")" 403
TB1=$(git ls-remote "$B/ada/tracker.git" refs/heads/main | cut -f1)
check "edit it" is "$(post "$T/ada" /ada/tracker/edit/main/README.md path=README.md "text="$'# Tracker\r\nAll the issues\r\n' "base=$TB1" "summary=Describe it")" 303
check "browsers' line endings are fixed" is "$(curl -s "$B/ada/tracker/raw/main/README.md" | od -c | grep -c '\\r')" 0
check "edits based on old versions are refused" contains "$(curl -s -b "$T/ada" -H "$O" -d path=README.md -d text=x -d "base=$TB1" "$B/ada/tracker/edit/main/README.md")" "Someone changed"
TB2=$(git ls-remote "$B/ada/tracker.git" refs/heads/main | cut -f1)
check "rename it" is "$(post "$T/ada" /ada/tracker/edit/main/README.md path=docs/README.md "text=# Tracker" "base=$TB2")" 303
check "it moved" is "$(curl -s "$B/ada/tracker/raw/main/docs/README.md")" "# Tracker"
check "files and folders don't mix" contains "$(curl -s -b "$T/ada" -H "$O" -d path=docs -d text=x "$B/ada/tracker/new/main")" "is a folder"
check "add issue templates" is "$(post "$T/ada" /ada/tracker/new/main path=.github/ISSUE_TEMPLATE/bug.md "text="$'---\nname: Bug report\nabout: Something is broken\ntitle: "[Bug] "\nlabels: bug\n---\nWhat happened?\n')" 303
check "two of them" is "$(post "$T/ada" /ada/tracker/new/main path=.github/ISSUE_TEMPLATE/idea.md "text="$'---\nname: Idea\n---\nWhat would help?')" 303
check "and a pull request template" is "$(post "$T/ada" /ada/tracker/new/main path=.github/pull_request_template.md "text=## What changed")" 303
check "upload files" is "$(curl -s -o /dev/null -w '%{http_code}' -b "$T/ada" -H "$O" -F "files=@$T/large.txt;filename=data.txt" -F "files=@$T/large.txt;filename=copy.txt" -F summary="Add data" "$B/ada/tracker/upload/main/assets")" 303
check "they're there" is "$(curl -s "$B/ada/tracker/raw/main/assets/data.txt")" large
check "folders offer adding files" contains "$(curl -s -b "$T/ada" "$B/ada/tracker/tree/main/assets")" '/ada/tracker/upload/main/assets'
check "delete a file" is "$(post "$T/ada" /ada/tracker/delete/main/assets/copy.txt summary=)" 303
check "it's gone" is "$(status "$B/ada/tracker/raw/main/assets/copy.txt")" 404
check "web commits show in the feed" feed_has 'pushed 1 commit to <a href="/ada/tracker/tree/main">main</a>'
check "protected branches need a pull request" contains "$(curl -s -b "$T/ada" -H "$O" -d path=README.md -d text=x -d target=direct "$B/ada/engine/edit/main/README.md")" "protected"
check "also as a \"new branch\" named like it" contains "$(curl -s -b "$T/ada" -H "$O" -d path=README.md -d text=x -d target=branch -d new_branch=main "$B/ada/engine/edit/main/README.md")" "protected"
check "so commit to a new branch" is "$(curl -s -o /dev/null -w '%{redirect_url}' -b "$T/ada" -H "$O" -d path=README.md -d text=Edited -d target=branch -d new_branch=ada-readme "$B/ada/engine/edit/main/README.md")" "$B/ada/engine/compare/main...ada-readme"
check "which starts a pull request" contains "$(curl -s -b "$T/ada" "$B/ada/engine/compare/main...ada-readme")" "Create pull request"
TB3=$(git ls-remote "$B/ada/tracker.git" refs/heads/main | cut -f1)
check "on any branch" is "$(curl -s -o /dev/null -w '%{redirect_url}' -b "$T/ada" -H "$O" -d path=docs/README.md -d text=Fixed -d "base=$TB3" -d target=branch -d new_branch=fix "$B/ada/tracker/edit/main/docs/README.md")" "$B/ada/tracker/compare/main...fix"
check "pull requests start with the template" contains "$(curl -s -b "$T/ada" "$B/ada/tracker/compare/main...fix")" "## What changed"

echo "== issue templates"
check "choose a template" contains "$(curl -s -b "$T/bob" "$B/ada/tracker/issues/new")" "Something is broken"
check "or none" contains "$(curl -s -b "$T/bob" "$B/ada/tracker/issues/new")" "Open a blank issue"
check "templates fill in the form" contains "$(curl -s -b "$T/bob" "$B/ada/tracker/issues/new?template=bug.md")" "What happened?"
check "open an issue from one" is "$(post "$T/bob" /ada/tracker/issues "title=[Bug] Crash" "body=It crashed" template=bug.md)" 303
check "it gets the template's labels" contains "$(curl -s "$B/ada/tracker/issues/1")" '>bug</a>'
check "blank issues" contains "$(curl -s -b "$T/bob" "$B/ada/tracker/issues/new?template=blank")" 'name="title" value=""'

echo "== stars and reactions"
check "star a repository" is "$(post "$T/bob" /ada/engine/star to=on)" 303
check "the header counts stars" contains "$(curl -s "$B/ada/engine")" 'aria-label="Stars: 1"'
check "stargazers" contains "$(curl -s "$B/ada/engine/stargazers")" '<strong>bob</strong>'
check "profiles list stars" contains "$(curl -s "$B/bob?tab=stars")" 'href="/ada/engine"'
check "starring shows in the feed" contains "$(curl -s "$B/bob")" 'starred <a href="/ada/engine">'
check "explore sorts by stars" is "$(curl -s "$B/explore?sort=stars" | grep -o 'class="repo-link" href="[^"]*"' | head -1)" 'class="repo-link" href="/ada/engine"'
check "star with the API" is "$(api_status "$RW" PUT /user/starred/ada/tracker)" 204
check "check a star" is "$(api_status "$RW" GET /user/starred/ada/tracker)" 204
check "your stars" is "$(api "$RW" GET /user/starred | field '[0]["full_name"]')" ada/tracker
check "stargazers in the API" is "$(curl -s "$API/repos/ada/engine/stargazers" | field '[0]["login"]')" bob
check "star counts in the API" is "$(curl -s "$API/repos/ada/engine" | field '["stargazers_count"]')" 1
check "unstar with the API" is "$(api_status "$RW" DELETE /user/starred/ada/tracker)" 204
check "not starred any more" is "$(api_status "$RW" GET /user/starred/ada/tracker)" 404
RI=$(curl -s -o /dev/null -w '%{redirect_url}' -b "$T/bob" -H "$O" -d "title=React to me&body=Hi" "$B/ada/engine/issues" | grep -o '[0-9]*$')
check "react to an issue" is "$(post "$T/ada" "/ada/engine/issues/$RI/reactions" content=+1)" 303
check "reactions show" contains "$(curl -s "$B/ada/engine/issues/$RI")" "ada reacted with 👍"
check "reacting again takes it back" is "$(post "$T/ada" "/ada/engine/issues/$RI/reactions" content=+1)" 303
check "it's gone" bash -c "! curl -s '$B/ada/engine/issues/$RI' | grep -q 'reacted with'"
RC=$(curl -s -o /dev/null -w '%{redirect_url}' -b "$T/bob" -H "$O" -d "body=Me too" "$B/ada/engine/issues/$RI/comments" | grep -o '[0-9]*$')
check "readers react to comments" is "$(post "$T/eve2" "/ada/engine/issues/$RI/reactions" content=heart "comment=$RC")" 303
check "unknown reactions are refused" is "$(post "$T/ada" "/ada/engine/issues/$RI/reactions" content=party)" 400
check "react with the API" is "$(api_status "$RW" POST "/repos/ada/engine/issues/$RI/reactions" -d '{"content":"rocket"}')" 201
check "twice is the same" is "$(api_status "$RW" POST "/repos/ada/engine/issues/$RI/reactions" -d '{"content":"rocket"}')" 200
check "issues count reactions" is "$(curl -s "$API/repos/ada/engine/issues/$RI" | field '["reactions"]["rocket"]')" 1
check "comment reactions in the API" is "$(curl -s "$API/repos/ada/engine/issues/comments/$RC/reactions" | field '[0]["user"]["login"]')" eve
check "and on comments" is "$(curl -s "$API/repos/ada/engine/issues/$RI/comments" | field '[0]["reactions"]["heart"]')" 1
RR=$(curl -s "$API/repos/ada/engine/issues/$RI/reactions" | field '[0]["id"]')
check "delete your reaction" is "$(api_status "$RW" DELETE "/repos/ada/engine/issues/$RI/reactions/$RR")" 204

echo "== issue search"
check "search a repository's issues" contains "$(curl -s "$B/ada/engine/issues?q=hook")" "Hook me"
check "with qualifiers" contains "$(curl -s "$B/ada/engine/issues?q=is:open+author:bob+hook")" "Hook me"
check "that narrow it down" contains "$(curl -s "$B/ada/engine/issues?q=author:ada+hook")" "Nothing matches"
check "across repositories" contains "$(curl -s "$B/issues?q=is:issue+hook")" 'issue-repo" href="/ada/engine"'
check "your issues" contains "$(curl -s -b "$T/bob" "$B/issues")" "Hook me"
check "search issues with the API" is "$(curl -s "$API/search/issues?q=hook+repo:ada/engine" | field '["items"][0]["title"]')" "Hook me"
check "private issues stay private" is "$(curl -s "$API/search/issues?q=repo:ada/secret" | field '["total_count"]')" 0

echo "== avatars and pins"
check "a generated avatar" is "$(curl -s -o /dev/null -w '%{content_type}' "$B/avatars/ada")" "image/svg+xml"
printf '\x89PNG\r\n\x1a\n\0\0\0\rIHDR\0\0\0\1\0\0\0\1' >"$T/a.png"
check "upload a picture" is "$(curl -s -o /dev/null -w '%{http_code}' -b "$T/ada" -H "$O" -F "avatar=@$T/a.png;type=image/png" "$B/settings/profile/avatar")" 303
check "it's served" is "$(curl -s "$B/avatars/ada" | cmp - "$T/a.png" && echo same)" same
ETAG=$(curl -s -D - -o /dev/null "$B/avatars/ada" | grep -i '^etag' | cut -d' ' -f2 | tr -d '\r')
check "and cached" is "$(curl -s -o /dev/null -w '%{http_code}' -H "If-None-Match: $ETAG" "$B/avatars/ada")" 304
check "only images" contains "$(curl -s -b "$T/ada" -H "$O" -F "avatar=@$T/large.txt" "$B/settings/profile/avatar")" "must be PNG"
check "remove it" is "$(post "$T/ada" /settings/profile/avatar/delete x=1)" 303
check "the pattern is back" is "$(curl -s -o /dev/null -w '%{content_type}' "$B/avatars/ada")" "image/svg+xml"
EID=$(curl -s "$API/repos/ada/engine" | field '["id"]')
SID=$(curl -s -b "$T/ada" "$B/settings/profile" | grep -o 'value="[0-9]*"> secret' | grep -o '[0-9]*')
check "pin repositories" is "$(post "$T/ada" /settings/profile/pins "repo=$SID" "repo=$EID")" 303
check "the profile shows them" contains "$(curl -s -b "$T/ada" "$B/ada")" 'pinned-card"><a class="repo-link" href="/ada/secret"'
check "private ones only to those who can see them" bash -c "curl -s -b '$T/eve2' '$B/ada' | grep -q 'pinned-card\"><a class=\"repo-link\" href=\"/ada/engine\"' && ! curl -s -b '$T/eve2' '$B/ada' | grep -q 'href=\"/ada/secret\"'"
check "at most six" contains "$(curl -s -b "$T/ada" -H "$O" -d "repo=1&repo=2&repo=3&repo=4&repo=5&repo=6&repo=7" "$B/settings/profile/pins")" "At most 6"
check "organizations have pictures too" is "$(status -b "$T/ada" "$B/org/acme/settings/profile")" 200

echo "== importing"
GITEA_PORT=$((HTTP_PORT + 5))
SRC_MAIN=$(git ls-remote "$B/ada/engine.git" refs/heads/main | cut -f1)
python3 - "$GITEA_PORT" "$HTTP_PORT" "$SRC_MAIN" <<'PY' &
import http.client, http.server, json, sys
port, keel, sha = int(sys.argv[1]), int(sys.argv[2]), sys.argv[3]
web = f"http://127.0.0.1:{port}"
user = lambda login: {"login": login, "html_url": f"{web}/{login}"}
t = "2024-01-02T03:04:05Z"
api = {
    "labels": [{"name": "bug", "color": "ee0701", "description": "Broken"}, {"name": "perf", "color": "0075ca", "description": ""}],
    "milestones": [{"title": "v2", "description": "", "state": "open", "due_on": None, "created_at": t}],
    "pulls": [{"number": 2, "title": "Faster start", "body": "Speeds it up", "state": "closed", "user": user("octo"),
               "created_at": t, "updated_at": t, "closed_at": t, "merged_at": t, "merge_commit_sha": sha,
               "base": {"ref": "main"}, "head": {"ref": "fast", "sha": sha}, "labels": [{"name": "perf"}]}],
    "issues": [{"number": 1, "title": "Crash on start", "body": "It crashes", "state": "open", "user": user("octo"),
                "created_at": t, "updated_at": t, "labels": [{"name": "bug"}], "milestone": {"title": "v2"}, "pull_request": None}],
    "issues/comments": [{"issue_url": f"{web}/octo/engine/issues/1", "body": "Same here", "user": user("hubot"), "created_at": t}],
    "pulls/2/reviews": [{"state": "APPROVED", "body": "Nice", "user": user("hubot"), "submitted_at": t, "commit_id": sha}],
    "releases": [{"tag_name": "v1.0", "name": "Imported release", "body": "Notes", "draft": False, "prerelease": False,
                  "author": user("octo"), "created_at": t, "published_at": t,
                  "assets": [{"id": 1, "name": "notes.txt", "size": 5, "content_type": "text/plain",
                              "browser_download_url": f"{web}/dl/notes.txt"}]}],
}
class H(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    def reply(self, code, body, kind="application/json"):
        self.send_response(code)
        self.send_header("Content-Type", kind)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)
    def proxy(self):
        # git (and Git LFS) go to keel's ada/engine.
        length = int(self.headers.get("Content-Length") or 0)
        body = self.rfile.read(length) if length else None
        path = self.path.replace("/octo/engine", "/ada/engine", 1)
        c = http.client.HTTPConnection("127.0.0.1", keel)
        headers = {k: v for k, v in self.headers.items() if k.lower() not in ("host", "content-length")}
        c.request(self.command, path, body=body, headers=headers)
        r = c.getresponse()
        data = r.read()
        self.send_response(r.status)
        for k, v in r.getheaders():
            if k.lower() not in ("transfer-encoding", "content-length", "connection"):
                self.send_header(k, v)
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)
    def do_GET(self):
        path, _, query = self.path.partition("?")
        prefix = "/api/v1/repos/octo/engine/"
        if path.startswith(prefix):
            key = path[len(prefix):]
            first = "page=1" in query or "page=" not in query
            return self.reply(200, json.dumps(api.get(key, []) if first else []).encode())
        if path == "/dl/notes.txt":
            return self.reply(200, b"hello", "text/plain")
        self.proxy()
    do_POST = proxy
    def log_message(self, *a): pass
http.server.ThreadingHTTPServer(("127.0.0.1", port), H).serve_forever()
PY
GITEA_PID=$!
for _ in $(seq 1 50); do curl -s -o /dev/null "http://127.0.0.1:$GITEA_PORT/api/v1/repos/octo/engine/labels" && break; sleep 0.1; done
check "the import page" contains "$(curl -s -b "$T/ada" "$B/new/import")" "Import a repository"
check "only from real addresses" contains "$(curl -s -b "$T/ada" -H "$O" -d "url=not a url" "$B/new/import")" "flash-error"
check "start an import" is "$(post "$T/ada" /new/import "url=http://127.0.0.1:$GITEA_PORT/octo/engine" name=imported issues=on releases=on wiki=on)" 303
imported() { for _ in $(seq 1 300); do s=$(curl -s "$API/repos/ada/imported/import" | field '["status"]' 2>/dev/null); [ "$s" = done ] || [ "$s" = failed ] && break; sleep 0.1; done; curl -s -H "Authorization: Bearer $RW" "$API/repos/ada/imported/import"; }
IMPORT=$(imported)
check "it finishes" is "$(field '["status"]' <<<"$IMPORT")" done
check "without problems" is "$(field '["error"]' <<<"$IMPORT")" None
check "with the branches" is "$(git ls-remote "$B/ada/imported.git" refs/heads/main | cut -f1)" "$SRC_MAIN"
check "and Git LFS files" lfs_arrived imported
IISSUE=$(curl -s "$B/ada/imported/issues/1")
check "issues keep their numbers" contains "$IISSUE" "Crash on start"
check "authors link to where they are" contains "$IISSUE" "href=\"http://127.0.0.1:$GITEA_PORT/octo\""
check "comments come along" contains "$IISSUE" "Same here"
check "marked as imported" contains "$IISSUE" ">imported</span>"
check "with labels" contains "$IISSUE" '>bug</a>'
check "and milestones" contains "$IISSUE" "v2"
IPULL=$(curl -s "$B/ada/imported/pull/2")
check "pull requests too" contains "$IPULL" "Faster start"
check "merged ones stay merged" contains "$IPULL" "state-merged"
check "with reviews" contains "$IPULL" "approved these changes"
check "releases" contains "$(curl -s "$B/ada/imported/releases")" "Imported release"
check "with their files" is "$(curl -s "$B/ada/imported/releases/download/v1.0/notes.txt")" hello
check "and the wiki" contains "$(curl -s "$B/ada/imported/wiki")" "Welcome to the"
check "new issues continue the numbering" is "$(curl -s -o /dev/null -w '%{redirect_url}' -b "$T/ada" -H "$O" -d "title=After the import" "$B/ada/imported/issues")" "$B/ada/imported/issues/3"
check "nobody was notified" bash -c "! curl -s -b '$T/ada' '$B/notifications' | grep -q 'Crash on start'"
check "imports with the API" is "$(api_status "$RW" POST /repos/migrate -d "{\"clone_addr\":\"http://127.0.0.1:$GITEA_PORT/octo/engine\",\"repo_name\":\"imported2\",\"issues\":false,\"releases\":false,\"wiki\":false}")" 201
kill "$GITEA_PID" 2>/dev/null

echo "== drafts and auto-merge"
check "create a repository for it" is "$(post "$T/ada" /new name=flow)" 303
FLOW="http://ada:password-ada@127.0.0.1:$HTTP_PORT/ada/flow.git"
FW=$T/flow
git init -q -b main "$FW" && echo one >"$FW/a.txt" && git -C "$FW" add -A && git -C "$FW" commit -qm one && git -C "$FW" push -q "$FLOW" main
git -C "$FW" checkout -q -b wip && echo two >"$FW/b.txt" && git -C "$FW" add -A && git -C "$FW" commit -qm two && git -C "$FW" push -q "$FLOW" wip
WIP=$(git -C "$FW" rev-parse HEAD)
check "open a draft" is "$(post "$T/ada" /ada/flow/pulls base=main head=wip title="Work in progress" draft=1)" 303
check "it says so" contains "$(curl -s -b "$T/ada" "$B/ada/flow/pull/1")" "still a draft"
check "lists mark it" contains "$(curl -s "$B/ada/flow/pulls")" "state-draft"
check "search finds drafts" contains "$(curl -s "$B/ada/flow/pulls?q=is:draft")" "Work in progress"
check "drafts can't be merged" contains "$(curl -s -b "$T/ada" -H "$O" -d style=merge "$B/ada/flow/pull/1/merge")" "still a draft"
check "the API says it's a draft" is "$(curl -s "$API/repos/ada/flow/pulls/1" | field '["draft"]')" True
check "bob can't mark it ready" contains "$(curl -s -b "$T/bob" -H "$O" -d x=1 "$B/ada/flow/pull/1/ready")" "Only the author"
check "ada marks it ready" is "$(post "$T/ada" /ada/flow/pull/1/ready x=1)" 303
check "the timeline shows it" contains "$(curl -s "$B/ada/flow/pull/1")" "marked this pull request as ready for review"
check "require a check" is "$(post "$T/ada" /ada/flow/settings/branches pattern=main require_pull=on required_approvals=0 required_checks=ci)" 303
check "auto-merge is offered" contains "$(curl -s -b "$T/ada" "$B/ada/flow/pull/1")" "Enable auto-merge"
check "turn it on" is "$(post "$T/ada" /ada/flow/pull/1/auto-merge style=squash delete_branch=on)" 303
check "it waits for the check" contains "$(curl -s "$B/ada/flow/pull/1")" "Auto-merge is on"
check "the API shows it" is "$(curl -s "$API/repos/ada/flow/pulls/1" | field '["auto_merge"]["merge_method"]')" squash
check "CI passes" is "$(api_status "$RW" POST "/repos/ada/flow/statuses/$WIP" -d '{"state":"success","context":"ci"}')" 201
automerged() { for _ in $(seq 1 50); do curl -s "$B/ada/flow/pull/1" | grep -q "state-badge state-merged" && return 0; sleep 0.1; done; return 1; }
check "and it merges by itself" automerged
check "as whoever turned it on" contains "$(curl -s "$B/ada/flow/pull/1")" "<strong>ada</strong> merged"
git -C "$FW" fetch -q "$FLOW" main
check "squashed" is "$(git -C "$FW" log -1 --format=%s FETCH_HEAD)" "Work in progress (#1)"
check "and the branch is gone" is "$(git ls-remote "$B/ada/flow.git" refs/heads/wip | wc -l)" 0
git -C "$FW" checkout -q -b later FETCH_HEAD && echo three >"$FW/c.txt" && git -C "$FW" add -A && git -C "$FW" commit -qm three && git -C "$FW" push -q "$FLOW" later
check "open drafts with the API" is "$(api "$RW" POST /repos/ada/flow/pulls -d '{"title":"Later","head":"later","base":"main","draft":true}' | field '["draft"]')" True
check "turning it off again" bash -c "[ \"\$(curl -s -o /dev/null -w '%{http_code}' -b '$T/ada' -H '$O' -d x=1 '$B/ada/flow/pull/2/ready')\" = 303 ] && [ \"\$(curl -s -o /dev/null -w '%{http_code}' -b '$T/ada' -H '$O' -d style=merge '$B/ada/flow/pull/2/auto-merge')\" = 303 ] && [ \"\$(curl -s -o /dev/null -w '%{http_code}' -b '$T/ada' -H '$O' -d x=1 '$B/ada/flow/pull/2/auto-merge/disable')\" = 303 ]"
check "is in the timeline" contains "$(curl -s "$B/ada/flow/pull/2")" "disabled auto-merge"

echo "== repository templates"
check "make flow a template" is "$(post "$T/ada" /ada/flow/settings description=Starter template=on)" 303
check "people can use it" contains "$(curl -s -b "$T/bob" "$B/ada/flow")" "Use this template"
check "the form picks it" contains "$(curl -s -b "$T/bob" "$B/new?template=ada/flow")" "<option selected>ada/flow</option>"
check "bob starts from it" is "$(post "$T/bob" /new template=ada/flow name=myflow)" 303
check "with its files" is "$(curl -s "$B/bob/myflow/raw/main/b.txt")" two
git clone -q "$B/bob/myflow.git" "$T/myflow"
check "but not its history" is "$(git -C "$T/myflow" rev-list --count HEAD)" 1
check "nor its other branches" is "$(git -C "$T/myflow" branch -r | grep -vc HEAD)" 1
check "the commit is bob's" is "$(git -C "$T/myflow" log -1 --format='%an %s')" "bob Initial commit"
check "it says where it's from" contains "$(curl -s "$B/bob/myflow")" 'generated from <a href="/ada/flow">'
check "with the API, with all branches" is "$(api "$RW" POST /repos/ada/flow/generate -d '{"name":"flow2","include_all_branches":true}' | field '["full_name"]')" ada/flow2
check "which are all there" is "$(git ls-remote "$B/ada/flow2.git" 'refs/heads/*' | wc -l)" 2
check "other repositories aren't templates" is "$(api_status "$RW" POST /repos/ada/tracker/generate -d '{"name":"x"}')" 422
check "private templates stay private" is "$(post "$T/eve2" /new template=ada/secret name=peek)" 404

echo "== insights"
post "$T/ada" /new name=blank >/dev/null
check "contributors graph" contains "$(curl -s "$B/ada/engine/graphs/contributors")" '<svg class="chart"'
check "linked to accounts" contains "$(curl -s "$B/ada/engine/graphs/contributors")" '<a href="/ada">Ada</a>'
check "commit activity" contains "$(curl -s "$B/ada/engine/graphs/commit-activity")" "in the last year"
check "with a bar per week" contains "$(curl -s "$B/ada/engine/graphs/commit-activity")" "<title>"
check "empty repositories" contains "$(curl -s -b "$T/ada" "$B/ada/blank/graphs/contributors")" "No commits yet"
check "private ones are hidden" is "$(status "$B/ada/secret/graphs/contributors")" 404

echo "== theme"
check "pages follow the system" bash -c "! curl -s '$B/explore' | grep -q 'data-theme'"
check "visitors pick one" is "$(curl -s -o /dev/null -w '%{redirect_url}' -c "$T/visitor" -H "$O" -H "Referer: $B/explore" -d theme=dark "$B/theme")" "$B/explore"
check "it's remembered" contains "$(curl -s -b "$T/visitor" "$B/explore")" '<html lang="en" data-theme="dark">'
check "the switch shows it" contains "$(curl -s -b "$T/visitor" "$B/explore")" 'value="dark" aria-pressed="true"'
check "the script needs no redirect" is "$(curl -s -o /dev/null -w '%{http_code}' -H "$O" -d theme=light -d js=1 "$B/theme")" 204
check "accounts keep theirs" is "$(post "$T/bob" /theme theme=light)" 303
grep -v keel_theme "$T/bob" >"$T/bob-elsewhere"
check "on other devices too" contains "$(curl -s -b "$T/bob-elsewhere" "$B/")" 'data-theme="light"'
check "back to the system's" is "$(post "$T/bob" /theme theme=auto)" 303
CSS=$(curl -s "$B/" | grep -o 'href="/static/style.css[^"]*"' | cut -d'"' -f2)
check "the styles know both" bash -c "curl -s '$B$CSS' | grep -q ':root\[data-theme=\"dark\"\] .hl-comment'"

echo "== rebase and merge"
check "a repository for it" is "$(post "$T/ada" /new name=linear)" 303
LIN="http://ada:password-ada@127.0.0.1:$HTTP_PORT/ada/linear.git"
LW=$T/linear
git init -q -b main "$LW" && echo one >"$LW/a.txt" && git -C "$LW" add -A && git -C "$LW" commit -qm one && git -C "$LW" push -q "$LIN" main
git -C "$LW" checkout -q -b topic && echo two >"$LW/b.txt" && git -C "$LW" add -A && git -C "$LW" commit -qm two
echo three >"$LW/c.txt" && git -C "$LW" add -A && git -C "$LW" commit -qm three && git -C "$LW" push -q "$LIN" topic
git -C "$LW" checkout -q main && echo moved >"$LW/m.txt" && git -C "$LW" add -A && git -C "$LW" commit -qm "main moves" && git -C "$LW" push -q "$LIN" main
check "open a pull request" is "$(post "$T/ada" /ada/linear/pulls base=main head=topic title=Topic)" 303
check "rebasing is offered" contains "$(curl -s -b "$T/ada" "$B/ada/linear/pull/1")" '<option value="rebase">Rebase and merge</option>'
check "rebase and merge" is "$(post "$T/ada" /ada/linear/pull/1/merge style=rebase)" 303
git -C "$LW" fetch -q "$LIN" main
check "the history is linear" is "$(git -C "$LW" log --format=%s FETCH_HEAD | tr '\n' ,)" "three,two,main moves,one,"
check "the commits are new" bash -c "[ \"\$(git -C '$LW' rev-parse FETCH_HEAD)\" != \"\$(git -C '$LW' rev-parse topic)\" ]"
check "it's merged" contains "$(curl -s "$B/ada/linear/pull/1")" "state-badge state-merged"
# Merges cleanly (the branch undoes its own change), but its first commit
# can't be replayed.
git -C "$LW" checkout -q -b undo FETCH_HEAD && echo mine >"$LW/a.txt" && git -C "$LW" commit -qam mine && echo one >"$LW/a.txt" && git -C "$LW" commit -qam undo
git -C "$LW" push -q "$LIN" undo
git -C "$LW" checkout -q -b theirs FETCH_HEAD && echo theirs >"$LW/a.txt" && git -C "$LW" commit -qam theirs && git -C "$LW" push -q "$LIN" theirs:main
check "another one" is "$(post "$T/ada" /ada/linear/pulls base=main head=undo title=Undo)" 303
check "says why it can't rebase" contains "$(curl -s -b "$T/ada" -H "$O" -d style=rebase "$B/ada/linear/pull/2/merge")" "conflicts in a.txt"
check "the API rebases too" contains "$(api "$RW" PUT /repos/ada/linear/pulls/2/merge -d '{"merge_method":"rebase"}')" "conflicts in a.txt"
check "and knows the methods" contains "$(api "$RW" PUT /repos/ada/linear/pulls/2/merge -d '{"merge_method":"octopus"}')" "merge, squash or rebase"

echo "== verified commits"
git -C "$LW" config gpg.format ssh && git -C "$LW" config user.signingkey "$T/key.pub"
git -C "$LW" checkout -q -b signed theirs && git -C "$LW" commit -S -q --allow-empty -m "signed with ada's key"
SIGNED=$(git -C "$LW" rev-parse HEAD)
ssh-keygen -q -t ed25519 -N "" -C stranger -f "$T/stranger"
git -C "$LW" -c user.signingkey="$T/stranger.pub" commit -S -q --allow-empty -m "signed with a stranger's key"
GIT_COMMITTER_EMAIL=bob@example.com git -C "$LW" commit -S -q --allow-empty -m "signed by ada, committed as bob"
git -C "$LW" push -q "$LIN" signed
check "signed commits are verified" contains "$(curl -s "$B/ada/linear/commits/signed")" '<span class="badge badge-verified" title="Signed with ada’s SSH key SHA256:'
check "unknown keys aren't" contains "$(curl -s "$B/ada/linear/commits/signed")" "which nobody here has added"
check "nor someone else's email" contains "$(curl -s "$B/ada/linear/commits/signed")" "the committer email isn’t their verified email"
check "unsigned ones show nothing" bash -c "! curl -s '$B/ada/linear/commit/$(git -C "$LW" rev-parse theirs)' | grep -q 'badge-verified\|badge-unverified'"
check "the commit page says how" contains "$(curl -s "$B/ada/linear/commit/$SIGNED")" "Signed with ada’s SSH key"
check "the API too" is "$(curl -s "$API/repos/ada/linear/commits/$SIGNED" | field '["commit"]["verification"]["reason"]')" valid
check "and lists" is "$(curl -s "$API/repos/ada/linear/commits?sha=signed&per_page=3" | field '[1]["commit"]["verification"]["reason"]')" unknown_key
check "SSH keys over the API" is "$(api "$RW" GET /user/keys | field '[0]["fingerprint"][:7]')" "SHA256:"
check "GPG keys page" contains "$(curl -s -b "$T/ada" "$B/settings/keys")" "GPG keys"
check "nonsense isn't a GPG key" contains "$(curl -s -b "$T/ada" -H "$O" --data-urlencode key=nonsense --data-urlencode password=password-ada "$B/settings/gpg-keys")" "doesn’t look like a GPG public key"
if command -v gpg >/dev/null; then
    export GNUPGHOME=$T/gnupg && mkdir -m 700 "$GNUPGHOME"
    gpg --batch --quiet --passphrase "" --quick-gen-key "Ada <ada@example.com>" ed25519 sign never
    check "add a GPG key" is "$(post "$T/ada" /settings/gpg-keys "key=$(gpg --armor --export ada@example.com)" password=password-ada)" 303
    check "it lists its emails" contains "$(curl -s -b "$T/ada" "$B/settings/keys")" "ada@example.com"
    git -C "$LW" -c gpg.format=openpgp -c user.signingkey=ada@example.com commit -S -q --allow-empty -m "signed with GPG"
    git -C "$LW" push -q "$LIN" signed
    check "GPG signatures verify" is "$(curl -s "$API/repos/ada/linear/commits/signed" | field '["commit"]["verification"]["reason"]')" valid
    check "the API lists GPG keys" is "$(api "$RW" GET /user/gpg_keys | field '[0]["emails"][0]["email"]')" ada@example.com
    gpgconf --kill gpg-agent
    unset GNUPGHOME
fi

echo "== commits signed by keel"
check "require verified commits on a branch" is "$(post "$T/ada" /ada/linear/settings/branches pattern=signed required_approvals=0 require_signed=on)" 303
git -C "$LW" commit -q --allow-empty -m "unsigned"
check "unsigned pushes are refused" bash -c "git -C '$LW' push '$LIN' signed 2>&1 | grep -q 'signed requires verified commits; not verified: .* (unsigned)'"
check "over SSH too" bash -c "git -C '$LW' push 'ssh://git@127.0.0.1:$SSH_PORT/ada/linear.git' signed 2>&1 | grep -q 'requires verified commits'"
git -C "$LW" reset -q --hard HEAD~1 && git -C "$LW" commit -S -q --allow-empty -m "signed again"
check "verified ones go through" git -C "$LW" push -q "$LIN" signed
check "edits in the browser are signed by keel" is "$(post "$T/ada" /ada/linear/new/signed path=keel.txt text=keel)" 303
check "and verified" contains "$(curl -s "$B/ada/linear/commits/signed")" 'title="Made on keel and signed with its key SHA256:'
check "the public key is published" contains "$(curl -s "$B/signing-key.pub")" "ssh-ed25519 "
printf '* namespaces="git" %s\n' "$(curl -s "$B/signing-key.pub")" >"$T/allowed_signers"
git -C "$LW" pull -q "$LIN" signed
check "git verifies them too" git -C "$LW" -c gpg.ssh.allowedSignersFile="$T/allowed_signers" verify-commit HEAD

echo "== archiving"
check "a webhook for everything" is "$(post "$T/ada" /ada/linear/settings/hooks "url=http://127.0.0.1:$HOOK_PORT/linear" which=all active=on)" 303
LINHOOK=$(curl -s -b "$T/ada" "$B/ada/linear/settings/hooks" | grep -o 'settings/hooks/[0-9]*' | head -1 | grep -o '[0-9]*$')
queued() { curl -s -b "$T/ada" "$B/ada/linear/settings/hooks/$LINHOOK" | grep -c "<span class=\"mono\">$1</span>"; } # queued <event.action>
check "archive" is "$(post "$T/ada" /ada/linear/archive to=on)" 303
check "webhooks hear of it" is "$(queued repository.archived)" 1
check "it says so" contains "$(curl -s "$B/ada/linear")" "It is read-only"
check "pushes are refused" bash -c "git -C '$LW' push '$LIN' HEAD:refs/heads/late 2>&1 | grep -q 'archived'"
check "over SSH too" bash -c "git -C '$LW' push 'ssh://git@127.0.0.1:$SSH_PORT/ada/linear.git' HEAD:refs/heads/late 2>&1 | grep -q 'archived'"
check "no new issues" contains "$(curl -s -b "$T/bob" -H "$O" -d title=Hi "$B/ada/linear/issues")" "archived"
check "no comments" contains "$(curl -s -b "$T/bob" "$B/ada/linear/pull/2")" "closed to new comments"
check "nor merges" bash -c "! curl -s -b '$T/ada' '$B/ada/linear/pull/2' | grep -q 'name=\"style\"'"
check "the API refuses too" contains "$(api "$RW" POST /repos/ada/linear/issues -d '{"title":"x"}')" "archived"
check "settings still open" is "$(status -b "$T/ada" "$B/ada/linear/settings")" 200
check "out of the way on Explore" bash -c "! curl -s '$B/explore?q=linear' | grep -q 'href=\"/ada/linear\"'"
check "unless asked for" contains "$(curl -s "$B/explore?q=linear&archived=1")" 'href="/ada/linear"'
check "the API says so" is "$(curl -s "$API/repos/ada/linear" | field '["archived"]')" True
check "unarchive with the API" is "$(api "$RW" PATCH /repos/ada/linear -d '{"archived":false}' | field '["archived"]')" False
check "writable again" git -C "$LW" push -q "$LIN" HEAD:refs/heads/late
check "and of that" is "$(queued repository.unarchived)" 1

echo "== project boards"
check "a board" is "$(curl -s -b "$T/ada" -H "$O" -o /dev/null -w '%{redirect_url}' -d title=Roadmap -d kanban=on "$B/ada/linear/projects")" "$B/ada/linear/projects/1"
check "with columns" contains "$(curl -s "$B/ada/linear/projects/1")" "<h3>In progress</h3>"
TODO=$(api "$RW" GET /repos/ada/linear/projects/1 | field '["columns"][0]["id"]')
DONE=$(api "$RW" GET /repos/ada/linear/projects/1 | field '["columns"][2]["id"]')
check "add a pull request" is "$(post "$T/ada" /ada/linear/projects/1/cards "column=$TODO" "text=#2")" 303
check "and a note" is "$(post "$T/ada" /ada/linear/projects/1/cards "column=$TODO" "text=Write the **docs**")" 303
check "notes are Markdown" contains "$(curl -s "$B/ada/linear/projects/1")" "<strong>docs</strong>"
check "unknown numbers" contains "$(curl -s -b "$T/ada" -H "$O" -d "column=$TODO" -d "text=#99" "$B/ada/linear/projects/1/cards")" "no issue or pull request #99"
check "the pull request shows it" contains "$(curl -s "$B/ada/linear/pull/2")" "Roadmap</a> <span class=\"muted small\">· To do"
CARD=$(api "$RW" GET /repos/ada/linear/projects/1 | field '["columns"][0]["cards"][0]["id"]')
check "move a card" is "$(post "$T/ada" "/ada/linear/projects/1/cards/$CARD/move" "column=$DONE" position=0)" 303
check "the script's way" is "$(curl -s -b "$T/ada" -H "$O" -o /dev/null -w '%{http_code}' -d "column=$TODO" -d position=5 -d js=1 "$B/ada/linear/projects/1/cards/$CARD/move")" 204
check "closing moves it to Done" bash -c "[ \"\$(curl -s -o /dev/null -w '%{http_code}' -b '$T/ada' -H '$O' -d action=close -d body= '$B/ada/linear/issues/2/comments')\" = 303 ] && curl -s '$API/repos/ada/linear/projects/1' | python3 -c 'import json,sys; c=json.load(sys.stdin)[\"columns\"]; sys.exit(0 if c[2][\"cards\"] and c[2][\"cards\"][0][\"issue\"][\"number\"] == 2 else 1)'"
check "the list shows progress" contains "$(curl -s "$B/ada/linear/projects")" "<strong>100%</strong> done"
check "add a column" is "$(post "$T/ada" /ada/linear/projects/1/columns name=Review)" 303
check "bob can look" is "$(status -b "$T/bob" "$B/ada/linear/projects/1")" 200
check "but not change it" is "$(post "$T/bob" /ada/linear/projects/1/columns name=Mine)" 403
check "the API adds cards" is "$(api_status "$RW" POST /repos/ada/linear/projects/1/cards -d "{\"column_id\":$TODO,\"note\":\"From the API\"}")" 201
check "and moves them" is "$(api_status "$RW" POST "/repos/ada/linear/projects/1/cards/$CARD/moves" -d "{\"column_id\":$TODO}")" 204
check "the timeline says so" contains "$(curl -s "$B/ada/linear/pull/2")" 'added this to <strong>To do</strong> in'
check "also moves" contains "$(curl -s "$B/ada/linear/pull/2")" 'moved this from <strong>To do</strong> to <strong>Done</strong> in'
check "webhooks get cards" is "$(queued project_card.moved)" 4
check "and boards" is "$(queued project.created)" 1
check "close the board" is "$(post "$T/ada" /ada/linear/projects/1/state to=closed)" 303
check "it's under closed" contains "$(curl -s "$B/ada/linear/projects?state=closed")" "Roadmap"
check "search by board" is "$(curl -s "$API/search/issues?q=project:ada/linear/1" | field '["total_count"]')" 1

echo "== organization boards"
check "members see the organization's boards" contains "$(curl -s -b "$T/ada" "$B/acme")" '/org/acme/projects'
check "an organization board" is "$(curl -s -b "$T/ada" -H "$O" -o /dev/null -w '%{redirect_url}' -d title=Launch -d kanban=on "$B/org/acme/projects")" "$B/org/acme/projects/1"
check "it's for members" is "$(status -b "$T/bob" "$B/org/acme/projects/1")" 404
check "not anonymous people" is "$(status "$B/org/acme/projects")" 404
OISSUE=$(curl -s -b "$T/ada" -H "$O" -o /dev/null -w '%{redirect_url}' -d "title=Ship it" "$B/acme/tool/issues" | grep -o '[0-9]*$')
check "an issue in an organization repository" test -n "$OISSUE"
OTODO=$(api "$RW" GET /orgs/acme/projects/1 | field '["columns"][0]["id"]')
check "plain numbers need a repository" contains "$(curl -s -b "$T/ada" -H "$O" -d "column=$OTODO" -d "text=#1" "$B/org/acme/projects/1/cards")" "Write it like repo#1"
check "add it" is "$(post "$T/ada" /org/acme/projects/1/cards "column=$OTODO" "text=tool#$OISSUE")" 303
check "the card says where it's from" contains "$(curl -s -b "$T/ada" "$B/org/acme/projects/1")" "<span class=\"muted small\">tool#$OISSUE</span>"
check "not from elsewhere" contains "$(curl -s -b "$T/ada" -H "$O" -d "column=$OTODO" -d "text=ada/engine#1" "$B/org/acme/projects/1/cards")" "no issue or pull request ada/engine#1"
check "the issue shows the board to members" contains "$(curl -s -b "$T/ada" "$B/acme/tool/issues/$OISSUE")" 'href="/org/acme/projects/1">Launch</a>'
check "the API lists them" is "$(api "$RW" GET /orgs/acme/projects | field '[0]["html_url"]')" "$B/org/acme/projects/1"
check "and their cards' repositories" is "$(api "$RW" GET /orgs/acme/projects/1 | field '["columns"][0]["cards"][0]["issue"]["repository"]')" acme/tool
check "issue search finds them" is "$(api "$RW" GET "/search/issues?q=project:acme/1" | field '["total_count"]')" 1

echo "== pages"
for p in / /ada /acme /ada/engine /ada/engine/tree/main/src /ada/engine/blob/main/src/main.rs \
    /ada/engine/commits/main /ada/engine/branches /ada/engine/issues /ada/engine/issues/new \
    /ada/engine/settings /ada/engine/pulls "/ada/engine/pulls?state=closed" /ada/engine/compare/main...conflict \
    /notifications "/notifications?all=1" /ada/engine/settings/hooks /org/acme/settings/hooks /settings/notifications /ada/engine/labels /ada/engine/forks "/ada/engine/compare/main...bob/engine:main" "/ada/engine/pull/$FPR/files?comment=file-0-n1" \
    "/search?q=engine" "/ada/engine/search?q=engine" \
    /admin /admin/users /admin/repos /ada/engine/milestones /ada/engine/milestones/new "/ada/engine/milestones/$MS/edit" /ada/engine/settings/branches \
    /ada/engine/releases /ada/engine/releases/new "/ada/engine/releases/$REL/edit" /settings/security /org/acme/settings /settings/account /settings/keys /settings/tokens \
    /explore "/explore?sort=name&page=2" /ada/engine/stargazers "/ada?tab=stars" /ada/tracker/issues/new /ada/tracker/new/main/docs \
    /ada/tracker/upload/main /ada/tracker/edit/main/docs/README.md /ada/tracker/delete/main/docs/README.md \
    /issues /pulls "/pulls?q=is:pr" "/ada/engine/pulls?q=is:closed" /settings/profile /new/import /ada/imported /ada/imported/pulls?q=is:merged \
    /ada/engine/graphs/contributors /ada/engine/graphs/commit-activity /ada/flow/pull/1 /ada/flow/pull/2 "/new?template=ada/flow" \
    /ada/linear/projects /ada/linear/projects/new /ada/linear/projects/1 /ada/linear/projects/1/edit "/ada/linear/commits/signed" "/ada/linear/commit/$SIGNED" \
    /org/acme/projects /org/acme/projects/new /org/acme/projects/1 /org/acme/projects/1/edit "/org/acme/projects?state=closed" /signing-key.pub \
    /ada/engine/wiki /ada/engine/wiki/_pages /ada/engine/wiki/_new /ada/engine/wiki/Home/edit /ada/engine/wiki/Home/history /ada/engine/wiki/_Sidebar; do
    check "GET $p" is "$(status -b "$T/ada" "$B$p")" 200
done

echo "== renaming and transferring"
check "rename" is "$(post "$T/ada" /ada/engine/rename name=motor)" 303
check "old pages redirect" is "$(curl -s -o /dev/null -w '%{redirect_url}' "$B/ada/engine/issues?state=closed")" "$B/ada/motor/issues?state=closed"
check "old clone URLs work" git ls-remote -q "$B/ada/engine.git"
check "and pushes" git -C "$W" push -q "$ADA_HTTP" HEAD:refs/heads/after-rename
check "so does SSH" git ls-remote -q "ssh://git@127.0.0.1:$SSH_PORT/ada/engine.git"
check "the API follows" is "$(curl -s "$API/repos/ada/engine" | field '["name"]')" motor
check "names can't clash" contains "$(curl -s -b "$T/ada" -H "$O" -d name=secret "$B/ada/motor/rename")" "already has"
check "rename a private one" is "$(post "$T/ada" /ada/secret/rename name=vault)" 303
check "outsiders don't learn the new name" is "$(status -b "$T/eve2" "$B/ada/secret")" 404
check "collaborators are redirected" is "$(status -b "$T/bob" "$B/ada/secret")" 307
check "transfers need confirming" contains "$(curl -s -b "$T/ada" -H "$O" -d "new_owner=acme&confirm=x" "$B/ada/motor/transfer")" "exactly"
check "not to other people" contains "$(curl -s -b "$T/ada" -H "$O" -d "new_owner=bob&confirm=ada/motor" "$B/ada/motor/transfer")" "move repositories to bob"
check "transfer to acme" is "$(post "$T/ada" /ada/motor/transfer new_owner=acme confirm=ada/motor)" 303
check "it's there" contains "$(curl -s "$B/acme/motor")" "acme"
check "with its wiki" contains "$(curl -s "$B/acme/motor/wiki")" "Welcome to the"
check "old wiki clone URLs work" git ls-remote -q "$B/ada/engine.wiki.git"
check "the oldest name still leads to it" is "$(curl -s -o /dev/null -w '%{redirect_url}' "$B/ada/engine/pulls")" "$B/acme/motor/pulls"
check "and keeps its pull requests" contains "$(curl -s "$B/acme/motor/pulls?state=closed")" "Checked"
check "move it back with the API" is "$(api_status "$RW" POST /repos/acme/motor/transfer -d '{"new_owner":"ada"}')" 202
check "and rename it" is "$(api "$RW" PATCH /repos/ada/motor -d '{"name":"engine"}' | field '["full_name"]')" ada/engine
check "the name is its own again" is "$(status "$B/ada/engine")" 200

if [ $FAILED -ne 0 ]; then
    echo "--- server log"
    tail -20 "$T/server.log"
    exit 1
fi
echo "All checks passed."
