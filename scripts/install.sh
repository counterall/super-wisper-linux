#!/usr/bin/env bash
# Builds and installs osw + osw-daemon into ~/.local/bin and registers the
# daemon to start with the desktop session.
#   scripts/install.sh          # Vulkan build
#   scripts/install.sh --cuda   # CUDA build (needs nvidia-cuda-toolkit, gcc-12)
set -euo pipefail
cd "$(dirname "$0")/.."

if [[ "${1:-}" == "--cuda" ]]; then
    export CMAKE_CUDA_HOST_COMPILER="${CMAKE_CUDA_HOST_COMPILER:-/usr/bin/g++-12}"
    if [[ -z "${CMAKE_CUDA_ARCHITECTURES:-}" ]]; then
        cap=$(nvidia-smi --query-gpu=compute_cap --format=csv,noheader | head -1)
        export CMAKE_CUDA_ARCHITECTURES="${cap/./}"
    fi
    target=target-cuda
    cargo build --release --no-default-features --features cuda \
        -p osw-cli -p osw-daemon --target-dir "$target"
else
    target=target
    cargo build --release -p osw-cli -p osw-daemon
fi

bin="$HOME/.local/bin"
mkdir -p "$bin" "$HOME/.config/autostart" "$HOME/.local/share/applications"
install -m 755 "$target/release/osw" "$target/release/osw-daemon" "$bin/"
install -m 644 packaging/opensuperwhisper.desktop "$HOME/.local/share/applications/"
install -m 644 packaging/opensuperwhisper.desktop "$HOME/.config/autostart/"
echo "Installed to $bin. Start now with: osw-daemon &"
