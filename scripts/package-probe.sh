#!/usr/bin/env bash
# Requires git, rustup, zig, cargo-zigbuild, and readelf (binutils). No build.rs network access.
set -euo pipefail
cd "$(dirname "$0")/.."
probe_target="${1:-x86_64-unknown-linux-musl}"
case "$probe_target" in
  x86_64-unknown-linux-musl|aarch64-unknown-linux-musl) ;;
  *) echo 'Supported targets: x86_64-unknown-linux-musl, aarch64-unknown-linux-musl' >&2; exit 1 ;;
esac
probe_arch="${probe_target%%-*}"
probe_root="$PWD"
probe_sources="$probe_root/target/probe-sources"
probe_assets="$probe_root/target/probe-assets/$probe_arch"
mkdir -p "$probe_sources" "$probe_assets"
rustup target add "$probe_target"
rustup component add llvm-tools
probe_host="$(rustc -vV | sed -n 's/^host: //p')"
probe_strip="$(rustc --print sysroot)/lib/rustlib/$probe_host/bin/llvm-strip"
prepare_source() {
  local tool_name="$1" tool_url="$2" tool_commit="$3"
  local source_dir="$probe_sources/$tool_name"
  if [[ ! -d "$source_dir/.git" ]]; then git init "$source_dir"; fi
  git -C "$source_dir" fetch --depth 1 "$tool_url" "$tool_commit"
  git -C "$source_dir" checkout --detach FETCH_HEAD
}
prepare_source ripgrep https://github.com/BurntSushi/ripgrep.git 6ec72defacfb042f203ca0b4bf2513a0a5505a7e
prepare_source fd https://github.com/sharkdp/fd.git 4f81778774463bf414a184cbe6d5219ad2229646
cargo zigbuild --locked --release --target "$probe_target" --manifest-path "$probe_sources/ripgrep/Cargo.toml" --target-dir "$probe_root/target/probe-tools" --bin rg
cargo zigbuild --locked --release --target "$probe_target" --manifest-path "$probe_sources/fd/Cargo.toml" --target-dir "$probe_root/target/probe-tools" --bin fd
cp "$probe_root/target/probe-tools/$probe_target/release/rg" "$probe_assets/rg"
cp "$probe_root/target/probe-tools/$probe_target/release/fd" "$probe_assets/fd"
for probe_tool in rg fd; do
  "$probe_strip" --strip-all "$probe_assets/$probe_tool"
  chmod +x "$probe_assets/$probe_tool"
done
cp "$probe_sources/ripgrep/COPYING" "$probe_assets/ripgrep-COPYING"
cp "$probe_sources/ripgrep/LICENSE-MIT" "$probe_assets/ripgrep-LICENSE-MIT"
cp "$probe_sources/fd/LICENSE-MIT" "$probe_assets/fd-LICENSE-MIT"
cp "$probe_sources/fd/LICENSE-APACHE" "$probe_assets/fd-LICENSE-APACHE"
if [[ "${PSA_PREPARE_ONLY:-0}" != 1 ]]; then
  cargo zigbuild --locked --release --target "$probe_target" -p psa_probe
  "$probe_strip" --strip-all "$probe_root/target/$probe_target/release/psa_probe"
  for probe_binary in "$probe_root/target/$probe_target/release/psa_probe" "$probe_assets/rg" "$probe_assets/fd"; do
    if readelf -l "$probe_binary" | grep -q INTERP || readelf -d "$probe_binary" | grep -q NEEDED; then
      echo "Dynamic dependency found in $probe_binary" >&2; exit 1
    fi
  done
fi
