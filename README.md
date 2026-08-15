# ReelForge Capture

Experimental **host-ffmpeg** screen capture that writes a crash-safe session store and a headless `CaptureProject` JSON for [ReelForge](https://github.com/sergii-ziborov/ReelForge).

This is **not** a finished cross-platform recorder. Windows `gdigrab` + `dshow` is the supported grab path. macOS `avfoundation` and Linux `x11grab` exist as command builders only (no Portal/PipeWire video, no native window picker, no permission UX). Click-zoom is a **marker**, not a rendered crop.

```text
display / window / region     Windows gdigrab (macOS/Linux: experimental argv)
        + system + mic as separate AAC streams
        + pointer log (Windows collector; other hosts: none)
        ↓
SessionSupervisor             owns ffmpeg, PID, stderr, session clock
        ↓
segmented session store       WAL begin/commit of closed files (ffprobe duration)
        ↓
post-capture analyze          demux audio legs to their own files;
                              measure frame difference + audio energy
        ↓
headless CaptureProject v1    trim / remove / speed / idle / zoom markers
        ↓
ReelForge compile + encode
```

## Product split

| Product | Repo | Owns |
| --- | --- | --- |
| **Capture (this repo)** | [ReelForge-Capture](https://github.com/sergii-ziborov/ReelForge-Capture) | Host-ffmpeg grab, session supervisor, store, non-destructive edit, headless `CaptureProject` |
| **ReelForge** | [ReelForge](https://github.com/sergii-ziborov/ReelForge) | Deterministic render engine (`compile_project` → `RenderGraph` → encode) |
| **Intelligence** | [ReelForge-Intelligence](https://github.com/sergii-ziborov/ReelForge-Intelligence) | Semantic edit plans (Capture does not re-implement queries) |
| **SightLoom** | [SightLoom](https://github.com/sergii-ziborov/SightLoom) | Vision index, tracks, masks |

Capture never queries subjects and never embeds a SightLoom crate.

## What v0.1 does

- **Supervisor:** `start --run` owns the ffmpeg child (PID file, Ctrl+C / `--for-secs`, pause/resume API, WAL harvest of closed `000001.mkv`…). Fire-and-forget spawn is gone.
- **Video:** full desktop, window title / id, or pixel region via the **host ffmpeg** grabber.
- **Audio (capture):** system and/or microphone as **separate mapped streams** (`aresample=async=1`, AAC 48 kHz). Not `amix`. Default stream-selection would keep only one input.
- **Audio (project):** each leg is **demuxed into its own file** after capture (`audio/<leg>/000001.m4a`, `-c copy`, recorded in `audio.json`). A project clip points at a single-stream file, so nothing has to guess a stream index. Drift against the video segment is kept as a first-class `gap`, not rounded away.
- **Pointer:** Windows `GetCursorPos` / button edges → `events.jsonl`. Other hosts have no collector yet.
- **Store:** `sessions/<id>/` with `manifest.json`, `wal.jsonl`, closed `segments/`, append-only events. Unfinished tail is dropped on recover.
- **Clocks:** monotonic session clock; commit uses `ffprobe` duration when available; `FrameGap` / `AudioGap` / `DiskFull` / `DeviceLost` events when drift or host failure is visible. This is not a full A/V sync controller.
- **Edit:** trim / remove / speed (speed **splits** partial overlaps).
- **Idle:** agreement between **pointer**, **frame difference** (`signalstats` `YDIF`), and **audio energy** (`astats` RMS dBFS). A range is idle only where every measured source says so; a source that was not measured neither votes nor vetoes, and with no evidence at all `--remove` is refused. Idle can now be found on a session with no pointer log at all.
- **Project:** every committed segment is a media entry; kept ranges that span segments become multiple clips; audio legs become tracks over their demuxed files. Click-zoom is a marker. The document is built from typed structs and `validate()`d (no dangling media ids) before it is written.
- **Schema:** `reelforge-capture-schema` is the single typed definition of `CaptureProject` v1 plus a golden document — see [CaptureProject contract](#captureproject-contract).

## What v0.1 does not do

- Wayland Portal / PipeWire video, macOS arbitrary-window capture, DPI/resize lifecycle, native pickers
- Auto-restart ffmpeg, IPC stop from another process
- Keyboard / active-window idle signals, waveform sidecars
- Render click-zoom as crop/scale/easing
- Depend on the ReelForge crate graph (the schema crate is a checked mirror, not a path-dep)

## Requirements

- Rust **1.97+** (`rust-toolchain.toml`)
- Host **ffmpeg** / **ffprobe** on `PATH` for live grab and duration probe
- **Windows (supported grab):** `gdigrab` + `dshow` / `wasapi`
- **macOS (experimental argv):** `avfoundation` (screen index `REELFORGE_CAPTURE_SCREEN`, default `1`; region = crop; window = device name, not a window-title API). System loopback needs a virtual device. Screen Recording permission required.
- **Linux (experimental argv):** `x11grab` + Pulse. `DISPLAY` defaults to `:0.0`. Prefer an X window id (`0x…`). A non-hex title is **not** a real `x11grab` window selector. Wayland video is not implemented.

## Build

```bash
cargo test --workspace
cargo run -p reelforge-capture-cli -- --help
```

Tests that need a host ffmpeg are ignored by default. They build synthetic
segments with `lavfi` (no screen is recorded) and drive the real demux /
measure / project path:

```bash
cargo test --workspace -- --ignored
```

## CLI

```bash
# list grab targets (ffmpeg device probe + window titles)
reelforge-capture devices

# print the planned ffmpeg command (no supervisor)
reelforge-capture start --dir sessions --screen --mic --system-audio --segment-secs 5

# own the grab: commit closed segments, sample pointer, stop on Ctrl+C
reelforge-capture start --dir sessions --screen --run
reelforge-capture start --dir sessions --screen --run --for-secs 15

# non-destructive edits
reelforge-capture edit sessions/<id> trim --start 2 --end 20
reelforge-capture edit sessions/<id> remove --start 8 --end 9.5
reelforge-capture edit sessions/<id> speed --start 12 --end 16 --factor 2
reelforge-capture zoom-clicks sessions/<id> --duration 0.4 --scale 1.8   # markers only

# demux system / mic into their own files (audio/<leg>/… + audio.json)
reelforge-capture audio sessions/<id>

# idle from pointer + frame difference + audio energy (every measured source must agree)
reelforge-capture idle sessions/<id> --threshold 3
reelforge-capture idle sessions/<id> --threshold 3 --motion-above 1.5 --audio-above-db -50
reelforge-capture idle sessions/<id> --threshold 3 --no-motion --no-audio   # pointer only
reelforge-capture idle sessions/<id> --threshold 3 --remove                 # refused without evidence

# CaptureProject v1 JSON for ReelForge (demuxes audio first unless told not to)
reelforge-capture project sessions/<id> -o project.json
reelforge-capture project sessions/<id> -o project.json --no-audio-extract
```

Without demuxed files the audio legs are still described — as **muted** tracks
tagged `audio_resolved=false` — so an unaddressable stream never silently
plays the wrong leg.

Hand that JSON to ReelForge:

```rust
use reelforge::{CaptureProject, compile_project};

let project = CaptureProject::from_json(&std::fs::read_to_string("project.json")?)?;
let compiled = compile_project(&project)?;
```

## Session layout

```text
sessions/<id>/
  manifest.json      # sources, committed segment list, closed duration
  wal.jsonl          # uncommitted ops (replay after crash)
  grab.pid           # live ffmpeg pid (removed on stop)
  events.jsonl       # cursor / click / FrameGap / AudioGap / DiskFull / DeviceLost
  edits.json         # trim / remove / speed
  click_zoom.json    # markers, not rendered crop
  audio.json         # demuxed leg → file map, with per-file drift
  project.json       # last emitted CaptureProject
  segments/
    000001.mkv       # ffmpeg -segment_start_number 1
    000002.mkv
  audio/
    system/000001.m4a       # one stream per file — what project clips reference
    microphone/000001.m4a
```

A crash mid-segment leaves the WAL + closed segments. Recovery drops the unfinished tail.

## CaptureProject contract

`reelforge-capture-schema` holds the typed v1 document and one checked-in
golden file, `tests/golden/capture_project_v1.json`, that exercises every
field and enum variant. Capture no longer hand-builds JSON maps that merely
*look* like `reelforge_project::CaptureProject`.

Changing the format:

```bash
REELFORGE_BLESS=1 cargo test -p reelforge-capture-schema
```

then copy the golden file into ReelForge, assert `CaptureProject::from_json`
still accepts it unchanged, and bump `CAPTURE_PROJECT_VERSION` on both sides
in the same release. Today's golden document round-trips through the real
`reelforge-project` crate byte-identically.

## Crates

| Crate | Role |
| --- | --- |
| `reelforge-capture-core` | Time, sources, pointer + clock events, signal series, session spec |
| `reelforge-capture-store` | Segmented layout, WAL, event log, `audio.json` |
| `reelforge-capture-platform` | ffmpeg argv, ffprobe, signal measurement, audio demux, Windows pointer, disk free |
| `reelforge-capture-runtime` | Live session supervisor |
| `reelforge-capture-analyze` | Post-capture audio demux and signal measurement |
| `reelforge-capture-edit` | Range EDL, multi-signal idle, click-zoom markers |
| `reelforge-capture-schema` | CaptureProject v1 wire contract + golden document |
| `reelforge-capture-project` | Session → CaptureProject authoring |
| `reelforge-capture-cli` | `reelforge-capture` binary |

## License

MIT © Sergii Ziborov
