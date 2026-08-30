#!/usr/bin/env bash
# R1 lexical parity: vc query vs rg, identical semantics.
#
# rg flags pin vc's walk semantics (velocity-code-kernel::walk::walk_files):
#   --no-config          vc has no user-rc-file equivalent to honor either
#   --hidden             vc's WalkBuilder uses .hidden(false) — dotfiles ARE
#                         walked (the ignore crate's "hidden" toggle is a
#                         *filter*, so false means "don't filter them out")
#   -g '!.git' -g '!.vc' vc's filter_entry excludes exactly these two dirs
#                         by name, everywhere in the tree
#   --no-ignore-global    vc's WalkBuilder uses .git_global(false) — no
#                         per-machine ~/.gitignore / core.excludesFile
#   --ignore-file=<f>     one per DISCOVERED .vcignore under the corpus
#   (repeated)            (root AND any subdirectory's, found fresh each run
#                         via `find "$corpus" -name .vcignore`). vc registers
#                         ".vcignore" as a per-directory custom ignore
#                         filename via add_custom_ignore_filename — every
#                         directory in the walk gets its own, exactly like
#                         .gitignore. rg has no CLI equivalent of "treat this
#                         filename as a recursive per-directory ignore file",
#                         so each discovered .vcignore is passed as its own
#                         --ignore-file instead; per rg's own docs its
#                         patterns are matched relative to the CWD (the
#                         corpus root), not to the ignore file's own
#                         directory. That would be a real gap for an
#                         anchored pattern (e.g. "/foo.rs") — this harness's
#                         fixture only uses bare, unanchored filenames in
#                         every .vcignore (e.g. "sub_generated.rs"), and a
#                         bare gitignore-style pattern matches at any depth
#                         under BOTH interpretations, so the flat --ignore-file
#                         model and vc's true per-directory recursion agree
#                         for this corpus. A future .vcignore using an
#                         anchored pattern would need this reworked.
#   (no flag)             .gitignore and ripgrep's own .ignore convention are
#                         both honored by default on both sides — vc's
#                         WalkBuilder defaults (git_ignore(true), ignore(true))
#                         match rg's defaults exactly, since vc and rg share
#                         the same underlying `ignore` crate
#   (no flag)             binary-file skip (NUL byte in the first chunk) is
#                         each tool's own default; neither needs -a/--text
#
# Regex anchors: vc's search_regex compiles with `regex::bytes::RegexBuilder
# .multi_line(true)` (R1 parity ruling 2026-08-29), so `^`/`$` anchor at
# every line boundary within a file — matching rg's default line-oriented
# search mode exactly. queries_lexical.txt includes anchored patterns
# (`^fn`, `\}$`) to exercise this; there is no known regex-semantics
# divergence left between vc and rg for this query list.
set -uo pipefail
corpus="$(cd "$(dirname "$0")/corpus" && pwd)"
queries="$(dirname "$0")/queries_lexical.txt"

# `vc` finds its repo root by walking UP from the CWD looking for `.vc/`,
# so without a `.vc` of its own the corpus silently binds to whichever
# ancestor has one — this repo's, making vc and rg search different trees
# and every "parity" result meaningless. Demonstrated leak, not a
# hypothetical: run from a checkout, the corpus queries resolved against
# velocity-code itself.
mkdir -p "$corpus/.vc"

# One --ignore-file per .vcignore found anywhere under the corpus (root and
# every subdirectory) — see the flag-rationale block above.
rg_ignore_args=()
while IFS= read -r f; do
  rg_ignore_args+=("--ignore-file=$f")
done < <(find "$corpus" -name .vcignore -type f | sort)

fail=0
n=0
vc_tmp="$(mktemp)"
rg_tmp="$(mktemp)"
trap 'rm -f "$vc_tmp" "$rg_tmp"' EXIT

