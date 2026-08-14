# ReelForge Capture

**Screen / window / region** capture on **Windows, macOS, and Linux**, with system + mic audio, cursor and click metadata, crash-safe segments, and a **headless CaptureProject** that compiles into [ReelForge](https://github.com/sergii-ziborov/ReelForge).

This is the capture + edit product — not a desktop Premiere clone with a full GUI, and not a vision stack.

```text
display / window / region     Windows gdigrab · macOS avfoundation · Linux x11grab
        + system audio + microphone     dshow/wasapi · avfoundation · pulse
        + cursor / click log
        ↓
segmented session store   ← crash-safe WAL + closed segments
        ↓
headless CaptureProject   ← trim / remove / speed / click-zoom / idle
        ↓
ReelForge compile + encode
```

## Product split

| Product | Repo | Owns |
| --- | --- | --- |
| **Capture (this repo)** | [ReelForge-Capture](https://github.com/sergii-ziborov/ReelForge-Capture) | Screen/audio grab, pointer events, session store, non-destructive edit, headless `CaptureProject` |
| **ReelForge** | [ReelForge](https://github.com/sergii-ziborov/ReelForge) | Deterministic render engine (`compile_project` → `RenderGraph` → encode) |
| **Intelligence** | [ReelForge-Intelligence](https://github.com/sergii-ziborov/ReelForge-Intelligence) | Semantic edit plans (almost ready — Capture does not re-implement queries) |
| **SightLoom** | [SightLoom](https://github.com/sergii-ziborov/SightLoom) | Vision index, tracks, masks |

Capture never queries subjects and never embeds a SightLoom crate. Semantic blur / “find the person” goes through Intelligence when you want it.

## What v0.1 does

- **Video source:** full desktop, window title / id, or pixel region (host ffmpeg grabber)
- **Audio:** system loopback and/or microphone (host device names)
- **Pointer:** cursor samples + click events (JSONL sidecar)
- **Store:** `sessions/<id>/` with `manifest.json`, `wal.jsonl`, closed `segments/`, append-only `events.jsonl`
- **Headless project:** emit a ReelForge `CaptureProject` JSON (no GUI)
- **Edit:** trim / remove / speed ranges, automatic click zoom, idle-range detection

## Requirements

- Rust **1.97+** (`rust-toolchain.toml`)
- Host **ffmpeg** / **ffprobe** on `PATH` for live grab and encode
- **Windows:** `gdigrab` + `dshow` / `wasapi` (window title, desktop, region)
- **macOS:** `avfoundation` (screen index via `REELFORGE_CAPTURE_SCREEN`, default `1`; region = crop; window = device name). System loopback needs a virtual device (e.g. BlackHole). Screen Recording permission required.
- **Linux:** `x11grab` + Pulse (`pactl` listing). `DISPLAY` defaults to `:0.0`. Window can be a title or an X id (`0x…`). Wayland works when a Pulse/PipeWire monitor exists; grab is X11/`x11grab` unless you pass another backend.

## Build

```bash
cargo test --workspace
cargo run -p reelforge-capture-cli -- --help
```

## CLI

```bash
# list grab targets (ffmpeg device probe + window titles)
reelforge-capture devices

# start a segmented session (crash-safe)
reelforge-capture start --dir sessions --screen --mic --system-audio --segment-secs 5

# apply non-destructive edits
reelforge-capture edit sessions/<id> trim --start 2 --end 20
reelforge-capture edit sessions/<id> remove --start 8 --end 9.5
reelforge-capture edit sessions/<id> speed --start 12 --end 16 --factor 2
reelforge-capture idle sessions/<id> --threshold 3 --remove
reelforge-capture zoom-clicks sessions/<id> --duration 0.4 --scale 1.8

# write CaptureProject JSON for ReelForge
reelforge-capture project sessions/<id> -o project.json
```

Hand that JSON to ReelForge:

```rust
use reelforge::{CaptureProject, compile_project};

let project = CaptureProject::from_json(&std::fs::read_to_string("project.json")?)?;
let compiled = compile_project(&project)?;
// run_render_graph(&compiled.graph, …)
```

Intelligence can sit in front of the same media later (`SemanticEditPlan` → resolved graph). Capture only authors the timeline and the event log.

## Session layout

```text
sessions/<id>/
  manifest.json      # sources, segment list, closed duration
  wal.jsonl          # uncommitted ops (replay after crash)
  events.jsonl       # cursor + click (append-only)
  edits.json         # trim / remove / speed / zoom
  project.json       # last emitted CaptureProject
  segments/
    000001.mkv
    000002.mkv
```

A crash mid-segment leaves the WAL + closed segments. Recovery drops the unfinished tail and keeps committed media.

## Crates

| Crate | Role |
| --- | --- |
| `reelforge-capture-core` | Time, sources, pointer events, session spec |
| `reelforge-capture-store` | Segmented layout, WAL, event log |
| `reelforge-capture-platform` | Host grab args (`gdigrab` / `avfoundation` / `x11grab`), device list |
| `reelforge-capture-edit` | Range EDL, idle, click-zoom |
| `reelforge-capture-project` | Headless CaptureProject JSON |
| `reelforge-capture-cli` | `reelforge-capture` binary |

## License

MIT © Sergii Ziborov
