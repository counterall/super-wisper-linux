# OpenSuperWhisper for Linux — project notes

Keep this file up to date: whenever a decision, measurement, layout or
workflow changes, update the relevant section in the same change.

## Goal

Port the macOS dictation app at `../OpenSuperWhisper` (Swift) to Linux.
**Top requirement: super fast** (hotkey → recording, stop → pasted text).
Every engine/audio change should be checked with `osw bench`.

## Decisions

- **Whisper only** (whisper.cpp via `whisper-rs` 0.16). No Parakeet/ONNX — the
  user explicitly dropped it; don't add multi-engine abstractions.
- Rust workspace, single native binaries. UI: GTK4 + libadwaita (planned).
- **X11 first** (user runs Cinnamon on X11); Wayland is phase 3.
- Default model: **Turbo V3 q5_0**; fallback **Small q8_0**
  (`models::DEFAULT_PREFERENCE`).
- GPU: Vulkan is the default cargo feature (portable); **CUDA is ~3x faster on
  the dev GPU** and is what we use for perf work.
- Flash attention on only with GPU (it is ~30% slower on CPU).

## Layout

- `crates/osw-core` — no UI code.
  - `audio.rs` file decode (symphonia) → mono → 16 kHz (rubato FFT).
  - `engine.rs` `WhisperEngine`: load, `warm_up`, `prepare` (pre-create
    state), `transcribe` (VAD gate → fresh `WhisperState` per recording).
    Abort/progress use raw C callbacks pointing at stack data (the
    whisper-rs `*_safe` setters leak, and the abort one is unsound).
  - `vad.rs` speech-only stitching (port of Mac `speechOnlySamples`).
  - `text.rs` segment join + cleanup + Asian autocorrect (zh/ja/ko).
  - `models.rs` catalog/defaults; `paths.rs` XDG dirs, embedded Silero VAD.
  - `settings.rs` `TranscribeSettings` (defaults match the Mac app).
- `crates/osw-cli` — `osw transcribe|bench|models`.
- Planned: `osw-daemon` (PipeWire capture, X11 hotkeys, XTest paste, tray,
  overlay), `osw-ui` (GTK settings/history).

## Build / test

```sh
cargo build --release && cargo test --release          # Vulkan
CMAKE_CUDA_HOST_COMPILER=/usr/bin/g++-12 CMAKE_CUDA_ARCHITECTURES=61 \
  cargo build --release --no-default-features --features cuda -p osw-cli \
  --target-dir target-cuda                             # CUDA (~6 min clean)
./target-cuda/release/osw bench clip.wav -m ggml-large-v3-turbo-q5_0.bin
```

Models: `~/.local/share/opensuperwhisper/models`. Test audio:
`../OpenSuperWhisper/jfk.wav` (11 s); a 5 s cut is useful for dictation-like
numbers.

## Dev machine

Mint 22.3, Cinnamon/X11, PipeWire, i5-6600K (4c, AVX2), GTX 1070 8 GB
(Pascal sm_61, driver 580: no fast FP16/int-dot under Vulkan), CUDA 12.0 from
apt (needs gcc-12 host compiler).

## Measurements (GTX 1070, 5 s clip, stop → text, median)

| Model | Vulkan | CUDA |
|---|---|---|
| Turbo V3 q5_0 | 1757 ms | 541 ms |
| Medium q5_0 | 1203 ms | 437 ms |
| Small q8_0 | 412 ms | 198 ms |

- Turbo is encoder-bound (fixed 30 s window): 11 s clip ≈ same cost.
- VAD costs ~26 ms per 5 s of audio → run it incrementally while recording.
- Rejected: shrinking `audio_ctx` to the audio length (Turbo loops/hallucinates
  on short clips); `GGML_VK_*` toggles (no effect).

## Known gaps vs. the Mac app

- whisper-rs has no setter for `carry_initial_prompt`, so long (>30 s)
  recordings with a custom prompt don't re-send it per window.

## Roadmap

1. Phase 1 (core loop): daemon with PipeWire capture into memory, X11
   hotkeys (combo, single modifier L/R, mouse buttons, hold-to-record),
   Esc cancel, XTest paste + clipboard restore, overlay, tray, settings TOML.
2. Phase 2: GTK history (SQLite FTS5, playback, re-transcribe, retention),
   drag-and-drop queue, onboarding, model downloads, streaming transcription.
3. Phase 3: Wayland (portal shortcuts, libei/wtype, layer-shell), vocabulary
   boosting, packaging (.deb, AppImage/Flatpak).
