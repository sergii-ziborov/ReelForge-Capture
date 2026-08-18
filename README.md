# ReelForge Capture

Experimental **host-ffmpeg** screen capture that writes a crash-safe session store and a headless `CaptureProject` JSON for [ReelForge](https://github.com/sergii-ziborov/ReelForge).

This is **not** a finished cross-platform recorder. Windows `gdigrab` + `dshow` is the supported grab path. macOS `avfoundation` and Linux `x11grab` exist as command builders only (no Portal/PipeWire video, no native window picker, no permission UX). Click-zoom is a **marker**, not a rendered crop.

```text
display / window / region     Windows gdigrab (macOS/Linux: experimental argv)
        + system + mic as separate AAC streams
        + pointer log (Windows / macOS / X11; Wayland: none)
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

Capture never queries subjects, never embeds a SightLoom crate, never hosts MCP, and never compiles a privacy graph.

**Why post-capture analysis lives here.** Measuring frame difference and audio
energy can look like render-engine work, but they are different jobs. ReelForge
compiles and encodes a *project*; it never sees a session, a segment, an event
log, or a pointer sample. Idle is an editorial decision about a **session**, so
moving it into ReelForge would drag Capture's whole model — store layout, WAL,
`events.jsonl` — into the render engine. Nor is this vision work: it is
`signalstats` and `astats` over files Capture just wrote, through the host
ffmpeg CLI it already depends on, with no libav link and no SightLoom crate.

The boundary that does hold: the moment a signal becomes **semantic** — who is
speaking, which window is focused, where a scene cuts — it belongs to SightLoom
or Intelligence, and Capture should consume a handle instead of computing it.
And if ReelForge ever publishes a shared media-probe crate, the ffmpeg argv
layer here should move to it. The *decisions* stay in Capture either way.

## What v0.1 does

- **Supervisor:** `start --run` owns the ffmpeg child (PID file, Ctrl+C / `--for-secs`, pause/resume, WAL harvest). Closed segments come from ffmpeg's `closed.list`, not "everything except the newest file". An unexpected ffmpeg death **restarts** the grab (up to 3 times / 30 s) from the next ordinal; another process can `stop` / `pause` / `resume` via `control.json`.
- **Video:** full desktop, window title / id, or pixel region via the **host ffmpeg** grabber.
- **Audio (capture):** system and/or microphone as **separate mapped streams** (`aresample=async=1`, AAC 48 kHz). Not `amix`. Default stream-selection would keep only one input.
- **Audio (project):** each leg is **demuxed into its own file** after capture (`audio/<leg>/000001.m4a`, `-c copy`, recorded in `audio.json`). A project clip points at a single-stream file, so nothing has to guess a stream index. If the file is shorter than its video slot (or a restart leaves a hole), the project inserts a real timeline `gap` so later clips stay aligned — the number is not left as a tag.
- **Waveform:** `waveforms.json` holds min/max peaks per leg on the session clock (8 kHz decode, 50 ms buckets). A UI reads that file instead of running ffmpeg again. Audio media in the project is tagged `waveform=waveforms.json` when the sidecar exists.
- **Pointer / keyboard / window:** poll writes cursor, click edges, key-down, and foreground-window changes to `events.jsonl`. Windows (`GetCursorPos`), macOS (CoreGraphics), Linux (X11 via `dlopen`). Wayland-only sessions have no collector.
- **Store:** `sessions/<id>/` with `manifest.json`, `wal.jsonl`, closed `segments/`, append-only events. Unfinished tail is dropped on recover.
- **Clocks:** monotonic session clock; each commit **slews** it to the **video stream** duration (not the container — that often follows the longer audio). Session clock is the fallback when ffprobe cannot read the picture. `clocks.json` records session / video / every audio leg (`duration` + `start_time`) and the correction. `FrameGap` / `AudioGap` fire when drift exceeds 0.75 s (one `AudioGap` per drifting leg). Audio never owns the timeline. `clocks` CLI backfills missing rows. Project tags clips from that sidecar. This is a PLL against probed media, not an `itsoffset` packet rewrite.
- **Edit:** trim / remove / speed (speed **splits** partial overlaps).
- **Idle:** agreement between **input** (cursor, clicks, keys, foreground window), **frame difference** (`signalstats` `YDIF`), and **audio energy** (`astats` RMS dBFS). A range is idle only where every measured source says so; a source that was not measured neither votes nor vetoes, and with no evidence at all `--remove` is refused. Input stillness **accumulates across samples**.
- **Project:** every committed segment is a media entry with an **absolute URI + duration**; kept ranges that span segments become multiple clips; audio legs become tracks over their demuxed files. Click-zoom **splits the video track** and writes `crop` + `scale_to` (ease-in / hold / ease-out, clamped to the frame). Markers stay as editorial labels. `ReelForge` compile emits `rf.transform.crop` / `rf.transform.scale`.
- **Host ingest:** Host takes those `media[].uri` values as `--video` / `ingest_video`. Capture stays grab + project. Do not glob `sessions/<id>/` — the unfinished tail lives there. `emit-media --session ID` prints the same paths. Capture does **not** compile a privacy graph.
- **Schema:** `reelforge-capture-schema` is the single typed definition of `CaptureProject` v1 plus a golden document — see [CaptureProject contract](#captureproject-contract).

## What v0.1 does not do

- Wayland Portal / PipeWire video, Wayland input collector, macOS arbitrary-window capture, DPI/resize lifecycle, native pickers
- Depend on the ReelForge crate graph (the schema crate is a checked mirror; ReelForge parses the same golden in `reelforge-project` conformance tests)
- Compile a privacy / redaction graph, run Intelligence ops, or host MCP (those belong in Host / Intelligence / SightLoom)

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

# another terminal: talk to the live supervisor
reelforge-capture status sessions/<id>
reelforge-capture pause sessions/<id>
reelforge-capture resume sessions/<id>
reelforge-capture stop sessions/<id>

# non-destructive edits
reelforge-capture edit sessions/<id> trim --start 2 --end 20
reelforge-capture edit sessions/<id> remove --start 8 --end 9.5
reelforge-capture edit sessions/<id> speed --start 12 --end 16 --factor 2
reelforge-capture zoom-clicks sessions/<id> --duration 0.4 --scale 1.8   # markers only

# demux system / mic into their own files (audio/<leg>/… + audio.json)
reelforge-capture audio sessions/<id>

# min/max peaks for a timeline UI (waveforms.json)
reelforge-capture waveform sessions/<id>

# idle from input + frame difference + audio energy (every measured source must agree)
reelforge-capture idle sessions/<id> --threshold 3
reelforge-capture idle sessions/<id> --threshold 3 --motion-above 1.5 --audio-above-db -50
reelforge-capture idle sessions/<id> --threshold 3 --no-motion --no-audio   # pointer only
reelforge-capture idle sessions/<id> --threshold 3 --remove                 # refused without evidence

# CaptureProject v1 JSON for ReelForge (demuxes audio first unless told not to)
reelforge-capture project sessions/<id> -o project.json
reelforge-capture project sessions/<id> -o project.json --no-audio-extract

# committed segment paths Host feeds to ingest_video (do not glob sessions/<id>/)
reelforge-capture emit-media --session <id>
reelforge-capture emit-media --session <id> --json          # uri + duration + role
reelforge-capture emit-media --session <id> --all           # + demuxed audio files

# per-segment session / video / audio clocks (probes missing rows)
reelforge-capture clocks --session <id>
reelforge-capture clocks --session <id> --json
reelforge-capture clocks --session <id> --no-repair
```

Without demuxed files the audio legs are still described — as **muted** tracks
tagged `audio_resolved=false` — so an unaddressable stream never silently
plays the wrong leg.

## Host ingest

Capture writes mp4/mkv segments and a `CaptureProject`. Host consumes that
document. Capture does not encode, does not compile a graph, and does not
detect people.

```text
Capture                          Host
  grab + session store
  project.json  ──────────────►  media[].uri  →  --video / ingest_video
  emit-media --session ID  ───►  same paths, one per line
```

Each `media` entry with `role: "video"` is a **committed** segment:

| Field | Meaning |
| --- | --- |
| `uri` | Absolute filesystem path. Pass it to Host `--video` as-is. |
| `duration` | Session-clock length of that file. Do not re-guess from the directory. |
| `id` | Stable `seg_000001` / `system_000001` handle clips already reference. |

Do **not** list `sessions/<id>/segments/*.mkv` yourself. ffmpeg's in-flight
tail and crash leftovers live in that folder and are not in the project.

`emit-media --session ID` looks up `sessions/<ID>/` (or `--dir`) and prints
the same URIs `project` writes. `--json` is the `media` array Host can parse
without reading the whole timeline.

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
  closed.list        # ffmpeg's list of finalized segments (commit source of truth)
  control.json       # pending stop / pause / resume from another process
  status.json        # live phase / pid / elapsed (rewritten every tick)
  events.jsonl       # cursor / click / FrameGap / AudioGap / DiskFull / DeviceLost
  edits.json         # trim / remove / speed
  click_zoom.json    # markers, not rendered crop
  audio.json         # demuxed leg → file map, with per-file drift
  waveforms.json     # min/max peaks per audio leg (session clock)
  clocks.json        # per-segment session / video / every audio leg + start_time
  project.json       # last emitted CaptureProject
  segments/
    000001.mkv       # ffmpeg -segment_start_number 1
    000002.mkv
  audio/
    system/000001.m4a       # one stream per file — what project clips reference
    microphone/000001.m4a
```

A crash mid-segment leaves the WAL + closed segments. Recovery drops the unfinished tail.

## What analysis costs

Post-capture work re-reads finished files with host ffmpeg, so it is worth
knowing the budget before wiring it into a UI. Measured on Windows 11 /
ffmpeg 9.0 with `cargo test -p reelforge-capture-analyze --release --test
bench -- --ignored --nocapture` (6 × 5 s of 720p30 with two audio legs):

| Step | Wall clock | vs. recording length |
| --- | --- | --- |
| Demux both audio legs (`-c copy`) | 0.68 s | 44× realtime |
| Demux again (files already on disk) | 0.35 s | 86× realtime |
| Measure frame difference | 0.50 s | 60× realtime |
| Measure audio energy (2 legs) | 0.31 s | 98× realtime |
| **Measure both — one pass per segment** | **0.55 s** | **55× realtime** |
| Measure both — a pass per signal (fallback) | 1.03 s | 29× realtime |

Each segment is read **once**: one ffmpeg invocation demuxes every audio leg,
and one more measures the picture and every leg together (each leg is
downmixed to mono and merged, so `astats` reports it under its own channel
key). The per-signal path is still there and is what a segment falls back to
when the combined pass cannot read it — an old ffmpeg build, or a stream the
device never opened — so one broken leg does not cost the others.

So a 10-minute recording costs roughly 11 s to measure, and `project` — which
demuxes but does not measure — about 14 s.

Everything after measurement is cheap. On a synthetic **one-hour** session
(720 segments, 7 200 motion + 14 400 audio samples, 14 400 pointer events):

| Step | Wall clock |
| --- | --- |
| `detect_idle_multi` (4 sources → 48 ranges) | 1 ms |
| `project_from_session` + validate (576 clips, 2 160 media) | 14 ms |
| Serialize / parse the 873 KB document | 1 ms / 2 ms |

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
| `reelforge-capture-store` | Segmented layout, WAL, event log, `audio.json`, `waveforms.json` |
| `reelforge-capture-platform` | ffmpeg argv, ffprobe, signal measurement, audio demux, host pointer (Windows / macOS / X11), disk free |
| `reelforge-capture-runtime` | Live session supervisor |
| `reelforge-capture-analyze` | Post-capture audio demux and signal measurement |
| `reelforge-capture-edit` | Range EDL, multi-signal idle, click-zoom markers |
| `reelforge-capture-schema` | CaptureProject v1 wire contract + golden document |
| `reelforge-capture-project` | Session → CaptureProject authoring |
| `reelforge-capture-cli` | `reelforge-capture` binary |

## License

MIT © Sergii Ziborov
