#!/usr/bin/env bash
# End-to-end smoke test of the agora CLI. Fully hermetic.
#
# Three identities (owner, bob, carol) act from three folders under one fresh temporary root.
# The run covers init and roles, an encrypted channel, chat both ways, an attachment round trip,
# a reaction, membership with key rotation, a card through the work queue, a sprint, a note, and
# an MCP stdio session. The root holds AGORA_HOME and HOME, so the user's ~/.agora is never read
# or written. The daemon the run starts is stopped on exit.
#
# Usage: scripts/e2e-smoke.sh
#   AGORA_BIN  the agora binary. A bare name is looked up on PATH; a relative path is taken from
#              the current directory. Default: target/debug/agora under the repository root.
#              The script does not build it: run `cargo build -p agora` first.
#   KEEP=1     keep the temporary root for inspection; its path is printed.
#
# Requires bash, coreutils and python3. Exit 0 when every step passes. Otherwise the failing step
# prints its command, exit status and output, and the script exits 1.
set -euo pipefail

SCRIPT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
REPO_ROOT=$(cd "$SCRIPT_DIR/.." && pwd)

ROOT=""
AGORA=""
RC=0
CMD=""
OUT=""
ERR=""
STEP_NO=0
STEP_DESC=""

# Runs a command with a time limit. Usage: timed SECS CWD OUT ERR CMD...
# stdout and stderr go to the OUT and ERR files. The exit status is the command's own, 124 when
# the limit passed and the command was killed, 127 when it could not start.
TIMED_PY='
import os, signal, subprocess, sys
secs, cwd, out, err = float(sys.argv[1]), sys.argv[2], sys.argv[3], sys.argv[4]
cmd = sys.argv[5:]
with open(out, "wb") as fo, open(err, "wb") as fe:
    try:
        p = subprocess.Popen(cmd, cwd=cwd, stdin=subprocess.DEVNULL, stdout=fo, stderr=fe,
                             start_new_session=True)
    except OSError as e:
        fe.write(("%s\n" % e).encode())
        sys.exit(127)
    try:
        rc = p.wait(timeout=secs)
    except subprocess.TimeoutExpired:
        os.killpg(p.pid, signal.SIGKILL)
        p.wait()
        fe.write(("timed out after %g s\n" % secs).encode())
        sys.exit(124)
sys.exit(rc if rc >= 0 else 128 - rc)
'

timed() {
  python3 -I -c "$TIMED_PY" "$@"
}

# Reads one JSON document on stdin and prints EXPR, with d set to the document. Strings print
# bare; other values print as JSON. Usage: jget EXPR
jget() {
  python3 -I -c '
import json, sys
d = json.load(sys.stdin)
v = eval(sys.argv[1])
print(v if isinstance(v, str) else json.dumps(v))
' "$1"
}

# Runs a command in DIR with a time limit of LIMIT seconds. Sets RC, OUT, ERR and CMD.
run() {
  local dir=$1 limit=$2
  shift 2
  CMD="(in $dir) $*"
  RC=0
  timed "$limit" "$dir" "$ROOT/last.out" "$ROOT/last.err" "$@" || RC=$?
  OUT=$(cat "$ROOT/last.out")
  ERR=$(cat "$ROOT/last.err")
}

# ag IDENTITY ARGS...: runs agora from the folder of IDENTITY (owner, bob or carol).
# CALL_LIMIT overrides the 20 s limit for one call, e.g. CALL_LIMIT=90 ag bob ...
ag() {
  local who=$1
  shift
  case $who in
    owner | bob | carol) ;;
    *)
      echo "e2e-smoke: internal error: unknown identity $who" >&2
      exit 2
      ;;
  esac
  run "$ROOT/$who" "${CALL_LIMIT:-20}" "$AGORA" "$@"
}

# begin DESC: starts a numbered step.
begin() {
  STEP_NO=$((STEP_NO + 1))
  STEP_DESC=$1
  CMD=""
  RC=0
  OUT=""
  ERR=""
}

# pass: prints the ok line of the current step.
pass() {
  printf 'ok %d - %s\n' "$STEP_NO" "$STEP_DESC"
}

# fail REASON: prints the failed step with its command, output and the daemon log tail, then
# exits 1. The EXIT trap stops the daemon and removes the root.
fail() {
  printf 'not ok %d - %s\n' "$STEP_NO" "$STEP_DESC"
  printf '  reason: %s\n' "$1"
  printf '  command: %s\n' "$CMD"
  printf '  exit status: %s\n' "$RC"
  printf '  stdout:\n%s\n' "$OUT"
  printf '  stderr:\n%s\n' "$ERR"
  if [[ -f ${AGORA_HOME:-}/daemon.log ]]; then
    printf '  daemon.log (last 20 lines):\n'
    tail -n 20 "$AGORA_HOME/daemon.log"
  fi
  exit 1
}

