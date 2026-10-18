#!/usr/bin/env bash
# Builds and installs osw, osw-daemon and osw-ui into ~/.local/bin, adds a
# menu entry for the window and starts the daemon with the desktop session.
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

# The window does not use the inference engine, so one build fits all GPUs.
cargo build --release -p osw-ui

bin="$HOME/.local/bin"
mkdir -p "$bin" "$HOME/.config/autostart" "$HOME/.local/share/applications"
install -m 755 "$target/release/osw" "$target/release/osw-daemon" "$bin/"
install -m 755 target/release/osw-ui "$bin/"
install -m 644 packaging/opensuperwhisper.desktop "$HOME/.local/share/applications/"
install -m 644 packaging/opensuperwhisper-autostart.desktop "$HOME/.config/autostart/opensuperwhisper.desktop"
echo "Installed to $bin. Restart a running daemon with: osw quit; osw-daemon &"
