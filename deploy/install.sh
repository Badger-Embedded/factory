#!/usr/bin/env bash
# Runs on the host as the user that runs the factory. Builds the images from
# the source in $1, installs the Quadlet unit and restarts the factory.
set -euo pipefail

src=${1:?usage: install.sh SOURCE_DIR}
cd "$src"

if ! podman secret exists gh-token; then
    gh auth token | podman secret create gh-token -
fi
if ! podman secret exists claude-token; then
    echo "The podman secret claude-token is missing." >&2
    echo "Run 'claude setup-token' and pipe the token into 'podman secret create claude-token -'." >&2
    exit 1
fi

podman build --tag localhost/factory-worker:latest --build-arg "UID=$(id -u)" containers/worker
podman build --tag localhost/factory:latest --file containers/factory/Containerfile .

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
