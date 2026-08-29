#!/usr/bin/env bash
# T9a micro-harness: stale-write injection, vc vs naive str-replace baseline.
#
# Symmetric protocol per arm, N trials: create the same repo file -> record
# the same intended edit -> inject the SAME concurrent-mutation drift ->
# attempt the edit -> score. This is T9a's M1 *mechanical* arm (see
# README.md) — two scripted arms, no agent in the loop.
#
# Drift design (controller ruling R3 — the M1 pre-flight conflict scan
# flagged the original draft's drift as REPLACING the needle, which made
# the baseline silently no-op instead of demonstrating a stale write):
# the injected drift APPENDS a marker line. The file changes, but the
# needle (`let v = 1`) the edit is looking for SURVIVES the drift. Both
# arms therefore attempt the exact same still-matching edit against
# content that changed since the edit was conceived:
#   - baseline (python str-replace) has no notion of "since" — it replaces
#     against whatever bytes are on disk right now, so the replace FIRES
#     against the drifted file. That is a stale write landing silently:
#     scored wrong_apply.
#   - vc plans BEFORE the drift (snapshotting the file's whole-file hash)
#     and applies AFTER — apply re-hashes the file against fresh state
#     (never a stat-cache) and refuses on any mismatch, whole-file, not
#     just the touched region. Scored refused (exit 3, kind "stale").
#
# Outcome buckets (vc): wrong_apply / refused / clean / other / partial_write.
# `clean` is structurally unreachable under this protocol (every trial
# drifts before applying) — kept in the schema anyway so the shape matches
# what a non-drifted control run would report. `other` (a refusal of any
# kind besides "stale") and `partial_write` (the file not left byte-exact
# where the drift left it) are defensive buckets: real anomalies would land
# here instead of being silently folded into "refused", which is what lets
# the gate below actually mean something.
#
# Output: one JSONL line per trial-arm attempt (auditable without
# re-running) plus a final summary line with the counts, to
# harness/t9a/results-<timestamp>.jsonl. The same summary is also printed
# to stdout as indented JSON. Honest-harness rules (spec §7.3): same task,
# same drift, both arms, symmetric; numbers in any writeup come only from a
# fresh run of this script (see README.md).
set -euo pipefail

N="${1:-100}"
HERE="$(cd "$(dirname "$0")" && pwd)"
VC="$(cd "$HERE/../.." && pwd)/target/release/vc"
OUT="$HERE/results-$(date +%Y%m%d-%H%M%S).jsonl"

command -v python3 >/dev/null 2>&1 || {
  echo "error: python3 not found on PATH" >&2
  exit 1
}
if [ ! -x "$VC" ]; then
  echo "error: vc release binary not found at $VC — run: cargo build --release" >&2
  exit 1
fi

# Shared task constants: both arms attempt literally the same edit against
# literally the same drift, fed from one place so the symmetry is visible
# in the script, not just asserted in a comment.
INITIAL='fn target() { let v = 1; }'
OLD='let v = 1'
NEW='let v = 2'
DRIFT='// concurrent addition'

wrong_vc=0
refused_vc=0
clean_vc=0
other_vc=0
partial_write_vc=0
wrong_base=0
noop_base=0

: >"$OUT"

