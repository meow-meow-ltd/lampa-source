#!/usr/bin/env bash
# Portion-diff audit for upstream-sync / rebase branches.
#
# Answers: "what does BASE (default: origin/main) have that TARGET (default: HEAD)
# does NOT have?" — on file and line level, plus a guard for the parts of this
# repository that only exist in our fork (backend/, CI, Docker, delivery docs).
#
# Why: when a branch rebases 400+ upstream commits, real fork features can be
# silently dropped while resolving conflicts (this already happened once with
# the local-account layer: Permit.isLocal / profileId / provider guards vanished,
# so profiles stopped loading). This script catches that class of regression.
#
# Usage:
#   scripts/diff-vs-main.sh                  # report only: origin/main vs HEAD
#   scripts/diff-vs-main.sh --strict         # CI mode: exit 1 on fork regressions
#   scripts/diff-vs-main.sh main HEAD
#   scripts/diff-vs-main.sh --strict origin/main HEAD~3
#   FETCH=1 scripts/diff-vs-main.sh          # git fetch origin --prune first
#
# Exit codes: 0 = ok, 1 = fork regression (strict mode), 2 = usage/refs error.
#
# Interpretation:
#   large "missing" counts in files our fork never touched (player/, advert/,
#   generated css) are normal — upstream rewrote them. Only FORK-ONLY paths and
#   FORK MARKERS are hard failures.

set -uo pipefail

STRICT=0
if [ "${1:-}" = "--strict" ]; then STRICT=1; shift; fi

BASE=${1:-origin/main}
TARGET=${2:-HEAD}
FORK_ONLY=(backend .github Dockerfile DELIVERY.md)
# Markers that exist only in our fork's layer. If their count drops, we lost code.
TOKENS=(account_provider isLocal isCub profileId device_token pin_gate ip_rate_limits)

violations=0
viol() { violations=$((violations + 1)); printf '\033[31m  ❌ %s\033[0m\n' "$*"; }
ok()   { printf '\033[32m  ✅ %s\033[0m\n' "$*"; }
say()  { printf '\n\033[1m== %s\033[0m\n' "$*"; }

cd "$(git rev-parse --show-toplevel)" 2>/dev/null || { echo "not a git repo" >&2; exit 2; }

for ref in "$BASE" "$TARGET"; do
  git rev-parse --verify --quiet "$ref^{commit}" >/dev/null || {
    echo "unknown ref: $ref" >&2; exit 2; }
done

if [ "${FETCH:-0}" = "1" ]; then git fetch origin --prune -q || true; fi

say "refs"
git log -1 --format='  base    %h  %ci  %s' "$BASE"
git log -1 --format='  target  %h  %ci  %s' "$TARGET"
printf '  merge-base: %s\n' "$(git log -1 --format='%h %ci %s' "$(git merge-base "$BASE" "$TARGET")")"

say "1) файли, які Є у $BASE, але ЗНИКЛИ у $TARGET"
deleted=$(git diff --name-status "$BASE" "$TARGET" --diff-filter=D)
if [ -z "$deleted" ]; then
  echo "  (немає)"
else
  printf '%s\n' "$deleted" | sed 's/^/  /'
fi

say "2) файли, яких НЕ БУЛО у $BASE (нові у $TARGET)"
added_count=$(git diff --name-status "$BASE" "$TARGET" --diff-filter=A | wc -l)
echo "  усього: $added_count"
git diff --name-status "$BASE" "$TARGET" --diff-filter=A | head -20 | sed 's/^/  /'
[ "$added_count" -gt 20 ] && echo "  ... (показано 20 з $added_count)"

say "3) рядки, які Є у $BASE, але ЯКИХ НЕМАЄ у $TARGET (топ-20 файлів)"
git diff -U0 --no-color "$BASE" "$TARGET" 2>/dev/null | awk '
  /^diff --git/ { f=$3; sub(/^a\//,"",f); file=f; next }
  /^-[^-]/ { miss[file]++ }
  END { for (f in miss) printf "%6d  %s\n", miss[f], f }
' | sort -rn | head -20 | sed 's/^/  /'

say "4) ГВАРД: fork-only теки"
for path in "${FORK_ONLY[@]}"; do
  if ! git cat-file -e "$TARGET:$path" 2>/dev/null; then
    viol "у $TARGET немає '$path' (є у $BASE)"
    continue
  fi
  lost=$(git diff -U0 --no-color "$BASE" "$TARGET" -- "$path" | grep -c '^-[^-]')
  if [ "$lost" -eq 0 ]; then
    ok "$path — втрат немає"
  else
    viol "$path — $lost рядків з $BASE відсутні у $TARGET"
    git diff -U0 --no-color "$BASE" "$TARGET" -- "$path" | grep '^-[^-]' | head -5 | sed 's/^/       /'
  fi
done

say "5) ГВАРД: маркери нашого fork-шару (src/, backend/)"
for token in "${TOKENS[@]}"; do
  base_n=$(git grep -I -o -F -- "$token" "$BASE" -- src backend 2>/dev/null | wc -l)
  target_n=$(git grep -I -o -F -- "$token" "$TARGET" -- src backend 2>/dev/null | wc -l)
  if [ "$target_n" -lt "$base_n" ]; then
    viol "маркер '$token': у $BASE $base_n, у $TARGET $target_n (втрата)"
  elif [ "$base_n" -gt 0 ]; then
    printf '  ✅ %-18s base=%-4s target=%-4s\n' "$token" "$base_n" "$target_n"
  fi
done

say "підсумок"
if [ "$violations" -eq 0 ]; then
  printf '\033[32m  fork-шар цілий: втрат не виявлено.\033[0m\n'
else
  printf '\033[31m  знайдено %s проблем(и).\033[0m\n' "$violations"
  echo "  Деталі по файлу: git diff $BASE $TARGET -- <path>"
  if [ "$STRICT" -eq 1 ]; then
    echo "  strict mode -> exit 1"
    exit 1
  fi
fi

echo "  (нагадування: великі втрати у player/, advert/, *.css — це upstream-переписування, не наш код)"
exit 0
