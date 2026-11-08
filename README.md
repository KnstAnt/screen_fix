# screen_fix

Record a Windows display and its input events for an AI training dataset.
Windows recording requires Windows 10 version 1903 or later and compatible
D3D11 hardware H.264 encoding. The Linux and macOS builds provide the UI shell.

## Build and record

On Windows x64, with the Rust MSVC toolchain and Windows build tools installed:

```powershell
cargo build --release --target x86_64-pc-windows-msvc
.\target\x86_64-pc-windows-msvc\release\screen-fix.exe
```

Select a display and use **Browse…** to choose an existing output directory,
or paste the path. Click **Start recording**, then **Stop recording** to close
the session. The recorder creates a new subdirectory for each session and
displays the saved video path when export completes.

Keyboard and mouse data are recorded locally. Enable **Save clipboard text**
before Start if needed. The option resets when a session starts. Clipboard
text is limited to 128 KiB per block. Delayed clipboard reads may extend Stop.
On failure, the app shows diagnostics and attempts to write a local report.

For the UI shell, build with `x86_64-unknown-linux-gnu` or
`aarch64-apple-darwin` and the corresponding SDK and system libraries.

## Session output

| File | Contents |
|---|---|
| `video.mp4` | Finalized H.264 recording |
| `frames.jsonl` | WGC observation time, admission time, and verified video PTS and duration |
| `events.jsonl` | Raw input and foreground context, plus clipboard changes when enabled |
| `manifest.json` | Completion marker, package index, display geometry, counts, and timeline |
| `export/events.jsonl` | Before, after, and presented frame references, or reasons for missing frames |
| `export/drags.jsonl` | Drag boundaries, input references, and boundary frame links |
| `export/*.png` | Images shared by events that reference the same observation |

Times ending in `_100ns` use 100 ns units. Admission is relative to session
Start. PTS is relative to video start. The manifest relates the two origins
and records Stop. Physical cursor position is observed at read time.
An unfinished session may contain files without a final manifest.

To export a completed version 2 session again on Windows:

```powershell
.\target\x86_64-pc-windows-msvc\release\screen-fix.exe --export "<session-directory>"
```

The command prints the new `export-<id>` directory. Source files and earlier
exports are preserved. A directory ending in `.pending` is unpublished.

## Project documentation

- [Project goal](doc/target.txt)
- [MVP specification and acceptance criteria](doc/SCREEN_FIX_SPEC.md)
- [Architecture, implementation status, and known gaps](doc/SCREEN_FIX_ARCHITECTURE.md)
- [Historical video pipeline review](doc/SCREEN_FIX_VIDEO_STATIC_REVIEW.md)
- [Project working prompt](doc/SCREEN_FIX_PROJECT_CONTEXT.md) and
  [critic prompt](doc/SCREEN_FIX_CRITIC_PROMPT.md)

The current source package has not been built or exercised on Windows.