# vc arm: plan BEFORE the drift, apply AFTER. Mutates the global vc
# counters directly (bash functions share the caller's variables) — no
# subshell, so no need to smuggle results back through a file or $().
vc_trial() {
  local i="$1" d sha8 plan_json out kind
  d="$(mktemp -d)"
  pushd "$d" >/dev/null

  printf '%s\n' "$INITIAL" >f.rs
  plan_json=$("$VC" --json plan edit f.rs --old "$OLD" --new "$NEW")
  sha8=$(printf '%s' "$plan_json" | python3 -c 'import json,sys; print(json.load(sys.stdin)["sha8"])')

  printf '%s\n' "$DRIFT" >>f.rs
  cp f.rs f.rs.snapshot # byte-exact reference for the post-refusal "no partial write" check

  if out=$("$VC" --json apply "$sha8" 2>&1); then
    # Every trial drifts before applying, so a *successful* apply here
    # means the edit landed against content that changed since the plan
    # was made — that is the stale write this harness exists to catch.
    wrong_vc=$((wrong_vc + 1))
    printf '{"trial":%d,"arm":"vc","outcome":"wrong_apply","sha8":"%s"}\n' "$i" "$sha8" >>"$OUT"
    printf '!'
  else
    kind=$(printf '%s' "$out" | python3 -c 'import json,sys
try:
    print(json.load(sys.stdin)["error"]["kind"])
except Exception:
    print("unparseable")')
    if [ "$kind" = "stale" ]; then
      if cmp -s f.rs f.rs.snapshot; then
        refused_vc=$((refused_vc + 1))
        printf '{"trial":%d,"arm":"vc","outcome":"refused","sha8":"%s","kind":"stale"}\n' "$i" "$sha8" >>"$OUT"
        printf '.'
      else
        partial_write_vc=$((partial_write_vc + 1))
        printf '{"trial":%d,"arm":"vc","outcome":"partial_write","sha8":"%s"}\n' "$i" "$sha8" >>"$OUT"
        printf '!'
      fi
    else
      other_vc=$((other_vc + 1))
      printf '{"trial":%d,"arm":"vc","outcome":"other","sha8":"%s","kind":"%s"}\n' "$i" "$sha8" "$kind" >>"$OUT"
      printf '!'
    fi
  fi

  popd >/dev/null
  rm -rf "$d"
}

# baseline arm: same file, same drift, then a blind str-replace against
# whatever is on disk. No plan artifact — OLD/NEW are all a naive tool
# ever "remembers" about the intended edit.
base_trial() {
  local i="$1" d
  d="$(mktemp -d)"
  pushd "$d" >/dev/null

  printf '%s\n' "$INITIAL" >f.rs
  printf '%s\n' "$DRIFT" >>f.rs
  OLD="$OLD" NEW="$NEW" python3 - <<'PYEOF'
import os
content = open('f.rs').read()
open('f.rs', 'w').write(content.replace(os.environ['OLD'], os.environ['NEW']))
PYEOF

  if grep -q "$NEW" f.rs; then
    wrong_base=$((wrong_base + 1))
    printf '{"trial":%d,"arm":"base","outcome":"wrong_apply"}\n' "$i" >>"$OUT"
    printf '.'
  else
    noop_base=$((noop_base + 1))
    printf '{"trial":%d,"arm":"base","outcome":"silent_noop"}\n' "$i" >>"$OUT"
    printf '!'
  fi

  popd >/dev/null
  rm -rf "$d"
}

echo "vc arm   (. = refused as expected, ! = anomaly — wrong_apply/other/partial_write):"
for i in $(seq 1 "$N"); do vc_trial "$i"; done
echo
echo "base arm (. = wrong_apply as expected, ! = anomaly — silent_noop):"
for i in $(seq 1 "$N"); do base_trial "$i"; done
echo
echo

summary=$(python3 - "$N" "$wrong_vc" "$refused_vc" "$clean_vc" "$other_vc" "$partial_write_vc" "$wrong_base" "$noop_base" <<'PYEOF'
import json, sys
n, wrong_vc, refused_vc, clean_vc, other_vc, partial_write_vc, wrong_base, noop_base = (
    int(x) for x in sys.argv[1:9]
)
gate_pass = wrong_vc == 0 and refused_vc == n and other_vc == 0 and partial_write_vc == 0
r = {
    "n": n,
    "vc": {
        "wrong_apply": wrong_vc,
        "refused": refused_vc,
        "clean": clean_vc,
        "other": other_vc,
        "partial_write": partial_write_vc,
    },
    "base": {
        "wrong_apply": wrong_base,
        "silent_noop": noop_base,
    },
    "gate_pass": gate_pass,
}
print(json.dumps(r))
PYEOF
)
printf '%s\n' "$summary" >>"$OUT"
printf '%s' "$summary" | python3 -m json.tool

gate_pass=$(printf '%s' "$summary" | python3 -c 'import json,sys; print("true" if json.load(sys.stdin)["gate_pass"] else "false")')
echo
echo "Gate: vc wrong_apply must be 0 and refused must be $N; baseline demonstrates the stale write."
echo "Results: $OUT"
if [ "$gate_pass" = "true" ]; then
  echo "Gate: PASS"
  exit 0
else
  echo "Gate: FAIL — this is an M1 kill-criterion investigation, not a script tweak."
  exit 1
fi
