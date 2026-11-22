# Run every lint and check (the same command runs in CI)
lint:
    prek run --all-files

# Install the git hooks
hooks:
    prek install

test:
    cargo test --workspace --locked

# Run the browser tests in headless Chromium: just e2e tests/board.spec.ts -g "name" runs one
[positional-arguments]
e2e *args:
    cd e2e && npm ci --prefer-offline
    cargo build --locked -p factory-server --bin factory-server --example e2e_fixture
    cd e2e && npx playwright test "$@"

# Check that committed changes since BASE come with tests (CI runs this on PRs)
check-tests base="origin/main":
    prek run --hook-stage manual tests-accompany-changes --from-ref {{base}} --to-ref HEAD

# Check a PR title and description: just check-pr "area: summary" body.md
check-pr title body_file base="origin/main":
    PR_TITLE={{quote(title)}} PR_BODY="$(cat {{quote(body_file)}})" lint/scripts/check-pr-message.sh {{base}}

# Deploy a git ref to moon.local: build the images there and restart the factory
deploy ref="origin/main":
    deploy/deploy.sh {{ref}}
