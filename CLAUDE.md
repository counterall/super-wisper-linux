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
  - `config.rs` `Config` TOML at `~/.config/opensuperwhisper/config.toml`
    (trigger, paste options, mic, model). Shared with the future UI.
- `crates/osw-cli` — `osw transcribe|bench|models|download` and daemon
  control `osw toggle|start|stop|cancel|reload|quit` (Unix socket at
  `$XDG_RUNTIME_DIR/opensuperwhisper.sock`).
- `crates/osw-daemon` — no GTK; one thread per subsystem, all reporting to the
  controller loop in `main.rs` over one channel:
  - `capture.rs` PipeWire stream asking for 16 kHz mono f32 (adapter
    resamples); connected once, only (de)activated per recording.
  - `input.rs`/`keys.rs` X11 triggers: key grab (combo), XInput2 raw events
    (single modifier; other key while held → `Chord` discards), button grab
    (mouse); Esc grabbed only while busy. XKB detectable auto-repeat.
  - `paste.rs` arboard clipboard + XTest Ctrl+V (Ctrl+Shift+V for terminals),
    held modifiers released around the paste, clipboard restored after 1.5 s
    if unchanged. Clipboard snapshot is taken at stop, during transcription.
  - `overlay.rs` override-redirect ARGB window (no focus, click-through),
    tiny-skia drawing, animated only while visible.
  - `tray.rs` ksni (SNI) menu: toggle, mic picker, settings, quit.
  - `worker.rs` owns the `WhisperEngine`; must be joined before exit
    (CUDA "driver shutting down" abort otherwise).
- Planned: `osw-ui` (GTK settings/history window, separate process).

## Build / test

```sh
cargo build --release && cargo test --release          # Vulkan
CMAKE_CUDA_HOST_COMPILER=/usr/bin/g++-12 CMAKE_CUDA_ARCHITECTURES=61 \
  cargo build --release --no-default-features --features cuda -p osw-cli \
  --target-dir target-cuda                             # CUDA (~6 min clean)
./target-cuda/release/osw bench clip.wav -m ggml-large-v3-turbo-q5_0.bin
```

Install + autostart: `scripts/install.sh [--cuda]` (→ `~/.local/bin`,
`~/.config/autostart`).

End-to-end test without touching the user's keyboard/clipboard: run the
daemon with `XDG_CONFIG_HOME=<scratch>` and a config with
`trigger.type="none"`, `auto_paste=false`, `microphone="osw_vmic"`; create a
virtual mic with `pw-loopback --capture-props='media.class=Audio/Sink
node.name=osw_in audio.position=[MONO]' --playback-props='media.class=Audio/Source
node.name=osw_vmic audio.position=[MONO]'`, drive with `osw start`,
`pw-play --target osw_in clip.wav`, `osw stop`, read the log. Never send
XTest keystrokes into the user's session while they are using it.

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
- Benchmarks are only comparable on an idle GPU: with another GPU app running
  (~30% load) Turbo q5_0 measured ~720 ms instead of ~540 ms.
- Daemon end-to-end (CUDA): mic start latency 1–7 ms after the request;
  silence-only recording discarded in ~11 ms.
- VAD costs ~26 ms per 5 s of audio → run it incrementally while recording.
- Rejected: shrinking `audio_ctx` to the audio length (Turbo loops/hallucinates
  on short clips); `GGML_VK_*` toggles (no effect).

## Known gaps vs. the Mac app

- Clipboard restore is text-only (arboard without image support): an image
  in the clipboard is not restored after a paste.
- Indicator sits near the mouse pointer, not the text caret.
- No start sound, no history/UI yet (phase 2).
- whisper-rs has no setter for `carry_initial_prompt`, so long (>30 s)
  recordings with a custom prompt don't re-send it per window.

## Roadmap

1. Phase 1 (core loop): DONE in code — daemon, triggers, paste, overlay,
   tray, config, IPC. Pending: user testing of hotkeys/paste/overlay in a
   real session; run VAD incrementally during recording (~30 ms).
2. Phase 2: GTK history (SQLite FTS5, playback, re-transcribe, retention),
   drag-and-drop queue, onboarding, model downloads, streaming transcription.
3. Phase 3: Wayland (portal shortcuts, libei/wtype, layer-shell), vocabulary
   boosting, packaging (.deb, AppImage/Flatpak).
