#!/usr/bin/env bash
# Runs on the host as the user that runs the factory. Builds the images from
# the source in $1, installs the Quadlet unit and restarts the factory.
set -euo pipefail

src=${1:?usage: install.sh SOURCE_DIR}
cd "$src"

# strip_newline NAME: rewrites secret NAME without its trailing newline.
# `podman secret create NAME -` stores stdin as it is, and a token piped from
# a command or typed into a terminal ends in one, which breaks the
# Authorization header built from it.
# `podman secret inspect --format` prints its own newline after the value, so
# a value that ends in a newline ends in two in its output.
strip_newline() {
    local value last
    last=$(podman secret inspect --showsecret --format '{{.SecretData}}' "$1" | tail -c 2 | od -An -c | tr -d ' ')
    if [[ $last == '\n\n' ]]; then
        value=$(podman secret inspect --showsecret --format '{{.SecretData}}' "$1")
        printf '%s' "$value" | podman secret create --replace "$1" - >/dev/null
        echo "Removed the trailing newline from the $1 secret."
    fi
}

if ! podman secret exists gh-token; then
    gh auth token | tr -d '\n' | podman secret create gh-token - >/dev/null
fi
if ! podman secret exists claude-token; then
    echo "The podman secret claude-token is missing." >&2
    echo "Run 'claude setup-token' and pipe the token into 'podman secret create claude-token -'." >&2
    exit 1
fi
strip_newline gh-token
strip_newline claude-token

# The docker format keeps the Containerfiles' SHELL, which sets pipefail for
# their curl | sh steps. The OCI format ignores it.
podman build --format docker --tag localhost/factory-worker:latest --build-arg "UID=$(id -u)" --file containers/worker/Containerfile .
podman build --format docker --tag localhost/factory:latest --file containers/factory/Containerfile .

mkdir -p ~/.local/share/factory ~/.config/containers/systemd
cp deploy/factory.container ~/.config/containers/systemd/factory.container
systemctl --user daemon-reload
systemctl --user restart factory.service

if ! tailscale serve status 2>/dev/null | grep -q 127.0.0.1:8080; then
    tailscale serve --bg http://127.0.0.1:8080
fi

for _ in $(seq 1 30); do
    if curl -fsS -o /dev/null http://127.0.0.1:8080/; then
        echo "factory is up: $(tailscale serve status 2>/dev/null | head -1)"
        exit 0
    fi
    sleep 2
done
echo "factory did not answer on 127.0.0.1:8080" >&2
journalctl --user -u factory.service -n 50 --no-pager >&2
exit 1