while IFS=$'\t' read -r mode q || [ -n "$mode" ]; do
  [ -z "$mode" ] && continue
  n=$((n + 1))
  # `--` before the data argument on both sides (the rg side always had
  # it): a query is arbitrary text, and one beginning with `-` would
  # otherwise be parsed as a flag — silently on the vc side, where it would
  # look like a mismatch rather than a misparse. Every vc flag therefore
  # goes BEFORE the `--`, and `--json` goes before the subcommand, since it
  # is a global flag and after `--` would be read as a positional scope
  # path.
  case "$mode" in
  L) vcargs=(query -- "$q") rgargs=(-F -- "$q") ;;
  R) vcargs=(query --regex -- "$q") rgargs=(-- "$q") ;;
  *)
    echo "queries_lexical.txt: bad mode '$mode' on line $n" >&2
    exit 2
    ;;
  esac

  vc_out=$(cd "$corpus" && vc --json "${vcargs[@]}" </dev/null)
  vc_rc=$?
  if [ "$vc_rc" -ne 0 ]; then
    echo "vc error (exit $vc_rc) on query [$mode] $q: $vc_out" >&2
    exit "$vc_rc"
  fi

  # Parse pipeline: check every stage's exit code (PIPESTATUS), not just the
  # final one — a jq parse error on garbled vc output would otherwise
  # silently collapse to an empty vc_set that spuriously "matches" an empty
  # rg_set on any negative-control query (Alpha, SECRETVALUE, ...). Written
  # against a temp file rather than `x=$(pipeline)` because this bash does
  # NOT propagate PIPESTATUS through a pipeline run inside a command
  # substitution — only a bare pipeline (redirected to a file) does.
  printf '%s' "$vc_out" | jq -r '.hits[] | "\(.path):\(.line)"' | sort -u >"$vc_tmp"
  vc_pipe=("${PIPESTATUS[@]}") # printf jq sort
  if [ "${vc_pipe[1]}" -ne 0 ] || [ "${vc_pipe[2]}" -ne 0 ]; then
    echo "vc parse pipeline failed (jq=${vc_pipe[1]} sort=${vc_pipe[2]}) on query [$mode] $q" >&2
    echo "raw vc output: $vc_out" >&2
    exit 1
  fi
  vc_set=$(cat "$vc_tmp")

  # rg exits 1 for "no match" (expected on the ignored/binary/case-mismatch
  # queries below) and 2 for a real error — only the latter is fatal here.
  # </dev/null matters: without it, rg inherits this loop's stdin (the
  # queries file, mid-read via `done < "$queries"` below) and, since that
  # fd isn't a tty, silently switches to searching STDIN instead of the
  # corpus directory — same reason vc's invocation above is also </dev/null.
  rg_out=$(cd "$corpus" && rg --no-config --hidden --no-ignore-global \
    -g '!.git' -g '!.vc' "${rg_ignore_args[@]}" \
    --line-number "${rgargs[@]}" </dev/null)
  rg_rc=$?
  if [ "$rg_rc" -gt 1 ]; then
    echo "rg error (exit $rg_rc) on query [$mode] $q: $rg_out" >&2
    exit "$rg_rc"
  fi

  # Same PIPESTATUS discipline as the vc side. `grep -v '^$'` legitimately
  # exits 1 when rg_out was empty (every line filtered out — zero matches is
  # not an error); only grep exit >1, or any nonzero from cut/sort, is fatal.
  printf '%s' "$rg_out" | grep -v '^$' | cut -d: -f1,2 | sort -u >"$rg_tmp"
  rg_pipe=("${PIPESTATUS[@]}") # printf grep cut sort
  if [ "${rg_pipe[1]}" -gt 1 ] || [ "${rg_pipe[2]}" -ne 0 ] || [ "${rg_pipe[3]}" -ne 0 ]; then
    echo "rg parse pipeline failed (grep=${rg_pipe[1]} cut=${rg_pipe[2]} sort=${rg_pipe[3]}) on query [$mode] $q" >&2
    echo "raw rg output: $rg_out" >&2
    exit 1
  fi
  rg_set=$(cat "$rg_tmp")

  if ! diff <(echo "$vc_set") <(echo "$rg_set") >/dev/null; then
    echo "R1 MISMATCH [$mode] $q"
    diff <(echo "$vc_set") <(echo "$rg_set") || true
    fail=1
  fi
done <"$queries"

if [ "$fail" -eq 0 ]; then
  echo "R1 lexical parity: PASS ($n queries, 0 mismatches)"
else
  echo "R1 lexical parity: FAIL — see mismatches above"
fi
exit $fail
