#!/usr/bin/env bash
# R1 definitions: top-1 symbol-search accuracy against a frozen, hand-labeled
# Rust corpus (harness/r/corpus_defs/), same bash+jq shape as r1_lexical.sh.
#
# For every ground-truth row, `vc query NAME --symbol --json`'s FIRST hit
# (hits[0] — search_symbol sorts exact matches by (path, start_line), so
# hits[0] is deterministic) must equal the labeled `path:line`. A `NONE`
# row instead asserts zero EXACT matches: `fuzzy` must be `true` (the
# match, if any, came only from the substring fallback — search_symbol
# returns the fuzzy pool only when the exact pool is empty, so fuzzy=true
# with a non-empty hits[] still means zero exact matches; see the vc-query
# fuzzy-fallback contract this harness pins).
#
# Gate thresholds (spec-pinned): top-1 >= 98% AND confidently-wrong <= 1%.
# "Confidently wrong" = a `fuzzy: false` top-1 whose path:line differs from
# the label — i.e. vc was SURE and still picked the wrong definition, the
# failure mode that actually misleads a caller (a fuzzy=true miss just means
# the exact tier came up empty, which is a recall gap, not a wrong answer).
set -uo pipefail
corpus="$(cd "$(dirname "$0")/corpus_defs" && pwd)"
truth="$(dirname "$0")/r1_defs/ground_truth.tsv"

top1_correct=0
confidently_wrong=0
n=0
out_tmp="$(mktemp)"
trap 'rm -f "$out_tmp"' EXIT

while IFS=$'\t' read -r name kind loc || [ -n "$name" ]; do
  [ -z "$name" ] && continue
  n=$((n + 1))

  vc_out=$(cd "$corpus" && vc query "$name" --symbol --json </dev/null)
  vc_rc=$?
  if [ "$vc_rc" -ne 0 ]; then
    echo "vc error (exit $vc_rc) on name [$name]: $vc_out" >&2
    exit "$vc_rc"
  fi

  # Parse pipeline: check every stage's exit code (PIPESTATUS), not just the
  # final one — same discipline as r1_lexical.sh, and for the same reason: a
  # jq parse error on garbled vc output must not silently collapse into an
  # empty/false-y result that looks like an ordinary miss.
  printf '%s' "$vc_out" | jq -r '[(.fuzzy | tostring), (.hits[0].path // "NONE"), (.hits[0].line // "NONE" | tostring)] | @tsv' >"$out_tmp"
  jq_pipe=("${PIPESTATUS[@]}") # printf jq
  if [ "${jq_pipe[1]}" -ne 0 ]; then
    echo "vc parse pipeline failed (jq=${jq_pipe[1]}) on name [$name]" >&2
    echo "raw vc output: $vc_out" >&2
    exit 1
  fi
  IFS=$'\t' read -r fuzzy got_path got_line <"$out_tmp"

  if [ "$kind" = "NONE" ]; then
    # Negative control: exact tier must be empty. search_symbol only ever
    # returns the fuzzy pool when the exact pool is empty, so fuzzy=true is
    # exactly "zero exact matches" — hits[] may still be non-empty (fuzzy
    # substring fallback), which is not a failure for a NONE row.
    if [ "$fuzzy" != "true" ]; then
      echo "MISS $name expected NONE got $got_path:$got_line fuzzy=$fuzzy"
      continue
    fi
    top1_correct=$((top1_correct + 1))
    continue
  fi

  got="$got_path:$got_line"
  if [ "$got" = "$loc" ]; then
    top1_correct=$((top1_correct + 1))
  else
    echo "MISS $name expected $loc got $got fuzzy=$fuzzy"
    if [ "$fuzzy" = "false" ]; then
      confidently_wrong=$((confidently_wrong + 1))
    fi
  fi
done <"$truth"

# Integer-percent thresholds via *100 comparison (no bc/awk float dependency,
# same style as this repo's other gates): top1 >= 98% <=> top1*100 >= n*98;
# wrong <= 1% <=> wrong*100 <= n*1.
top1_pct=$(awk -v c="$top1_correct" -v t="$n" 'BEGIN { printf "%.2f", (t > 0 ? c / t * 100 : 0) }')
wrong_pct=$(awk -v c="$confidently_wrong" -v t="$n" 'BEGIN { printf "%.2f", (t > 0 ? c / t * 100 : 0) }')

echo "R1 definitions: top-1 $top1_correct/$n (${top1_pct}%), confidently-wrong $confidently_wrong/$n (${wrong_pct}%)"

fail=0
if [ $((top1_correct * 100)) -lt $((n * 98)) ]; then
  echo "R1 definitions: FAIL — top-1 ${top1_pct}% < 98% threshold"
  fail=1
fi
if [ $((confidently_wrong * 100)) -gt $((n * 1)) ]; then
  echo "R1 definitions: FAIL — confidently-wrong ${wrong_pct}% > 1% threshold"
  fail=1
fi
if [ "$fail" -eq 0 ]; then
  echo "R1 definitions: PASS"
fi
exit $fail
