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
#   --ignore-file <corpus>/.vcignore (when present)
#                         vc registers ".vcignore" as a per-directory custom
#                         ignore filename via add_custom_ignore_filename;
#                         the fixture corpus keeps a single root-level
#                         .vcignore, so a flat --ignore-file is equivalent
#   (no flag)             .gitignore and ripgrep's own .ignore convention are
#                         both honored by default on both sides — vc's
#                         WalkBuilder defaults (git_ignore(true), ignore(true))
#                         match rg's defaults exactly, since vc and rg share
#                         the same underlying `ignore` crate
#   (no flag)             binary-file skip (NUL byte in the first chunk) is
#                         each tool's own default; neither needs -a/--text
#
# NOT pinned, deliberately: regex anchors/multiline. vc's search_regex runs
# `regex::bytes::Regex::find_iter` once over each file's WHOLE byte buffer
# (no `(?m)`), so `^`/`$` match only at the true start/end of the file. rg's
# default (non -U) mode feeds the regex engine one line at a time, so `^`/`$`
# match at every line boundary. That is a genuine architectural difference,
# not a bug or a missing flag — R1's query list (queries_lexical.txt) simply
# contains no anchored regex pattern, so parity here never exercises it.
set -uo pipefail
corpus="$(cd "$(dirname "$0")/corpus" && pwd)"
queries="$(dirname "$0")/queries_lexical.txt"

rg_ignore_args=()
if [ -f "$corpus/.vcignore" ]; then
  rg_ignore_args=(--ignore-file="$corpus/.vcignore")
fi

fail=0
n=0

while IFS=$'\t' read -r mode q || [ -n "$mode" ]; do
  [ -z "$mode" ] && continue
  n=$((n + 1))
  case "$mode" in
  L) vcargs=(query "$q") rgargs=(-F -- "$q") ;;
  R) vcargs=(query "$q" --regex) rgargs=(-- "$q") ;;
  *)
    echo "queries_lexical.txt: bad mode '$mode' on line $n" >&2
    exit 2
    ;;
  esac

  vc_out=$(cd "$corpus" && vc "${vcargs[@]}" --json </dev/null)
  vc_rc=$?
  if [ "$vc_rc" -ne 0 ]; then
    echo "vc error (exit $vc_rc) on query [$mode] $q: $vc_out" >&2
    exit "$vc_rc"
  fi
  vc_set=$(printf '%s' "$vc_out" | jq -r '.hits[] | "\(.path):\(.line)"' | sort -u)

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
  rg_set=$(printf '%s' "$rg_out" | grep -v '^$' | cut -d: -f1,2 | sort -u)

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