# must: the last agora call must have exited 0.
must() {
  if [[ $RC -ne 0 ]]; then
    fail "the command exited with status $RC"
  fi
}

# check DESC EXPR: EXPR over the JSON in OUT must be true.
check() {
  local got
  got=$(printf '%s' "$OUT" | jget "$2" 2>/dev/null) || got="(no JSON answer)"
  if [[ $got != true ]]; then
    fail "$1: expected $2 to be true, got $got"
  fi
}

# take NAME EXPR: sets NAME to EXPR over the JSON in OUT. Fails the step when there is no value.
take() {
  local v
  v=$(printf '%s' "$OUT" | jget "$2" 2>/dev/null) || v=""
  if [[ -z $v || $v == null ]]; then
    fail "no value for $2 in the output"
  fi
  printf -v "$1" '%s' "$v"
}

# Stops the daemon of this run. The agora stop verb is a no-op when no daemon runs. As a fallback,
# TERM goes to the daemons whose command line names this run's AGORA_HOME, so no other daemon is
# touched.
stop_daemon() {
  [[ -n ${AGORA_HOME:-} && -n $AGORA && -d $AGORA_HOME ]] || return 0
  timed 15 "$ROOT" "$ROOT/stop.out" "$ROOT/stop.err" "$AGORA" stop >/dev/null 2>&1 || true
  if command -v pgrep >/dev/null 2>&1; then
    local pid i pat
    pat=$(printf '%s' "$AGORA_HOME" | sed 's/[.[\*^$]/\\&/g')
    for pid in $(pgrep -f -- "daemon --home ${pat}\$" 2>/dev/null || true); do
      kill -TERM "$pid" 2>/dev/null || true
      for i in 1 2 3 4 5 6; do
        kill -0 "$pid" 2>/dev/null || break
        sleep 0.5
      done
    done
  fi
}

cleanup() {
  local status=$?
  trap - EXIT
  set +e
  if [[ -n $ROOT ]]; then
    stop_daemon
    if [[ ${KEEP:-0} == 1 ]]; then
      echo "e2e-smoke: KEEP=1, root kept at $ROOT" >&2
    else
      rm -rf "$ROOT"
    fi
  fi
  exit "$status"
}

