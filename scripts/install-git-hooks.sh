#!/usr/bin/env bash
# Install (or re-install) the project git hooks into .git/hooks/.
#
# Uses symlinks so edits to scripts/git-hooks/* take effect immediately
# for anyone who has run this script.
#
# Run from the workspace root:
#
#     scripts/install-git-hooks.sh

set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
workspace_root="$(cd "${script_dir}/.." && pwd)"

if [[ ! -d "${workspace_root}/.git" ]]; then
    printf "error: not a git checkout (no .git at %s)\n" "${workspace_root}" >&2
    exit 1
fi

src_dir="${script_dir}/git-hooks"
dst_dir="${workspace_root}/.git/hooks"

mkdir -p "${dst_dir}"

installed=0
for src in "${src_dir}"/*; do
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

printf "done: %d hook(s) installed\n" "${installed}"
