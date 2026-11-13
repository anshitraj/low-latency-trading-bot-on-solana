#!/usr/bin/env bash
# Build agave-validator from the official repo at a pinned tag (Anza no longer
# ships the validator binary in release installers). Run as root.
set -euo pipefail
TAG="${TAG:-v4.4.1}"
export DEBIAN_FRONTEND=noninteractive
apt-get install -y -qq build-essential pkg-config libssl-dev libudev-dev zlib1g-dev \
  llvm clang libclang-dev cmake protobuf-compiler libprotobuf-dev git >/dev/null
sudo -iu sol bash -s "$TAG" <<'EOS'
set -euo pipefail
TAG="$1"
if ! command -v ~/.cargo/bin/rustup >/dev/null 2>&1; then
  curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal >/dev/null
fi
source ~/.cargo/env
cd ~
[ -d agave ] || git clone -q --depth 1 --branch "$TAG" https://github.com/anza-xyz/agave.git
cd agave
git describe --tags --exact-match
# rust-toolchain.toml in the repo pins the compiler version.
cargo build --release --bin agave-validator 2>&1 | tail -3
mkdir -p ~/bin
cp target/release/agave-validator ~/bin/
~/bin/agave-validator --version
EOS
echo BUILD_DONE
