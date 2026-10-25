#!/usr/bin/env bash
# Build standalone probe binaries without host Rust/Zig dependencies.
set -euo pipefail

usage() {
  echo "Usage: $0 [amd64|arm64|all] [OUTPUT_ROOT]"
  echo "Defaults: amd64, <workspace>/target/probe-package"
  echo "Each binary is exported to OUTPUT_ROOT/<architecture>/psa_probe."
}

if [[ $# -gt 2 ]]; then
  usage >&2
  exit 2
fi

case "${1:-amd64}" in
  amd64|x86_64-unknown-linux-musl) architectures=(amd64) ;;
  arm64|aarch64-unknown-linux-musl) architectures=(arm64) ;;
  all) architectures=(amd64 arm64) ;;
  -h|--help) usage; exit 0 ;;
  *) usage >&2; exit 2 ;;
esac

if ! command -v docker >/dev/null 2>&1 || ! docker buildx version >/dev/null 2>&1; then
  echo 'Docker with Buildx is required.' >&2
  exit 1
fi

probe_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
output_root="${2:-$probe_root/target/probe-package}"

# Build separately so "all" also works with single-platform Docker builders.
for architecture in "${architectures[@]}"; do
  output_path="$output_root/$architecture"
  docker buildx build \
    --file "$probe_root/runtime/psa_probe/Dockerfile" \
    --platform "linux/$architecture" \
    --output "type=local,dest=$output_path" \
    "$probe_root"
  printf 'Probe exported to %s/psa_probe\n' "$output_path"
done
