#!/usr/bin/env bash
# Install Rill's git hooks into this clone. Run once after cloning.
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"
ln -sf ../../scripts/git-hooks/pre-push .git/hooks/pre-push
echo "installed .git/hooks/pre-push"
