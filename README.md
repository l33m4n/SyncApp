# SyncApp

Automatic, verified backups from your Windows PC to flash drives and external disks.

Pick the folders and files you care about, mark the drives you trust, and SyncApp backs up whatever changed every time one of those drives is plugged in. Every other drive is ignored.

- **Only marked drives**: each backup drive carries a hidden random ID, and that ID is checked against the drive's volume serial number. A random USB stick, or a copied ID file, never receives your files.
- **Only what changed**: unchanged files are recognised from metadata alone. If nothing changed, nothing is written to the drive.
- **No data loss**: every copy is written to a temp file, flushed to the device, read back and checksum-verified (BLAKE3) before it replaces the previous copy. Unplugging mid-backup, a full drive or a crash can never damage an existing backup. Files on the PC are only read, and nothing on the drive is deleted unless you choose to in **Clean up**.
- **Several PCs, one drive**: PCs that share a backup drive write to the same folders. For each file the newest version is kept, and an older copy from another PC never replaces a newer one.
- **Tamper-resistant**: SyncApp never writes through junctions or symbolic links on a backup drive, so a modified drive can't redirect writes onto your PC.
- **Lightweight**: one ~6 MB `SyncApp.exe` with no runtime. The background service is event-driven and idles at ~0% CPU, no disk access and about 2 MB of memory, in Windows' efficiency mode. Automatic backups run at background disk priority. The manager window only runs while it's open.

## Using it

1. Run `SyncApp.exe`. The manager window opens and the background service (tray icon) starts.
2. **What to back up**: add folders or individual files. You can add skip patterns such as `*.iso` or `node_modules`.
3. **Backup drives**: plug in a drive and click **Use as backup drive...**.
4. Done. Whenever that drive is plugged in, changed files are copied and a notification tells you when it's safe to unplug.
5. **Settings > Install** copies SyncApp to `%LOCALAPPDATA%\Programs\SyncApp`, adds a Start menu entry and starts it at sign-in. **Settings > Uninstall** removes all of that again (backups on your drives are never touched).

Other features:

- **Per-drive options**: choose which sources go to which drive, turn automatic backup off, and schedule extra backups while the drive stays plugged in (every N minutes/hours, or at set times of day on chosen weekdays). Scheduled runs only notify you when they copied something or hit a problem.
- **Verify integrity**: re-reads every file on the drive and checks it against the checksum recorded at backup time. Corrupted files are re-copied on the next backup.
- **Restore**: browse the backup, search it, and restore selected files or folders to where they came from or to another folder. Each file shows whether it's missing or different on this PC, and which PC backed it up. Existing files are only replaced if you tick the option.
- **Clean up**: lists backed-up files that no PC has any more (deleted, renamed or moved everywhere, or from folders you stopped backing up), with their total size. Review and delete them to free space; SyncApp never deletes them on its own.
- **Updates**: once a day SyncApp asks GitHub whether a newer release exists and notifies you (can be turned off). **Update now** downloads it, checks it against the SHA-256 checksum published with the release, and restarts into the new version; it never updates while a backup is running and never installs anything without your click.
- **Battery saver**: scheduled backups wait while Windows Battery saver is on (optional; plug-in backups still run).
- **History**: every backup, verify, restore and clean-up, with any per-file problems.
- **Pause**: from the tray menu or Settings.

### Where things live

| What | Where |
| --- | --- |
| Settings | `%APPDATA%\SyncApp\config.json` |
| Log and history | `%LOCALAPPDATA%\SyncApp\` |
| On each backup drive | `X:\SyncApp Backup\<source name>\...` (your files, in normal folders)<br>`X:\.syncapp\` (hidden: drive ID and manifest) |

Backups are plain files, so you can open them on any computer without SyncApp.

### Locked and open files

Files that are open in another program are normally still readable and are backed up. A file that's edited while it's being copied is copied again, so the backup is always a complete version. A file that a program keeps **exclusively locked** (for example an Outlook `.pst` while Outlook is running) can't be read: it's reported, its previous backup copy is kept, and it's picked up on the first backup after the program closes it.

SyncApp doesn't use Volume Shadow Copy to read such files while they're locked, because that needs administrator rights, and granting them silently to an app installed in your user profile would let other programs abuse it.

### Encryption

Use **BitLocker To Go** on the drive (right-click it in Explorer > *Turn on BitLocker*). SyncApp works unchanged on encrypted drives.

## Building

Requires Rust (stable) and the Visual Studio C++ build tools.

```powershell
cargo build --release      # -> target\release\SyncApp.exe
cargo test                 # all tests
```

Every push is built and tested on Windows by GitHub Actions (`.github/workflows/ci.yml`), which also keeps `SyncApp.exe` as a build artifact.

### Releasing an update

Just push to `main`. When the build and tests pass, the workflow:

1. picks the next version automatically: the last published version + 1 (0.1.1, 0.1.2, ...), or the `version` in `Cargo.toml` if that is higher;
2. builds `SyncApp.exe` with that version;
3. replaces the single rolling GitHub release tagged `release` (titled "SyncApp X.Y.Z") with the new exe and its `SyncApp.exe.sha256` checksum.

Installed copies of SyncApp see it at their next daily check (or right away with **Settings > Check now**). To start a new minor or major series, set `version` in `Cargo.toml` (e.g. `0.2.0`) and push.

The exe is not code-signed, so Windows SmartScreen may warn the first time someone runs a downloaded copy.

Performance check (backs up a folder twice and prints timings):

```powershell
cargo run --release --example bench -- <source folder> <empty target folder>
```

If linking fails with `LNK1104: cannot open file 'msvcrt.lib'`, your Visual Studio install only has the OneCore libraries. Either add **"MSVC v14x - C++ x64/x86 build tools"** in the Visual Studio Installer, or run `. .\scripts\dev-env.ps1` in the same PowerShell session before building.

Set `SYNCAPP_HOME` to a folder to keep settings, log and history there instead (handy for testing).

## Layout

```
src/
  engine.rs    change detection, verified atomic copy, verify, restore, clean-up, multi-PC manifest
  service.rs   drive marking/recognition, locking, history
  config.rs    settings
  schedule.rs  schedules (intervals, times of day, daylight saving)
  planner.rs   which drives are due for a scheduled backup, and when to wake
  win.rs       Win32 helpers (drives, volume serials, mutexes, registry, power)
  log.rs       log file
  tray.rs      background service: tray icon, plug-in detection, notifications
  gui.rs       manager window (egui)
  main.rs      entry point, install/uninstall, icon
build.rs       embeds the icon and version info in the exe
assets/        app icon (regenerate with scripts/make_icon.py)
tests/         engine, multi-PC/clean-up and restore-path tests
examples/      bench.rs performance check
```

## License

MIT. See [LICENSE](LICENSE).
