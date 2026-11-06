# File History Tracker

**Automatic, invisible version history for your Windows folders.**

File History Tracker runs quietly in the system tray and keeps every saved version of every file in the folders you
choose. There is nothing to commit and nothing to remember: you just work as usual. When you need an older version —
an overwritten paragraph, a spreadsheet before a bad edit, a file you deleted last week — you open the app, pick the
file, see what changed, and restore it.

It is like Git or Windows File History, but fully automatic, per save, and focused on ordinary documents.

## What it does

- **Records every save automatically.** Each time a file in a watched folder is saved, a new version is stored. Works
  with any program: Word, Excel, editors, IDEs, image tools.
- **Shows what changed.** For each version you see the date and time, what happened (created, modified, renamed,
  deleted, restored), how many lines were added and removed, and a short preview of the change.
- **Compares any two versions** side by side or as a unified diff. Text, source code, Word (`.docx`), Excel
  (`.xlsx`, `.xlsm`, `.ods`) and PDF files are compared by their text content. Other files are stored byte-exact and
  can always be restored. An external diff tool (for example WinMerge) can be configured.
- **Restores in several ways:**
  - **Restore as…** — save an old version under a new name (default `name (YYYY-MM-DD HH-mm).ext`). If you save it
    inside a watched folder, the new file is tracked too.
  - **Restore in place** — put an old version back. The current content is stored as a version first, so a restore
    can always be undone.
  - **Undelete** — bring back a deleted file.
  - **Restore a whole folder to a point in time** — rebuild the folder as it was at a chosen date and time into another
    folder, using the file names valid at that moment.
- **Follows renames and moves**, including renamed folders, so a file keeps its history under its new name.
- **Keeps storage small** with compression, delta storage and configurable retention (see below).
- **Stays out of the way.** No taskbar entry; closing the window only hides it. Optional start with Windows. Monitoring
  can be paused globally or per folder from the tray or the settings.

## How it works

```
 watched folders ──► file system watcher ──► debounce & stability check ──► ingest worker
                                                                                  │
                     ┌────────────────────────────────────────────────────────────┤
                     ▼                                ▼                           ▼
             content hash (BLAKE3)          zstd full copy or delta        text extraction + diff
                     │                                │                           │
                     └──────────────► SQLite (metadata) + object store ◄─────────┘
                                                      ▲
                       React GUI (history, compare, restore, settings) via Tauri commands
```

1. **Watching.** Each watched folder is monitored with the native Windows file change notifications. Events are
   debounced, and a file is only read once it has stopped changing, so programs that save in several steps (Office,
   editors that write a temporary file and rename it) produce exactly one version.
2. **Filtering.** Temporary and lock files (`~$*`, `*.tmp`, `*.swp`, `.git`, `node_modules`, …) and files above a
   size limit are ignored. You can add your own ignore patterns per folder. Cloud placeholders (OneDrive
   "online-only" files) are skipped so nothing is downloaded.
3. **De-duplication.** Every version is identified by a BLAKE3 hash of its content. Saving a file without changes,
   or identical content in several files, is stored only once.
4. **Delta storage.** A version is stored either as a complete zstd-compressed copy (a *keyframe*) or as a zstd delta
   against the file's keyframe. Any version can therefore be rebuilt from at most one keyframe plus one delta. Each
   delta is verified by decoding it before it is accepted.
5. **Metadata.** Folders, files, versions and settings live in a SQLite database (WAL mode). Each version records its
   time, event, size, hash, the stored object, the previous name after a rename, and change statistics.
6. **Change preview.** For text-like files the text is extracted (UTF-8/UTF-16/Windows-1252, Word, Excel, PDF) and
   a line diff is computed for the history view and the compare window. Office and PDF parsing is size-limited, and
   PDFs are parsed in a separate helper process so a broken file cannot affect the app.
