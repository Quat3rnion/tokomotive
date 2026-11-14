#!/usr/bin/env python3
"""Check pushed commit ancestry, not dates (which can be rewritten)."""
import fnmatch
import json
import subprocess
import sys
from pathlib import Path


def git(*args):
    return subprocess.check_output(["git", *args], text=True).strip()


def ancestor(older, newer):
    return subprocess.run(
        ["git", "merge-base", "--is-ancestor", older, newer], check=False
    ).returncode == 0


def main():
    root = Path(git("rev-parse", "--show-toplevel"))
    manifest = json.loads((root / "scripts/screenshots/manifest.json").read_text())
    # These paths are mandatory; the manifest can add more, but cannot remove them.
    globs = ["templates/**", "src/web/**", "assets/**", "src/provider_types.rs"]
    globs += manifest["surface_globs"]
    blocked = []
    for line in sys.stdin:
        local_ref, tip, remote_ref, old = line.split()
        if set(tip) == {"0"}:  # A deleted remote ref introduces no pages.
            continue
        # Tags may point at non-commit objects.
        resolved = subprocess.run(
            ["git", "rev-parse", "--verify", f"{tip}^{{commit}}"],
            capture_output=True, text=True, check=False,
        )
        if resolved.returncode:
            continue
        tip = resolved.stdout.strip()
        known_old = set(old) != {"0"} and subprocess.run(
            ["git", "cat-file", "-e", f"{old}^{{commit}}"],
            capture_output=True, check=False,
        ).returncode == 0
        # New branches (or a remote object absent locally) check all reachable
        # history. Force pushes use old..tip even when old is not an ancestor.
        revisions = [tip, f"^{old}"] if known_old else [tip]
        changes = git("rev-list", *revisions).splitlines()
        images = git("log", "--format=%H", tip, "--", "docs/images/").splitlines()
        for commit in changes:
            paths = git("diff-tree", "--root", "-m", "--no-commit-id", "--name-only", "-r", commit).splitlines()
            if not any(fnmatch.fnmatchcase(path, pattern) for path in paths for pattern in globs):
                continue
            if not any(ancestor(commit, image) for image in images):
                blocked.append(f"{local_ref} -> {remote_ref}: web surface at {commit[:12]}")
                break
    if blocked:
        print("Push blocked: screenshots do not cover the latest web surface.", file=sys.stderr)
        print("\n".join(blocked), file=sys.stderr)
        print("Run scripts/screenshots/run.sh, then commit docs/images and push again.", file=sys.stderr)
        print("Use SKIP_SCREENSHOTS=1 only for an explicit exception (warning logged).", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
