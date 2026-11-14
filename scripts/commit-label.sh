#!/usr/bin/env bash
# Prints the checkout's commit as a benchmark label: the full SHA, with "-dirty" appended when
# tracked files have uncommitted changes, or "unknown" when git cannot be read.
#
#   scripts/commit-label.sh
#
# A worktree checked out from Windows has a .git file with a Windows path that Linux git cannot
# follow; WSL can still run the Windows git.
set -uo pipefail
cd "$(dirname "$0")/.." || exit 1
for g in git git.exe; do
    if command -v "$g" >/dev/null && sha="$("$g" rev-parse HEAD 2>/dev/null)"; then
        if [ -n "$("$g" status --porcelain --untracked-files=no 2>/dev/null)" ]; then
            sha="$sha-dirty"
        fi
        echo "$sha"
        exit 0
    fi
done
echo unknown