7. **Catching up.** At startup, periodically, and after the watcher reports an overflow, every folder is re-scanned.
   Changes made while the app was closed are recorded; files that disappeared are marked as deleted (but stay
   restorable). If a folder is temporarily unavailable (unplugged drive, offline share) nothing is marked deleted.

### Retention

Configured in **Configuration → Retention**, with these defaults:

| Age of a version | What is kept |
|---|---|
| up to 24 hours | every save |
| up to 7 days | one version per hour |
| up to 30 days | one version per day |
| older | one version per week |
| older than 90 days | deleted |

The newest version of every file and versions you **pin** are never deleted. Retention can be set per folder, a
maximum number of versions per file can be set, and **Preview cleanup** shows what would be removed before anything
is deleted. History of folders you stop watching is kept untouched unless you explicitly enable cleanup for them.

### Data location and privacy

All data stays on your computer, in `%LOCALAPPDATA%\FileHistoryTracker`:

- `history.db` — SQLite metadata (folders, files, versions, settings)
- `objects\xx\<hash>.zst` — compressed file contents and deltas
- `app.log` — log file (rotated at 5 MB)

The app makes no network connections. Before a database upgrade a backup copy of `history.db` is created.

## Using the app

1. Start the app; it appears as an icon in the system tray. Click the icon to open the window.
2. **Configuration → Add folder** and choose a folder to watch. Existing files are recorded as the first version.
3. Work as usual. Every save creates a version.
4. **Restore** tab: pick a folder and a file. The history list shows every version; the right panel shows what changed.
   Tick two versions and press **Compare selected** to compare them, or use the buttons to restore.

Tray menu: open the window, pause/resume monitoring, take a snapshot of all folders, run cleanup, quit.

## Building from source

Requirements: Windows 10/11, [Rust](https://rustup.rs) (stable, MSVC toolchain), [Node.js](https://nodejs.org) 20+,
and the WebView2 runtime (preinstalled on Windows 10/11).

```powershell
npm install
npm run tauri dev      # run in development mode
npm run tauri build    # build the release exe and the NSIS/MSI installers
```

Output:

- `src-tauri\target\release\file-history-tracker.exe`
- `src-tauri\target\release\bundle\nsis\*-setup.exe` — per-user installer, no administrator rights needed
- `src-tauri\target\release\bundle\msi\*.msi`

Builds from source are unsigned, so Windows SmartScreen may show a warning the first time you run them.

### Tests

```powershell
cd src-tauri
cargo test
```

Unit tests cover the delta store, diffing, text extraction, path and pattern handling, and the retention rules.
Integration tests (`tests/pipeline.rs`) cover versioning, de-duplication, deltas, renames and folder moves, deletion
and undelete, restore in place, point-in-time folder restore, the start-up re-scan, cleanup and ignore patterns.

## Project structure

| Path | Purpose |
|---|---|
| `src-tauri/src/watcher.rs` | file system watching and rename pairing |
| `src-tauri/src/ingest.rs` | debounce, stability check, filtering, versioning, re-scan |
| `src-tauri/src/store.rs` | content-addressed object store, zstd keyframes and deltas |
| `src-tauri/src/db.rs` | SQLite schema, migrations and queries |
| `src-tauri/src/extract.rs`, `diff.rs` | text extraction and line diffs |
| `src-tauri/src/restore.rs` | restore as, restore in place, folder point-in-time restore |
| `src-tauri/src/retention.rs` | retention, thinning and cleanup |
| `src-tauri/src/commands.rs` | commands used by the GUI |
| `src/` | React + TypeScript user interface |

## Technology

Rust · Tauri v2 · React · TypeScript · SQLite (rusqlite) · zstd · BLAKE3 · notify · similar

## License

Copyright (C) 2026 Michał Borkowski

This program is free software: you can redistribute it and/or modify it under the terms of the GNU General Public
License as published by the Free Software Foundation, either version 3 of the License, or (at your option) any later
version. See [LICENSE](LICENSE) for the full text.

This program is distributed in the hope that it will be useful, but WITHOUT ANY WARRANTY; without even the implied
warranty of MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.
