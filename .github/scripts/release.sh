#!/usr/bin/env bash
set -euo pipefail

: "${RELEASE_TAG:?RELEASE_TAG is required}"
: "${GH_REPO:?GH_REPO is required}"

package_dir="${1:-dist}"
shopt -s nullglob
packages=("$package_dir"/*.tar.gz "$package_dir"/*.zip)
checksums=("$package_dir"/*.sha256)
if [[ ${#packages[@]} -ne 6 || ${#checksums[@]} -ne 6 ]]; then
  echo "Expected six platform packages and six SHA256 files" >&2
  exit 1
fi
for package in "${packages[@]}"; do
  if [[ ! -f "$package.sha256" ]]; then
    echo "Missing SHA256 file for $package" >&2
    exit 1
  fi
done
(
  cd "$package_dir"
  sha256sum --check -- *.sha256
)

assets=("${packages[@]}" "${checksums[@]}")
if gh release view "$RELEASE_TAG" --repo "$GH_REPO" >/dev/null 2>&1; then
  # A rerun updates assets without replacing the release description or its release status.
  gh release upload "$RELEASE_TAG" "${assets[@]}" --repo "$GH_REPO" --clobber
else
  options=(--repo "$GH_REPO" --verify-tag --title "$RELEASE_TAG" --generate-notes)
  if [[ "$RELEASE_TAG" == *-* ]]; then
    options+=(--prerelease)
  fi
  gh release create "$RELEASE_TAG" "${assets[@]}" "${options[@]}"
fi
