#!/usr/bin/env bash
set -euo pipefail
root=$(cd "$(dirname "$0")/../.." && pwd)
base=${SHOTS_TEST_TMPDIR:-${TMPDIR:-/arboretum/tokomotive-screenshot-tests}}
mkdir -p "$base"
fixture=$(mktemp -d "$base/hook.XXXXXXXX")
trap 'rm -rf "$fixture"' EXIT
git init -q "$fixture"
cd "$fixture"
# Do not read or change the user's Git identity or hook configuration.
export GIT_AUTHOR_NAME=Test GIT_COMMITTER_NAME=Test
export GIT_AUTHOR_EMAIL=test@example.invalid GIT_COMMITTER_EMAIL=test@example.invalid
export SKIP_SCREENSHOTS=0
mkdir -p scripts/screenshots templates docs/images
cp "$root/scripts/screenshots/manifest.json" scripts/screenshots/
cp "$root/scripts/screenshots/check_push.py" scripts/screenshots/
commit() { git add .; git -c core.hooksPath=/dev/null commit -qm "$1"; }
echo initial > docs/images/demo.png
commit initial
base_commit=$(git rev-parse HEAD)
zero=0000000000000000000000000000000000000000
check() {
    local expect=$1 old=$2 tip=$3 result=0
    printf 'refs/heads/test %s refs/heads/test %s\n' "$tip" "$old" |
        bash "$root/.githooks/pre-push" origin https://example.invalid/repo.git >"$fixture/output" 2>&1 || result=$?
    if [[ $expect == allow && $result != 0 || $expect == block && $result == 0 ]]; then
        cat "$fixture/output" >&2
        echo "expected $expect, got exit $result" >&2
        exit 1
    fi
}
check allow "$zero" "$base_commit" # new branch with no uncovered surface
echo page > templates/demo.html
commit surface
surface=$(git rev-parse HEAD)
check block "$base_commit" "$surface"
check block "$zero" "$surface" # new branch cannot hide old surface changes
SKIP_SCREENSHOTS=1 check allow "$base_commit" "$surface"
grep -q WARNING "$fixture/output"
echo refreshed >> docs/images/demo.png
commit images
fresh=$(git rev-parse HEAD)
check allow "$base_commit" "$fresh"
check allow "$zero" "$fresh"
check allow "$surface" "$zero" # deletion
echo page2 >> templates/demo.html
echo refreshed2 >> docs/images/demo.png
commit combined
check allow "$fresh" "$(git rev-parse HEAD)" # same commit covers both
echo note > note.txt
commit docs
check allow "$fresh" "$(git rev-parse HEAD)"
git checkout -q -b rewritten "$base_commit"
mkdir -p templates
echo rewritten > templates/demo.html
commit rewritten
check block "$fresh" "$(git rev-parse HEAD)" # force push to divergent history
echo fixed >> docs/images/demo.png
commit fixed
check allow "$fresh" "$(git rev-parse HEAD)"
echo fixture > scripts/screenshots/new-recipe.js
commit recipe
check block "$fresh" "$(git rev-parse HEAD)" # manifest-declared surface glob
check block "$zero" "$(git rev-parse HEAD)"
echo 'screenshot hook: all cases passed'