# The MCP stdio session: initialize, notifications/initialized, tools/list. Prints the protocol
# version and the tool count. Exits 1 when inbox_wait is missing, a call fails or times out, or
# the server does not exit within 10 s after its stdin closes.
MCP_PY='
import json, os, select, subprocess, sys, time
exe, cwd, errpath = sys.argv[1], sys.argv[2], sys.argv[3]
ok = False
with open(errpath, "wb") as errf:
    p = subprocess.Popen([exe, "mcp"], cwd=cwd, stdin=subprocess.PIPE,
                         stdout=subprocess.PIPE, stderr=errf)
    buf = [b""]

    def send(msg):
        p.stdin.write((json.dumps(msg) + "\n").encode())
        p.stdin.flush()

    def recv(want_id, secs=10.0):
        deadline = time.monotonic() + secs
        while True:
            while b"\n" in buf[0]:
                line, buf[0] = buf[0].split(b"\n", 1)
                if line.strip():
                    msg = json.loads(line)
                    if msg.get("id") == want_id:
                        return msg
            left = deadline - time.monotonic()
            if left <= 0:
                raise RuntimeError("no response to request %d within %g s" % (want_id, secs))
            ready, _, _ = select.select([p.stdout], [], [], left)
            if ready:
                chunk = os.read(p.stdout.fileno(), 65536)
                if not chunk:
                    raise RuntimeError("mcp server closed stdout before answering request %d" % want_id)
                buf[0] += chunk

    try:
        send({"jsonrpc": "2.0", "id": 1, "method": "initialize",
              "params": {"protocolVersion": "2025-11-25", "capabilities": {},
                         "clientInfo": {"name": "agora-e2e-smoke", "version": "0"}}})
        init = recv(1)
        if "error" in init:
            raise RuntimeError("initialize failed: %s" % json.dumps(init["error"]))
        version = init["result"]["protocolVersion"]
        send({"jsonrpc": "2.0", "method": "notifications/initialized"})
        send({"jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {}})
        listed = recv(2)
        if "error" in listed:
            raise RuntimeError("tools/list failed: %s" % json.dumps(listed["error"]))
        names = [t["name"] for t in listed["result"]["tools"]]
        if "inbox_wait" not in names:
            raise RuntimeError("tools/list has no inbox_wait (%d tools)" % len(names))
        print("protocol %s, %d tools, inbox_wait listed" % (version, len(names)))
        ok = True
    except Exception as e:
        print("mcp: %s" % e, file=sys.stderr)
    finally:
        try:
            p.stdin.close()
        except OSError:
            pass
        try:
            rc = p.wait(timeout=10)
            if rc != 0:
                print("mcp: server exited with status %d after stdin closed" % rc, file=sys.stderr)
                ok = False
        except subprocess.TimeoutExpired:
            p.kill()
            p.wait()
            print("mcp: server did not exit within 10 s after stdin closed", file=sys.stderr)
            ok = False
sys.exit(0 if ok else 1)
'

command -v python3 >/dev/null 2>&1 || {
  echo "e2e-smoke: python3 is required" >&2
  exit 1
}

# Resolve the binary to an absolute, normalized path. Nothing is built here.
bin=${AGORA_BIN:-}
if [[ -z $bin ]]; then
  AGORA=$REPO_ROOT/target/debug/agora
elif [[ $bin == */* ]]; then
  case $bin in
    /*) AGORA=$bin ;;
    *) AGORA=$PWD/$bin ;;
  esac
else
  AGORA=$(command -v "$bin" 2>/dev/null || true)
fi
if [[ -z $AGORA || ! -f $AGORA || ! -x $AGORA ]]; then
  echo "e2e-smoke: agora binary not found: ${AGORA:-${bin:-target/debug/agora}}" >&2
  echo "e2e-smoke: build it with 'cargo build -p agora', or set AGORA_BIN to its path" >&2
  exit 1
fi
AGORA=$(cd "$(dirname "$AGORA")" && pwd)/$(basename "$AGORA")

# The hermetic root. Unix socket paths are limited to 104 bytes on macOS, so the path stays short.
ROOT=$(mktemp -d /tmp/agora-e2e.XXXXXX)
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
export AGORA_HOME=$ROOT/home HOME=$ROOT/userhome
unset AGORA_FOLDER WORK_SCOPE AGORA_NO_SPAWN AGORA_CLAUDE_BIN
mkdir -p "$AGORA_HOME" "$HOME" "$ROOT/owner" "$ROOT/bob" "$ROOT/carol"

echo "e2e-smoke: binary $AGORA"
START=$SECONDS

# 1. The owner identity. Its folder is $ROOT/owner.
begin "owner: agora init --owner registers the owner identity"
ag owner --json init --owner
must
check "the role is owner" "d['role'] == 'owner'"
take PK_OWNER 'd["pubkey"]'
pass

# 2. A second identity in its own folder. The owner grants it the worker role, which posting needs.
begin "bob: a second folder registers, and the owner grants it worker"
ag bob --json init
must
check "bob is registered" "d['registered'] is True"
take PK_BOB 'd["pubkey"]'
ag owner --json role grant "$PK_BOB" worker
must
ag bob --json whoami
must
check "bob holds the worker role" "d['role'] == 'worker'"
pass

# 3. An encrypted group channel with bob as a member. The channel reports its key epoch.
begin "owner: encrypted group channel with bob as a member"
ag owner --json channel create team --kind group --name "E2E team" --member "$PK_BOB" --encrypt
must
check "the create reply says encrypted" "d['encrypted'] is True"
ag owner --json channel info team
must
check "the channel is encrypted and has an integer epoch" \
  "d['channel']['encrypted'] is True and isinstance(d['channel']['epoch'], int)"
check "owner and bob are members" \
  "{m['pk'] for m in d['members']} >= {'$PK_OWNER', '$PK_BOB'}"
pass

# 4. Chat in both directions, each side received through inbox --wait (limit 10 s).
begin "chat: owner to bob and bob to owner, received through inbox --wait 10"
ag owner --json chat team "hello from owner"
must
ag bob --json inbox --wait 10
must
check "bob received the owner's message" "any(m['text'] == 'hello from owner' for m in d['messages'])"
ag bob --json chat team "hello from bob"
must
ag owner --json inbox --wait 10
must
check "owner received bob's reply" "any(m['text'] == 'hello from bob' for m in d['messages'])"
pass

# 5. An attachment comes back byte for byte. A reaction is accepted (no verb lists reactions).
begin "attachment: fetched bytes equal the original; a reaction is accepted"
head -c 2048 /dev/urandom >"$ROOT/payload.bin"
ag owner --json chat team "see the attached file" --attach "$ROOT/payload.bin"
must
take MSG_FILE 'd["id"]'
ag bob --json read team
must
take HASH "next(a['hash'] for m in d['messages'] if m['id'] == '$MSG_FILE' for a in m['attachments'])"
ag bob --json fetch "$HASH" --out "$ROOT/fetched.bin" --channel team
must
check "fetch reports 2048 bytes" "d['bytes'] == 2048"
if ! cmp -s "$ROOT/payload.bin" "$ROOT/fetched.bin"; then
  fail "the fetched file differs from the attachment"
fi
ag bob --json react "$MSG_FILE" +1
must
check "the reaction names the message and the emoji" \
  "d['target'] == '$MSG_FILE' and d['emoji'] == '+1' and d['remove'] is False"
pass

# 6. Carol joins the channel, then leaves. Removing a member of an encrypted channel rotates the key.
begin "membership: carol is added, then removed, and the key epoch rises"
ag carol --json init
must
take PK_CAROL 'd["pubkey"]'
ag owner --json channel add team "$PK_CAROL"
must
ag owner --json channel info team
must
take EPOCH_BEFORE 'd["channel"]["epoch"]'
check "carol is a member after the add" "'$PK_CAROL' in {m['pk'] for m in d['members']}"
ag owner --json channel remove team "$PK_CAROL"
must
ag owner --json channel info team
must
take EPOCH_AFTER 'd["channel"]["epoch"]'
check "carol is gone from the members" "not any(m['pk'] == '$PK_CAROL' for m in d['members'])"
if ((EPOCH_AFTER <= EPOCH_BEFORE)); then
  fail "the key epoch did not rise: $EPOCH_BEFORE before the removal, $EPOCH_AFTER after"
fi
pass

# 7. Board: the owner readies a card, bob claims it and runs a job for it, the job moves the card
# to review, the owner approves, and the owner moves it to done. The done move is refused without
# evidence, so it also shows that the job left evidence.
begin "board: card claimed, job run, review by the job, approved, done"
# hermetic home: admission must not depend on host load (CI runners are busy)
ag owner --json work settings --idle-min 0
must
ag owner --json card create c-smoke "E2E smoke card"
must
ag owner --json card move c-smoke ready
must
ag bob --json card move c-smoke claimed
must
ag owner --json card list
must
check "bob owns the claimed card" \
  "any(c['id'] == 'c-smoke' and c['state'] == 'claimed' and c['owner'] == '$PK_BOB' for c in d['cards'])"
CALL_LIMIT=90 ag bob --json work run --card c-smoke --timeout 60 -- true
must
ag owner --json card list
must
check "the job moved the card to review" \
  "any(c['id'] == 'c-smoke' and c['state'] == 'review' for c in d['cards'])"
ag owner --json card approve c-smoke
must
ag owner --json card move c-smoke done
must
ag owner --json card list
must
check "the card is done" "any(c['id'] == 'c-smoke' and c['state'] == 'done' for c in d['cards'])"
pass

# 8. A sprint holds the card. The summary is text and counts cards without naming them.
begin "sprint: created, card added, summary counts the done card"
NOW_S=$(date +%s)
ag owner --json sprint create s-smoke "E2E sprint" --goal "smoke" \
  --starts "$(((NOW_S - 3600) * 1000))" --ends "$(((NOW_S + 3600) * 1000))"
must
ag owner --json sprint add s-smoke c-smoke
must
ag owner sprint summary s-smoke
must
if [[ $OUT != "sprint s-smoke: total 1, done 1, "* ]]; then
  fail "the summary does not count the card as done"
fi
printf '# %s\n' "$OUT"
pass

# 9. A note is published by bob and found by search.
begin "knowledge: note published by bob and found by note search"
ag bob --json note publish n-smoke "E2E note" "smoke body token zqsmoke" --tags e2e
must
ag owner --json note search zqsmoke
must
check "the search finds the note" "any(n['id'] == 'n-smoke' for n in d['notes'])"
pass

# 10. The MCP stdio server answers tools/list with inbox_wait, and exits when stdin closes.
begin "mcp: stdio session from the owner folder lists inbox_wait and exits on stdin close"
run "$ROOT/owner" 45 python3 -I -c "$MCP_PY" "$AGORA" "$ROOT/owner" "$ROOT/mcp.err"
if [[ $RC -ne 0 ]]; then
  ERR="$ERR$(cat "$ROOT/mcp.err")"
  fail "the MCP session did not complete (see the mcp: lines in stderr)"
fi
pass

echo "e2e-smoke: all $STEP_NO steps passed in $((SECONDS - START)) s"
