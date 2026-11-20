#!/usr/bin/env bash
# Self-test for install.sh: runs it against stub podman, systemctl, tailscale,
# curl and gh commands, and checks that the installed Quadlet unit passes the
# claude-token secret only when it exists.
set -euo pipefail

src=$(cd "$(dirname "$0")/.." && pwd)
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
failures=0

# Stub commands on PATH. podman reads and writes secrets as files under
# $secrets so the test controls which exist; the rest exit 0, and curl reports
# the dashboard up so install.sh stops before its journalctl dump.
bin=$work/bin
secrets=$work/secrets
mkdir -p "$bin" "$secrets"

cat >"$bin/podman" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
dir=$PODMAN_SECRETS_DIR
case "$1 ${2:-}" in
  "secret exists") [[ -e "$dir/$3" ]] ;;
  "secret create")
    # args: secret create [--replace] NAME -
    name=$3; [[ $name == --replace ]] && name=$4
    cat >"$dir/$name" ;;
  "secret inspect")
    # args end with NAME; the real podman prints the value with a trailing
    # newline of its own.
    for name in "$@"; do :; done
    printf '%s\n' "$(cat "$dir/$name")" ;;
  "network exists") exit 0 ;;
  *) exit 0 ;;
esac
EOF

for cmd in systemctl tailscale gh; do
    cat >"$bin/$cmd" <<'EOF'
#!/usr/bin/env bash
case "$*" in
  *"serve status"*) echo "https://example.ts.net 127.0.0.1:8080" ;;
  *"auth token"*) echo "ghtok" ;;
esac
exit 0
EOF
done

cat >"$bin/curl" <<'EOF'
#!/usr/bin/env bash
exit 0
EOF
chmod +x "$bin"/*

unit=$work/home/.config/containers/systemd/factory.container

# run: install.sh with a fresh HOME, printing its exit status into $status.
run() {
    rm -rf "${work:?}/home"
    mkdir -p "$work/home"
    if HOME=$work/home PATH="$bin:$PATH" PODMAN_SECRETS_DIR=$secrets \
        bash "$src/deploy/install.sh" "$src" >/dev/null 2>&1; then
        status=pass
    else
        status=fail
    fi
}

# check <pass|fail> <description>
check() {
    if [[ $status == "$1" ]]; then
        echo "ok    $2"
    else
        echo "FAIL  $2 (expected $1, got $status)"
        failures=$((failures + 1))
    fi
}

# has <yes|no> <line> <description>
has() {
    local got=no
    grep -qxF "$2" "$unit" && got=yes
    if [[ $got == "$1" ]]; then
        echo "ok    $3"
    else
        echo "FAIL  $3 (expected $1, got $got)"
        failures=$((failures + 1))
    fi
}

claude_line='Secret=claude-token,type=env,target=CLAUDE_CODE_OAUTH_TOKEN'

# Without the claude-token secret: install succeeds and the unit omits the line.
rm -f "$secrets"/*
run
check pass "install succeeds without the claude-token secret"
has no "$claude_line" "unit has no claude-token Secret line"
has yes "Secret=gh-token,type=env,target=GH_TOKEN" "unit still passes gh-token"
has yes "Secret=factory-master-key,type=env,target=FACTORY_MASTER_KEY" "unit still passes the master key"

# With the claude-token secret present: the unit passes it in.
printf 'claudetok' >"$secrets/claude-token"
run
check pass "install succeeds with the claude-token secret"
has yes "$claude_line" "unit passes claude-token when the secret exists"

if [[ $failures -gt 0 ]]; then
    echo "$failures check(s) failed"
    exit 1
fi
echo "all checks passed"
