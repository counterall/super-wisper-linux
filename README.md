# OpenSuperWhisper for Linux

Linux port of [OpenSuperWhisper](https://github.com/Starmel/OpenSuperWhisper):
local voice dictation with Whisper (whisper.cpp), built for low latency.

Status: engine, CLI, desktop daemon (X11 hotkeys, PipeWire capture,
auto-paste, tray, recording indicator) and a GTK window with searchable
history, settings and model downloads.

## Layout

- `crates/osw-core` - audio decoding/resampling, Silero VAD gate, whisper.cpp
  engine, model catalog, paths. No UI code.
- `crates/osw-cli` - `osw` binary: `transcribe`, `bench`, `models`,
  `download`, and daemon control (`toggle`, `start`, `stop`, `cancel`,
  `reload`, `quit`).
- `crates/osw-daemon` - background app: trigger → record → transcribe → paste.
- `crates/osw-ui` - GTK4/libadwaita window: history (search, playback,
  re-transcribe, drop audio files to transcribe) and settings.

## Build

```sh
sudo apt install cmake clang libclang-dev libvulkan-dev glslc \
  libpipewire-0.3-dev libgtk-4-dev libadwaita-1-dev
cargo build --release            # Vulkan GPU backend (default)
```

CUDA is ~3x faster than Vulkan on Pascal GPUs (GTX 10xx), which have no fast
FP16 path. Ubuntu/Mint's CUDA 12.0 needs gcc 12 as host compiler; set the
architecture to your GPU's compute capability (61 = GTX 10xx):

```sh
sudo apt install nvidia-cuda-toolkit gcc-12 g++-12
CMAKE_CUDA_HOST_COMPILER=/usr/bin/g++-12 CMAKE_CUDA_ARCHITECTURES=61 \
  cargo build --release --no-default-features --features cuda
```

Measured on a GTX 1070, 5 s dictation, stop-to-text:

| Model | Vulkan | CUDA |
|---|---|---|
| Turbo V3 q5_0 (default) | 1757 ms | 541 ms |
| Medium q5_0 | 1203 ms | 437 ms |
| Small q8_0 | 412 ms | 198 ms |
| Base q8_0 | 157 ms | - |

## Install

```sh
scripts/install.sh --cuda      # or without --cuda for the Vulkan build
osw download "Turbo V3 small"  # ggml-large-v3-turbo-q5_0.bin
osw-daemon &                   # also starts automatically on next login
```

Open the window from the tray ("Settings…", which also has the History tab)
or the applications menu. Changes in the window apply immediately. Settings are
stored in `~/.config/opensuperwhisper/config.toml`; after editing that file
by hand, run `osw reload`.
Default trigger: Right Ctrl pressed on its own — tap to start/stop, or hold
to record until release (Right Ctrl + another key, e.g. Ctrl+C, is ignored).
Other triggers:

```toml
[trigger]
type = "shortcut"       # a key combination; must not be a desktop shortcut
keys = "Ctrl+Alt+d"

[trigger]
type = "mouse_button"
button = 9              # 8/9 = thumb buttons, 2 = middle

[trigger]
type = "none"           # bind `osw toggle` in the desktop's keyboard settings
```

Esc twice cancels (once with `esc_cancel_without_confirmation = true`).

## Usage

Models live in `~/.local/share/opensuperwhisper/models`; `osw models` lists
the catalog and what is installed. Without `-m`, Turbo V3 q5_0 is used if
installed, then Small q8_0, then any installed model.

```sh
osw transcribe recording.m4a -m ggml-large-v3-turbo-q5_0.bin
osw bench clip.wav -m ggml-base-q8_0.bin -m ggml-small-q8_0.bin --runs 5
```

`--cpu` disables the GPU; `-l auto` enables language detection.
