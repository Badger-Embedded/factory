#!/usr/bin/env bash
# Deploys a git ref of this repository to the factory host: copies the
# source, builds the images there and restarts the factory.
#
# Usage: deploy.sh [REF]   REF defaults to origin/main.
# FACTORY_HOST sets the ssh destination, moon@moon.local by default.
set -euo pipefail

ref=${1:-origin/main}
host=${FACTORY_HOST:-moon@moon.local}
src=factory-src

cd "$(git rev-parse --show-toplevel)"
# BatchMode makes a fetch that needs a passphrase or a key touch fail at once
# instead of waiting for input that a non-interactive shell never gets.
if ! GIT_SSH_COMMAND="ssh -o BatchMode=yes" git fetch --quiet origin; then
    echo "warning: could not fetch origin, deploying $ref as it is locally" >&2
fi
commit=$(git rev-parse --verify "$ref^{commit}")
echo "Deploying $(git log -1 --no-show-signature --format='%h %s' "$commit") to $host"

# $src is expanded here on purpose: it is a fixed directory name.
# shellcheck disable=SC2029
ssh "$host" "rm -rf ~/$src && mkdir -p ~/$src"
# shellcheck disable=SC2029
git archive --format=tar "$commit" | ssh "$host" "tar -x -C ~/$src"
# shellcheck disable=SC2029
ssh "$host" "bash ~/$src/deploy/install.sh ~/$src"
