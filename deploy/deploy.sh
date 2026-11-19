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
git fetch --quiet origin
commit=$(git rev-parse --verify "$ref^{commit}")
echo "Deploying $(git log -1 --format='%h %s' "$commit") to $host"

# $src is expanded here on purpose: it is a fixed directory name.
# shellcheck disable=SC2029
ssh "$host" "rm -rf ~/$src && mkdir -p ~/$src"
# shellcheck disable=SC2029
git archive --format=tar "$commit" | ssh "$host" "tar -x -C ~/$src"
# shellcheck disable=SC2029
ssh "$host" "bash ~/$src/deploy/install.sh ~/$src"
