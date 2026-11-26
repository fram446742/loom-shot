# loom-shot

> Part of [Loom](https://github.com/fram446742/loom).

Screenshot capture, a display-free annotation editor, capture history, pins,
optional OCR and screen recording for niri. Supports clipboard output, HDR
handling, multiple output formats, multi-output stitching and JSON metadata.
It is its own Cargo workspace with its own lockfile and build.

## Modes and commands

- `ui` — open niri's interactive screenshot UI (the path is used by the UI).
- `screen` — capture the focused output (or several with `--output`).
- `window` — capture a window (`--id`, default focused).
- `annotate` — edit an existing image with a persistent annotation log.
- `history` — list, show, prune or clear capture history.
- `pin` — register a stable copy and open it in an external viewer.
- `ocr` — run tesseract on an existing image.
- `record` — record an output, region or (via a generic backend) a window.
  See [Recording](#recording).

Screenshots go to `$XDG_PICTURES_DIR/Screenshots` (or
`$HOME/Pictures/Screenshots`) as `loom-shot-<epoch>.<ext>`, or `--path`.
`--copy`, annotations, `--metadata`, `--format`, `--hdr`, `--ocr` and
`--output` are rejected with `ui` because that mode saves asynchronously and
cannot be post-processed. `--no-history` is accepted everywhere.

## Recording

`loom-shot record` delegates recording to an existing tool instead of linking
a capture or PipeWire stack. Backend selection is automatic unless overridden
with `--backend`:

| Backend | Capture source | Output | Region | Window | Audio | HDR tone-map | Notes |
| --- | --- | --- | --- | --- | --- | --- | --- |
| `wf-recorder` | wlr-screencopy | `-o NAME` | `-g x,y WxH` | ✗ | `-a` / `--audio=DEV` | ✗ | Preferred when installed. |
| `command` | user template | placeholders | placeholders | placeholders | placeholder | placeholder | Escape hatch for portal recorders. |
| `portal` | xdg-desktop-portal ScreenCast | picker | ✗ | picker | ✗ | ✗ | Negotiates the stream through the portal and encodes it with `gst-launch-1.0` (x264/x265/av1). Source selection happens in the portal dialog. |

`auto` picks `wf-recorder` when it is in `PATH` (or `LOOM_SHOT_WF_RECORDER`
points at it), then `portal` when `gst-launch-1.0` is available, otherwise a
configured command template. If none is usable, the error lists every backend
and why it is unavailable.

### Targets

| Flag | Behaviour |
| --- | --- |
| *(none)* | Record niri's focused output (queried through `FocusedOutput` IPC). |
| `--output=DP-1` | Record a named output. |
| `--region=x,y,w,h` | Record a rectangle in compositor/physical pixels. Passed to wf-recorder as `x,y WxH`. |
| `--window=ID` | Record one window. `wf-recorder` has no per-window wlr-screencopy target, so this requires `--backend=command`. `--window=focused` (or a bare `--window`) asks niri for the focused window. niri's own screenshot/screencast UI window is refused. |

Target flags are mutually exclusive. `--region` needs a positive width and
height. The default target requires `NIRI_SOCKET` (or `--socket`); pass
`--output` to record without niri IPC.

### Codecs, containers and audio

| `--codec` | ffmpeg encoder | mp4 | mkv | webm |
| --- | --- | --- | --- | --- |
| `h264` | `libx264` | ✓ | ✓ | ✗ |
| `hevc` | `libx265` | ✓ | ✓ | ✗ |
| `av1` | `libaom-av1` | ✓ | ✓ | ✓ |
| `auto` | `libx264`, or `libaom-av1` for webm | ✓ | ✓ | ✓ |

`--format=mp4|mkv|webm` selects the container; without it the format comes from
`--path` (`.mp4`, `.mkv`, `.webm`) and defaults to MP4. A path without an
extension gets the format's extension appended. Paths and formats that
disagree are rejected, and WebM with h264/hevc is rejected before starting a
recorder.

`--audio`:

- `none` (default) — no audio.
- `default` — the recorder's default input (`wf-recorder -a`).
- `DEVICE` — a specific source name, e.g. `alsa_input.usb-mic`.
- `sink:SINK` — record a sink monitor; `.monitor` is appended when absent.
- `source:SOURCE` — explicit source; the prefix is stripped.

### HDR

Recording HDR is not the screenshot tone mapper. `--hdr=keep` (default) leaves
capture to the backend. `wf-recorder`/wlr-screencopy has no HDR or tone-map
switch, so `--hdr=tonemap` is rejected for it with a pointer to the command
backend. On HDR-enabled outputs, `keep` records whatever the compositor hands
to wlr-screencopy (often SDR-clipped); use a command backend with an ffmpeg
`zscale`/`tonemap` filter if a real HDR-to-SDR conversion is required.

Command templates receive the requested mode as `{hdr}` and
`LOOM_SHOT_RECORD_HDR` (`keep` or `tonemap`).

### Generic command backend

`--command` (or `LOOM_SHOT_RECORD_COMMAND`) runs a shell command with `sh -c`.
It must contain `{path}`; known placeholders are replaced with shell-quoted
values, and unknown braces (such as `${HOME}`) are left untouched:

```
{path} {output} {region} {geometry} {window} {fps} {codec} {encoder}
{format} {audio} {hdr} {backend}
```

The same values are exported as `LOOM_SHOT_RECORD_PATH`, `_OUTPUT`,
`_REGION`, `_GEOMETRY`, `_WINDOW`, `_FPS`, `_CODEC`, `_ENCODER`, `_FORMAT`,
`_AUDIO`, `_HDR` and `_BACKEND`. This backend is the only one that can record a
window or perform HDR conversion, because the external recorder decides what it
supports.

Example with a portal-capable recorder:

```sh
loom-shot record --backend=command --window=42 \
  --command='my-portal-recorder --window {window} --output {path}'
```

### Lifecycle, state files and stopping

Recordings default to
`$XDG_VIDEOS_DIR/Recordings/loom-shot-<epoch>.<ext>` (then
`$HOME/Videos/Recordings`, then the temp dir). Override with `--path`.

While a recording runs, loom-shot writes
`$XDG_STATE_HOME/loom-shot/recordings/record-<pid>.json` (falling back to
`~/.local/state/loom-shot/recordings/`). The file records the PID, output
path, target, backend, codec/format, start time, command line and log path. It
is the PID file used by `--stop`:

```sh
loom-shot record --output=DP-1 --duration=30 --json
loom-shot record --stop --json
```

- `--duration=SECONDS` stops the recorder gracefully: SIGINT first, then
  SIGTERM and SIGKILL if it does not exit. `--fps=auto` (or `0`) leaves the
  frame rate to the backend (wf-recorder then uses damage-driven frames).
- SIGINT/SIGTERM sent to `loom-shot` are forwarded to the recorder, and the
  child gets `PR_SET_PDEATHSIG` so a killed parent does not normally leave an
  orphan.
- `--stop` finds the newest live state file, signals that PID, waits, and
  removes the state and log. `--stop --dry-run` reports what it would stop
  without signalling. Stale state files (dead PIDs) are pruned automatically.
- A non-zero recorder exit is reported with a mapped reason (compositor
  connection failure, denied/portal permission, missing dependency) plus the
  last stderr lines and the preserved log path. A recorder that exits before
  `--duration`, or exits successfully without writing a non-empty file, is
  also an error.
- `--json` prints the final path, backend, target, codec/encoder, format, fps,
  audio, HDR mode, PID, wall-clock duration, how it stopped (`duration`,
  `signal` or `backend`), the command and `dry_run`.

`--dry-run` prints the resolved recorder command and output path without
starting a process or touching the state directory.

## Output formats

`--format=png|jpeg|webp` selects the file format. Without it, the format is
inferred from `--path` (`.png`, `.jpg`/`.jpeg`, `.webp`), defaulting to PNG.
Generated paths use the matching extension.

- PNG: lossless RGBA, the native format niri writes.
- JPEG: lossy RGB, quality set with `--quality 1-100` (default 90).
- WebP: lossless RGBA.

`--copy` always puts a **PNG** on the clipboard, even when the saved file is
JPEG or WebP. `--annotate` (satty) receives the saved file as-is.

## HDR

### What is detected

loom-shot asks niri for the focused output (`FocusedOutput`, which exposes the
output name and `max_bpc`), then reads that output's `hdr` block from niri's
config:

```
output "eDP-1" {
    max-bpc 10
    hdr mode="on" {
        reference-luminance 203
    }
}
```

Config lookup follows niri's precedence: `$NIRI_CONFIG` (a file or a directory
containing `config.kdl`), `$XDG_CONFIG_HOME/niri/config.kdl`, then
`~/.config/niri/config.kdl`; if no user config exists, `/etc/niri/config.kdl`
is tried. `include`d files are **not** followed.

Two limitations are inherent to niri's current IPC and are not worked around:

- IPC has no live "HDR is active" flag. `hdr mode="on"` means the output is
  always signalled as HDR; `hdr mode="auto"` means HDR engages only while an HDR
  fullscreen surface is shown. loom-shot reports the configured mode, not a
  guaranteed live state.
- `window` mode detects the **focused output**, not necessarily the output the
  requested window is on (niri's window IPC data has no output field).

### What niri captures

niri renders screenshots through its `ScreenCapture` target and explicitly
converts the HDR blend space back to SDR while capturing
(`set_sdr_capture_blend`). Captures written by niri are therefore already
tone-mapped, 8-bit RGB(A) PNGs. loom-shot detects that from the decoded image
and refuses to apply a second tone-map.

### `--hdr` modes

| Mode | Behaviour |
| --- | --- |
| `keep` (default) | Leave niri's output untouched. On current niri this is already SDR. A high-bit-depth capture is preserved if no conversion is needed; converting it to JPEG/WebP clips values to 8 bits. |
| `sdr` | Require SDR output. Accepts niri's 8-bit captures; rejects high-bit-depth captures with a message pointing at `--hdr=tonemap`. |
| `tonemap` | Apply a PQ → sRGB tone map, but only to a genuinely high-bit-depth capture. On current niri's 8-bit captures this fails with an explanation instead of double-mapping. |

The tone mapper decodes the PQ (SMPTE ST 2084) transfer function, scales
absolute light (10 000 cd/m²) to the output's configured SDR reference white
(BT.2408 203 cd/m² by default), converts BT.2020 to BT.709 primaries, applies a
simple Reinhard highlight roll-off per channel, and encodes sRGB. It is a
display-referred still-image conversion, not a colour-managed HDR pipeline.
Only 16-bit integer PQ images are supported; 32-bit float inputs are rejected.

Detection results and the niri-tone-maps fact are included in JSON output and
in `--metadata` sidecars.

## Annotations

`--annotate` without a value keeps the interactive satty workflow
(`satty -f <file>`, override with `LOOM_SHOT_ANNOTATE`).

`--annotate=SPEC` draws directly on the captured image with no GUI, display,
font server, or GPU dependency. It is repeatable and operations apply in
command-line order:

| Spec | Effect |
| --- | --- |
| `rect:x,y,w,h` | 3 px red outline. |
| `text:x,y,STRING` | Built-in 5×7 bitmap font, scaled 3×, red with a drop shadow. |
| `arrow:x1,y1,x2,y2` | 3 px shaft and a spread head. |
| `line:x1,y1,x2,y2` | 3 px line without a head. |
| `ellipse:cx,cy,rx,ry` | 3 px red elliptical outline. |
| `highlight:x,y,w,h` | Translucent yellow marker fill. |
| `blur:x,y,w,h[,RADIUS]` | Box blur inside the rectangle. Radius 1–64, default 8. |
| `pixelate:x,y,w,h[,BLOCK]` | Average-colour blocks. Block 1–256, default 12. |
| `solid-redaction:x,y,w,h[,RRGGBB]` | Opaque filled rectangle, default black. `redact` is an alias. |
| `spotlight:x,y,w,h[,DIM]` | Dim everything outside the rectangle by 1–100%, default 60. |
| `zoom:x,y,w,h[,CORNER[,SCALE]]` | Magnify a region into a corner, with source/destination borders. `CORNER` is `top-left`, `top-right` (default), `bottom-left` or `bottom-right`; `SCALE` is 2–8 (default 2). |
| `crop:x,y,w,h` | Crop the canvas. Later coordinates use the cropped image. |

- Coordinates are physical pixels in the image they apply to.
- Brute-force validation runs before any write: geometry must fit the current
  canvas (text may be clipped at the right/bottom), radii/scale/dim ranges are
  checked, and unknown kinds are rejected.
- Redaction, blur and pixelation are applied to the pixels that are encoded to
  the final file. They are not overlays: the saved PNG/JPEG/WebP contains the
  redacted or blurred data.
- Text lowercases as uppercase; unsupported characters render as `?`.
  Everything after the second comma is the text, so commas and spaces are
  allowed (quote the whole argument).
- `zoom` uses nearest-neighbour scaling and needs its scaled rectangle to fit
  with an 8 px margin. `crop` changes the image size, so metadata and history
  record the resulting dimensions.

Example:

```sh
loom-shot screen --annotate=rect:120,80,640,360 \
  --annotate=arrow:700,500,420,240 --annotate=text:130,60,Step 1 \
  --annotate=blur:900,120,320,80,16 \
  --annotate=solid-redaction:40,700,500,40
```

The built-in pipeline lets niri write a PNG, decodes it, applies every operation
in memory, then rewrites the requested format in place. `satty` can be combined
with built-in annotations: annotations are baked in first, then satty opens the
result.

### Batch scripts: `--annotations FILE`

`--annotations FILE` (repeatable) loads a JSON script. Inline `--annotate` specs
are applied first, then each file in command-line order. Both a bare array and
an object with `ops` are accepted:

```json
{
  "version": 1,
  "ops": [
    "crop:0,0,1280,720",
    {"op": "blur", "x": 40, "y": 60, "w": 320, "h": 80, "radius": 12},
    {"op": "solid-redaction", "x": 0, "y": 700, "w": 1280, "h": 20},
    {"op": "zoom", "x": 900, "y": 40, "w": 300, "h": 200, "corner": "top-right", "scale": 2}
  ]
}
```

Object operations are strict: unknown fields are errors, so a typo cannot
silently change a redaction. Colours accept `#rrggbb`, `rrggbb`, `black`,
`white`, `red`, or a `[r,g,b]` array. Operation names and fields are listed by
`loom-shot annotate --list-ops`.

## Annotation editor

```sh
loom-shot annotate PATH --op blur:100,100,400,200,12 \
  --op solid-redaction:0,520,1280,40
loom-shot annotate PATH --annotations steps.json
loom-shot annotate PATH --undo 2
loom-shot annotate PATH --redo
loom-shot annotate PATH --preview --op crop:0,0,800,600
loom-shot annotate PATH --history
loom-shot annotate PATH --clear-log
loom-shot annotate --list-ops
```

- Operations are validated and rendered in memory before the image is replaced;
  a failing batch writes nothing.
- The persistent log is `PATH.annotations.json`; the untouched pre-edit image is
  `PATH.annotations.baseline`. Every render replays the log from the baseline,
  so undo/redo are exact. Adding an operation after undo discards the redo tail.
- `--preview` writes `PATH.preview.EXT` and changes neither the image nor the
  log. `--history` prints the log as JSON (ops, applied count, pending redo).
- `--clear-log` removes the log and baseline but keeps the current image. Use it
  after redacting when the pre-redaction baseline must not remain. Without a
  baseline, undo is impossible; loom-shot reports that instead of guessing.
- `--undo`/`--redo` cannot be combined with new operations or `--preview`;
  `--history` and `--clear-log` are also exclusive with edits.
- The editor never needs a display, and works the same on a capture from
  `loom-shot screen`.

## Capture history

Every successful capture appends one entry per saved file (including a stitched
image) to `$XDG_STATE_HOME/loom-shot/history.json`, falling back to
`~/.local/state/loom-shot/history.json`. `--no-history` opts out. History
stores paths, source, timestamp, dimensions, format, HDR detection, annotation
specs and the metadata sidecar path — never pixels.

```sh
loom-shot history                     # list, newest first
loom-shot history list --limit 5 --json
loom-shot history show 1              # 1 = newest
loom-shot history show /path/shot.png
loom-shot history prune --keep 50 --older-than 30 --missing --dry-run
loom-shot history clear
```

- The file is bounded to 200 entries; appending removes the oldest.
- `show` accepts a 1-based newest-first index or an exact path.
- `prune` combines `--keep N`, `--older-than DAYS` and `--missing` (image no
  longer exists). `--dry-run` reports without writing.
- `clear` empties the list. A corrupt history file is an error, not silently
  reset.
- History write failures are reported on stderr but do not discard a saved
  screenshot.

## Pins

```sh
loom-shot pin /path/shot.png --no-open
loom-shot pin /path/shot.png --viewer imv
loom-shot pin list --json
loom-shot pin unpin pin-...
```

A real always-on-top pin needs a Wayland layer-shell surface. niri's IPC has no
request to create one and loom-shot does not ship a layer-shell client to fake
it. `pin` therefore copies the image to a stable path under
`$XDG_STATE_HOME/loom-shot/pins/<id>/`, registers it in
`$XDG_STATE_HOME/loom-shot/pins.json`, and opens that copy with
`$LOOM_SHOT_VIEWER`, then `xdg-open`. The viewer window is managed normally:
it can be covered and is not guaranteed to stay on top. `--no-open` only
registers the stable copy. If the viewer cannot be launched, the new pin is
rolled back and the error points at `--no-open`. `pin unpin` removes the
registry entry and the copy.

## OCR

```sh
loom-shot screen --ocr --ocr-sidecar
loom-shot ocr /path/shot.png --json
loom-shot ocr /path/shot.png --text
loom-shot ocr /path/shot.png --sidecar
```

OCR uses an installed `tesseract`: `$LOOM_SHOT_TESSERACT` when set, otherwise
`tesseract` from `PATH`. It runs `tesseract IMAGE stdout` and returns JSON with
the full text and lines. `--ocr` runs it for each saved capture (and a stitched
image); `--ocr-sidecar` writes `<image>.ocr.json`. A missing or failing
tesseract is a clear error with an install hint — OCR is never silently skipped.
The sidecar contains the same JSON as the `--json` output.

## Multiple outputs and stitching

`--output` is repeatable and accepts comma-separated names; it is valid with
`screen` only, because niri's screenshot IPC targets the active output.

```sh
loom-shot screen --output=DP-1,HDMI-A-1 --path=/tmp/shot-{output}.png
loom-shot screen --output=DP-1 --output=HDMI-A-1 \
  --stitch=horizontal --stitch-path=/tmp/all.png
```

- Captures are taken sequentially. loom-shot focuses each output through niri
  `FocusMonitor`, captures it, then restores the output that was focused before
  the command. If restoration fails, a warning is printed and the last captured
  output stays focused.
- With more than one output, `--path` must contain `{output}`; it is replaced
  with a filesystem-safe form of the output name. Without `--path`, generated
  names include `loom-shot-<epoch>-<output>.<ext>`.
- `--stitch` (default `horizontal`) or `--stitch=vertical` composes captures in
  physical pixels, left-to-right/top-to-bottom, top/left aligned. The canvas is
  opaque black where a capture does not cover it; images are not scaled.
  `--stitch-path` selects the output (default
  `Screenshots/loom-shot-<epoch>-stitched.<ext>`).
- `--copy` and `--annotate` (satty) need `--stitch` when capturing several
  outputs, otherwise it would be ambiguous which image to use.
- Each saved file gets its own metadata sidecar and history entry; the stitched
  image is recorded with source `stitch`.
- `--dry-run` prints the planned `FocusMonitor`/`ScreenshotScreen` actions and
  paths without contacting niri.

### Cursor note

`--pointer` (default) and its aliases `--cursor`/`--no-cursor` set niri's
`show_pointer` flag on the screenshot action. loom-shot never composites a
cursor itself. niri includes the pointer only when it is over the captured
output at capture time, so a multi-output run can contain the pointer on at most
one output. There is no IPC request that reports the cursor position or forces
it into every capture; loom-shot does not claim otherwise.

## Metadata

`--metadata` writes a JSON sidecar next to each image, appending `.json`
(`shot.png` → `shot.png.json`). It records:

- capture mode, window id, output name and `max_bpc`
- timestamp (Unix seconds), tool version and the full command line
- detected HDR config, `niri_tone_maps_capture`, requested `--hdr` mode and the
  reference luminance used
- output path, format, dimensions and source colour type
- applied annotation specs (inline and from `--annotations` files) and whether
  the image was copied
- stitched images use mode `stitch` and the resulting dimensions

OCR has its own `<image>.ocr.json` sidecar; it is not mixed into capture
metadata. Capture history stores the metadata and OCR-relevant paths separately.

## Usage

```sh
export NIRI_SOCKET=...        # normally set inside niri

loom-shot ui
loom-shot screen --copy
loom-shot window --id 42 --no-pointer --annotate
loom-shot screen --format webp --copy --metadata
loom-shot screen --path /tmp/shot.jpg --quality 92 --hdr=sdr
loom-shot screen --path /tmp/shot.png --dry-run
loom-shot screen --output=DP-1,HDMI-A-1 --path=/tmp/shot-{output}.png --stitch
loom-shot screen --annotations redact.json --ocr --ocr-sidecar

loom-shot annotate /tmp/shot.png --op blur:100,100,400,200,12
loom-shot annotate /tmp/shot.png --undo
loom-shot annotate /tmp/shot.png --preview --op crop:0,0,800,600
loom-shot annotate --list-ops
loom-shot history list --limit 10
loom-shot history prune --keep 50 --missing
loom-shot pin /tmp/shot.png --no-open
loom-shot pin list
loom-shot ocr /tmp/shot.png --json --sidecar

loom-shot record --output=DP-1 --duration=10
loom-shot record --region=100,200,1280,720 --fps=30 --codec=hevc --json
loom-shot record --output=DP-1 --format=mkv --audio=sink:speakers
loom-shot record --output=DP-1 --dry-run
loom-shot record --stop
loom-shot record --stop --json --dry-run
```

`--socket PATH` overrides `$NIRI_SOCKET`. `--dry-run` prints the planned niri
action without contacting niri, capturing, or writing metadata; HDR detection is
therefore empty in dry runs. For `record`, `--dry-run` prints the resolved
recorder command instead of starting it.

## Limitations

- The portal backend records monitors and windows only; ScreenCast has no
  region source and carries no audio. It also does not tone-map HDR. The
  stream is encoded by `gst-launch-1.0`, so the GStreamer plugins for the
  selected codec must be installed (`x264enc`, `x265enc` or `av1enc`).
- `wf-recorder` cannot record a single window and has no HDR tone-map option.
  loom-shot rejects those combinations rather than silently changing them.
- HDR recording is compositor-defined. There is no recording-side tone map and
  no HDR metadata validation; on HDR outputs `keep` usually captures clipped
  SDR pixels.
- Container validation only enforces WebM's AV1 requirement. Encoder names
  (`libx264`, `libx265`, `libaom-av1`) must exist in the local ffmpeg/wf-recorder
  build; otherwise the recorder's error is surfaced.
- `--stop` stops the newest live recording. Several concurrent recordings are
  possible, but older ones need to be stopped with a signal to their PID from
  their state file. PID liveness checks cannot detect all PID reuse.
- `--duration` is wall-clock time from spawn to exit. Shutdown latency and the
  20 ms supervision poll make it approximate; encoded frame counts can differ.
- The generic command backend runs through `sh -c`. Placeholders are safely
  quoted, but the rest of the template is the user's responsibility and must
  write to `{path}` itself.
- niri currently renders its screenshot UI as an internal overlay rather than
  an IPC-visible window. The refusal of `--window` targets that look like
  niri's screenshot/screencast UI is therefore defence in depth.
- HDR state is configured-mode detection plus `max_bpc`; niri IPC does not
  expose whether HDR is currently engaged.
- niri always captures SDR, so `--hdr=tonemap` rejects current screenshots. The
  tone mapper is ready for high-bit-depth captures (and is unit-tested), but
  producing such a capture currently requires a niri change outside this app.
- niri config `include` files are not scanned for `output`/`hdr` blocks.
- Coordinates are absolute pixels; there is no scale-relative or percentage
  syntax.
- JPEG has no alpha channel; transparency is dropped. WebP is lossless and may
  produce larger files than PNG for some content.
- `satty` support for JPEG/WebP depends on the satty build's decoders.
- Pins are file-backed stable copies opened in an external viewer, not
  compositor layer surfaces. They are not guaranteed to stay on top and do not
  restore themselves after the viewer exits. A true pin needs a niri IPC
  extension or a layer-shell client, neither of which is faked here.
- Multi-output capture is sequential and changes focus through `FocusMonitor`;
  it restores the previous output on success. It is not an atomic snapshot
  across outputs, and the pointer can only appear on the output it occupies.
- Stitching places captures at physical pixel size with top/left alignment and
  black padding. There is no scaling, grid layout, or overlap control.
- The annotation log keeps the pre-edit baseline for exact undo. After redacting,
  run `loom-shot annotate PATH --clear-log` if that unredacted copy must not
  remain on disk. The saved image itself always contains the redaction.
- OCR depends on tesseract, its installed languages and its default page
  segmentation. loom-shot exposes text only, not word/line bounding boxes.
- `--annotations` JSON is strict about fields to catch typos. Reordering ops
  changes the result when `crop` or `zoom` depend on earlier coordinates.
- History is bounded to 200 entries and stores paths only; pruning `--missing`
  does not delete images.

## Test

```sh
cargo test
cargo fmt --check
cargo clippy --all-targets
```

Tests are display-free, GPU-free and compositor-free. Screenshot tests use
synthetic images for HDR tone mapping, every annotation operation (blur,
pixelate, redaction, crop, zoom, spotlight, line, ellipse, highlight), format
encoding, path generation, metadata, multi-output stitching and CLI parsing.
Annotation-log tests cover baseline creation, deterministic replay, undo/redo,
redo-tail truncation, previews, clearing and malformed logs. History tests cover
bounding, list/show/prune/clear, dry runs and corrupt files. Pin tests use a fake
viewer and verify rollback when it is missing. OCR tests use a fake tesseract to
check JSON, sidecars, missing-tesseract degradation and non-zero exits.
Recording tests put fake `wf-recorder`/recorder scripts on a temporary `PATH`,
and drive state-file start/stop, duration stops, early exits, JSON reports,
backend selection and argument mapping without touching the real session.
