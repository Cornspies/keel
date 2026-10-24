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

"$BIN" serve --data-dir "$T/data" --http-addr "127.0.0.1:$HTTP_PORT" \
    --ssh-addr "127.0.0.1:$SSH_PORT" --base-url "$B" --webhook-allow-private \
    --smtp-url "smtp://127.0.0.1:$SMTP_PORT" --mail-from "keel <git@example.com>" >"$T/server.log" 2>&1 &
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
check "bob cannot push" bash -c "! git -C '$W' push -q 'http://bob:password-bob@127.0.0.1:$HTTP_PORT/ada/engine.git' main"

echo "== access tokens"
page=$(curl -s -b "$T/ada" -H "$O" --data-urlencode "name=ci" --data-urlencode "expires=30" "$B/settings/tokens")
RO=$(grep -o 'value="keel_[A-Za-z0-9_-]*"' <<<"$page" | cut -d'"' -f2)
page=$(curl -s -b "$T/ada" -H "$O" --data-urlencode "name=laptop" --data-urlencode "write=on" "$B/settings/tokens")
RW=$(grep -o 'value="keel_[A-Za-z0-9_-]*"' <<<"$page" | cut -d'"' -f2)
check "tokens are shown once" bash -c "[ -n '$RO' ] && [ -n '$RW' ] && ! curl -s -b '$T/ada' '$B/settings/tokens' | grep -q '$RW'"
check "read-only token clones private repo" git clone -q "http://ada:$RO@127.0.0.1:$HTTP_PORT/ada/secret.git" "$T/c2"
echo change >>"$W/README.md" && git -C "$W" commit -qam "Second"
check "read-only token cannot push" bash -c "! git -C '$W' push -q 'http://ada:$RO@127.0.0.1:$HTTP_PORT/ada/engine.git' main"
check "write token pushes (token as username)" git -C "$W" push -q "http://$RW:x@127.0.0.1:$HTTP_PORT/ada/engine.git" main
check "wrong token is refused" bash -c "! git ls-remote -q 'http://ada:keel_nope@127.0.0.1:$HTTP_PORT/ada/secret.git'"

echo "== git over SSH"
check "add SSH key" is "$(post "$T/ada" /settings/keys "key=$(cat "$T/key.pub")")" 303
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
check "unknown endpoints are JSON 404s" contains "$(curl -s "$API/nope")" '"message"'
git -C "$W" checkout -q -b api-change main~1 2>/dev/null || git -C "$W" checkout -q -b api-change
git -C "$W" fetch -q "$ADA_HTTP" main && git -C "$W" reset -q --hard FETCH_HEAD
echo "api" >"$W/api.txt" && git -C "$W" add -A && git -C "$W" commit -qm "Add api.txt"
git -C "$W" push -q "$ADA_HTTP" api-change
check "open a pull request" is "$(api_status "$RW" POST /repos/ada/engine/pulls -d '{"title":"API change","head":"api-change","base":"main"}')" 201
AP=$(api "$RW" GET /repos/ada/engine/pulls | field '[0]["number"]')
check "it says why it can't be merged yet" contains "$(curl -s "$API/repos/ada/engine/pulls/$AP" | field '["merge_blocked_by"]')" "approving review"
check "merging is refused" is "$(api_status "$RW" PUT "/repos/ada/engine/pulls/$AP/merge" -d '{}')" 405
page=$(curl -s -b "$T/bob" -H "$O" --data-urlencode "name=api" --data-urlencode "write=on" "$B/settings/tokens")
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

echo "== pages"
for p in / /ada /acme /ada/engine /ada/engine/tree/main/src /ada/engine/blob/main/src/main.rs \
    /ada/engine/commits/main /ada/engine/branches /ada/engine/issues /ada/engine/issues/new \
    /ada/engine/settings /ada/engine/pulls "/ada/engine/pulls?state=closed" /ada/engine/compare/main...conflict \
    /notifications "/notifications?all=1" /ada/engine/settings/hooks /org/acme/settings/hooks /settings/notifications /ada/engine/labels /ada/engine/forks "/ada/engine/compare/main...bob/engine:main" "/ada/engine/pull/$FPR/files?comment=file-0-n1" \
    "/search?q=engine" "/ada/engine/search?q=engine" \
    /admin /admin/users /admin/repos /ada/engine/milestones /ada/engine/milestones/new "/ada/engine/milestones/$MS/edit" /ada/engine/settings/branches \
    /org/acme/settings /settings/account /settings/keys /settings/tokens; do
    check "GET $p" is "$(status -b "$T/ada" "$B$p")" 200
done

if [ $FAILED -ne 0 ]; then
    echo "--- server log"
    tail -20 "$T/server.log"
    exit 1
fi
echo "All checks passed."
