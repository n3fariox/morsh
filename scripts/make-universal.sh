#!/usr/bin/env bash
# Build the universal (fat) macOS tarball from the per-arch tarballs.
#
# Shared by .github/workflows/release.yml (the tag release) and the
# universal-smoke job in .github/workflows/ci.yml, so CI exercises the exact
# shipping code path instead of a copy of it.
#
# Usage:
#   make-universal.sh bins
#       Print the packaged binary names, one per line. Both the per-arch
#       packaging steps and the smoke job read this list, so it cannot drift.
#
#   make-universal.sh make <tag> <artifacts-dir>
#       Create ./morsh-<tag>-universal-apple-darwin.tar.gz in the current
#       directory and print the asset filename on stdout. <artifacts-dir>
#       must hold the per-arch tarballs in the layout that
#       actions/download-artifact produces (nested by artifact name):
#         <artifacts-dir>/morsh-aarch64-apple-darwin/morsh-<tag>-aarch64-apple-darwin.tar.gz
#         <artifacts-dir>/morsh-x86_64-apple-darwin/morsh-<tag>-x86_64-apple-darwin.tar.gz
#       Each tarball contains a morsh-<tag>-<target>/ directory holding the
#       binaries, as produced by the release job's per-arch packaging step.
#
# Requires lipo (macOS). All progress output goes to stderr; stdout carries
# only the asset filename.

set -euo pipefail

BINS=(morsh morsh-client morsh-server)

make_universal() {
  local tag="$1" artifacts_dir="$2" staging target bin
  staging="morsh-${tag}-universal-apple-darwin"

  mkdir -p "$staging"

  for target in aarch64-apple-darwin x86_64-apple-darwin; do
    tar xzf "${artifacts_dir}/morsh-${target}/morsh-${tag}-${target}.tar.gz" -C "${artifacts_dir}"
  done

  for bin in "${BINS[@]}"; do
    lipo -create \
      "${artifacts_dir}/morsh-${tag}-aarch64-apple-darwin/${bin}" \
      "${artifacts_dir}/morsh-${tag}-x86_64-apple-darwin/${bin}" \
      -output "${staging}/${bin}"
    lipo -info "${staging}/${bin}" >&2
  done

  tar czf "${staging}.tar.gz" "$staging"
  echo "${staging}.tar.gz"
}

case "${1:-}" in
  bins)
    printf '%s\n' "${BINS[@]}"
    ;;
  make)
    if [ $# -ne 3 ]; then
      echo "usage: make-universal.sh make <tag> <artifacts-dir>" >&2
      exit 2
    fi
    make_universal "$2" "$3"
    ;;
  *)
    echo "usage: make-universal.sh {bins|make <tag> <artifacts-dir>}" >&2
    exit 2
    ;;
esac
