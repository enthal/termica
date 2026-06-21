#!/usr/bin/env bash
# Install (or re-install) the project git hooks.
#
# Git hooks live in the repository's *common* git dir, so a single
# install is shared by the primary checkout AND every linked worktree —
# you install once. This script is worktree-safe: it can be run from
# any worktree and always
#   - installs into the shared hooks dir (`git rev-parse --git-path
#     hooks`), not a per-worktree `.git/hooks` (in a worktree `.git` is
#     a file, not a directory), and
#   - points the symlinks at the PRIMARY worktree's scripts/git-hooks —
#     a stable path that survives removing any linked worktree (a
#     symlink into a worktree would dangle once that worktree is gone).
#
# Uses symlinks so edits to the primary checkout's scripts/git-hooks/*
# take effect immediately.
#
#     scripts/install-git-hooks.sh

set -euo pipefail

if ! git rev-parse --is-inside-work-tree >/dev/null 2>&1; then
    printf "error: not inside a git working tree\n" >&2
    exit 1
fi

# Shared hooks dir (the common git dir's hooks), as an absolute path.
common_dir="$(cd "$(git rev-parse --git-common-dir)" && pwd)"
dst_dir="${common_dir}/hooks"

# The primary worktree root is the parent of the common git dir; its
# scripts/git-hooks is the canonical, stable symlink source.
primary_root="$(dirname "${common_dir}")"
src_dir="${primary_root}/scripts/git-hooks"

if [[ ! -d "${src_dir}" ]]; then
    printf "error: no hooks found at %s\n" "${src_dir}" >&2
    printf "       (is the primary checkout on a branch that has scripts/git-hooks?)\n" >&2
    exit 1
fi

mkdir -p "${dst_dir}"

installed=0
for src in "${src_dir}"/*; do
    [[ -e "${src}" ]] || continue
    name="$(basename "${src}")"
    dst="${dst_dir}/${name}"
    chmod +x "${src}"
    # Remove any existing hook (symlink or regular file) before re-linking.
    rm -f "${dst}"
    ln -s "${src}" "${dst}"
    printf "installed hook: %s -> %s\n" "${dst}" "${src}"
    installed=$((installed + 1))
done

if [[ "${installed}" -eq 0 ]]; then
    printf "warning: no hooks found in %s\n" "${src_dir}" >&2
    exit 1
fi

printf "done: %d hook(s) installed into the shared hooks dir\n" "${installed}"
printf "      (%s) — applies to the primary checkout and all worktrees\n" "${dst_dir}"
