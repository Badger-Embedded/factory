#!/usr/bin/env bash
# Fail when a change range modifies crates/*/src/ without touching any
# crates/*/tests/.
#
# Tests may come from any commit in the range. A commit whose message has a
# `No-Tests: <reason>` trailer exempts that commit's own changes only.
#
# Usage: tests-accompany-changes.sh [FROM_REF [TO_REF]]
# Without arguments it reads PRE_COMMIT_FROM_REF / PRE_COMMIT_TO_REF, which
# prek sets for `prek run --from-ref X --to-ref Y`. With no range it skips.
set -euo pipefail

from=${1:-${PRE_COMMIT_FROM_REF:-}}
to=${2:-${PRE_COMMIT_TO_REF:-HEAD}}

if [[ -z $from ]]; then
    echo "tests-accompany-changes: no ref range given, skipping"
    exit 0
fi

src_re='^crates/[^/]+/src/'
tests_re='^crates/[^/]+/tests/'

have_tests=0
uncovered=()

while read -r commit; do
    # --no-show-signature: with log.showSignature set, gpg output would land in
    # $reason and make every signed commit look exempt.
    reason=$(git log -1 --no-show-signature --format='%(trailers:key=No-Tests,valueonly)' "$commit")
    exempt=0
    [[ -n ${reason//[[:space:]]/} ]] && exempt=1
    short=$(git rev-parse --short "$commit")

    while read -r file; do
        [[ -z $file ]] && continue
        if [[ $file =~ $tests_re ]]; then
            have_tests=1
        elif ((exempt)); then
            continue
        elif [[ $file =~ $src_re ]]; then
            uncovered+=("$short $file")
        fi
    done < <(git diff-tree --no-commit-id --name-only -r "$commit")
done < <(git rev-list --no-merges "$from..$to")

if ((${#uncovered[@]})) && ((!have_tests)); then
    echo "Crate code changed without a test:"
    printf '  %s\n' "${uncovered[@]}" | sort -u
    echo "Add or update a test under crates/*/tests/."
    echo
    echo "If a commit above changes no behaviour (refactor, rename, doc comment),"
    echo "add a trailer with the reason to that commit's message instead:"
    echo "  No-Tests: <why no test is needed>"
    echo "See CONTRIBUTING.md."
    exit 1
fi
