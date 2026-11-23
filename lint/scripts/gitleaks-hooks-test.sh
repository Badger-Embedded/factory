#!/usr/bin/env bash
set -euo pipefail

# Keep the user's git config (signing, hooks, templates) out of the throwaway repos.
export GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_NOSYSTEM=1

prek_toml=$(cd "$(dirname "$0")/../.." && pwd)/prek.toml
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
failures=0

new_repo() {
    repo=$work/$1
    git init -q -b main "$repo"
    git -C "$repo" config user.name test
    git -C "$repo" config user.email test@example.invalid
    cp "$prek_toml" "$repo/prek.toml"
    git -C "$repo" add prek.toml
    git -C "$repo" commit -q -m base
}

write_token() {
    printf 'token = "ghp_%s"\n' "$(head -c 400 /dev/urandom | tr -dc A-Za-z0-9 | head -c 36)" >"$repo/$1"
}

expect() {
    local want=$1 desc=$2 hook=$3 out status=0
    out=$(cd "$repo" && prek run "$hook" --all-files 2>&1) || status=$?
    if [[ $want == pass && $status -eq 0 ]] ||
        [[ $want == leak && $status -ne 0 && $out == *"leaks found: 1"* ]]; then
        echo "ok    $desc"
    else
        echo "FAIL  $desc (expected $want, exit $status)"
        printf '      %s\n' "${out//$'\n'/$'\n      '}"
        failures=$((failures + 1))
    fi
}

new_repo staged-clean
echo hello >"$repo/a.txt"
git -C "$repo" add a.txt
expect pass "staged hook passes a clean change" gitleaks-staged

new_repo staged-leak
write_token leak.txt
git -C "$repo" add leak.txt
expect leak "staged hook fails a staged token" gitleaks-staged

new_repo history-clean
echo hello >"$repo/a.txt"
git -C "$repo" add a.txt
git -C "$repo" commit -q -m clean
expect pass "history hook passes a clean branch" gitleaks-history

new_repo history-leak
write_token leak.txt
git -C "$repo" add leak.txt
git -C "$repo" commit -q -m leak
git -C "$repo" rm -q leak.txt
git -C "$repo" commit -q -m remove
expect leak "history hook fails a token committed earlier" gitleaks-history

new_repo history-other-ref
git -C "$repo" checkout -q -b other
write_token leak.txt
git -C "$repo" add leak.txt
git -C "$repo" commit -q -m leak
git -C "$repo" checkout -q main
expect pass "history hook ignores a token on an unrelated branch" gitleaks-history

new_repo history-merge
echo one >"$repo/a.txt"
git -C "$repo" add a.txt
git -C "$repo" commit -q -m a
git -C "$repo" checkout -q -b side
echo two >"$repo/b.txt"
git -C "$repo" add b.txt
git -C "$repo" commit -q -m b
git -C "$repo" checkout -q main
echo three >"$repo/c.txt"
git -C "$repo" add c.txt
git -C "$repo" commit -q -m c
git -C "$repo" merge -q --no-commit --no-ff side
write_token resolved.txt
git -C "$repo" add resolved.txt
git -C "$repo" commit -q -m merge
expect leak "history hook fails a token added in a merge commit" gitleaks-history

exit $((failures > 0))
