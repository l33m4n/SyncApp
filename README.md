# SyncApp

Automatic, verified backups from your Windows PC to flash drives and external disks.

Pick the folders and files you care about, mark the drives you trust, and SyncApp backs up whatever changed every time one of those drives is plugged in. Every other drive is ignored.

- **Only marked drives**: each backup drive carries a hidden random ID, and that ID is checked against the drive's volume serial number. A random USB stick, or a copied ID file, never receives your files.
- **Only what changed**: unchanged files are recognised from metadata alone. If nothing changed, nothing is written to the drive.
- **No data loss**: every copy is written to a temp file, flushed to the device, read back and checksum-verified (BLAKE3) before it replaces the previous copy. Unplugging mid-backup, a full drive or a crash can never damage an existing backup. Nothing on the drive is ever deleted, and files on the PC are only read.
- **Lightweight**: one ~6 MB `SyncApp.exe` with no runtime. The background service idles at about 10 MB of RAM. The manager window only runs while it's open.

## Using it

1. Run `SyncApp.exe`. The manager window opens and the background service (tray icon) starts.
2. **What to back up**: add folders or individual files. You can add skip patterns such as `*.iso` or `node_modules`.
3. **Backup drives**: plug in a drive and click **Mark as backup drive**.
4. Done. Whenever that drive is plugged in, changed files are copied and a notification tells you when it's safe to unplug.
5. **Settings > Install** copies SyncApp to `%LOCALAPPDATA%\Programs\SyncApp`, adds a Start menu entry and starts it at sign-in.

Other features:

- **Per-drive options**: choose which sources go to which drive, turn automatic backup off, or repeat the backup every N minutes while the drive stays plugged in.
- **Verify integrity**: re-reads every file on the drive and checks it against the checksum recorded at backup time. Corrupted files are re-copied on the next backup.
- **Restore**: copy a backup to its original location or to another folder. Existing files are only replaced if you tick the option.
- **History**: every backup, verify and restore, with any per-file problems.
- **Pause**: from the tray menu or Settings.

### Where things live

| What | Where |
| --- | --- |
| Settings | `%APPDATA%\SyncApp\config.json` |
| Log and history | `%LOCALAPPDATA%\SyncApp\` |
| On each backup drive | `X:\SyncApp Backup\<source name>\...` (your files, in normal folders)<br>`X:\.syncapp\` (hidden: drive ID and manifest) |

Backups are plain files, so you can open them on any computer without SyncApp.

### Encryption

Use **BitLocker To Go** on the drive (right-click it in Explorer > *Turn on BitLocker*). SyncApp works unchanged on encrypted drives.

## Building

Requires Rust (stable) and the Visual Studio C++ build tools.

```powershell
cargo build --release      # -> target\release\SyncApp.exe
cargo test                 # engine tests
```

If linking fails with `LNK1104: cannot open file 'msvcrt.lib'`, your Visual Studio install only has the OneCore libraries. Either add **"MSVC v14x - C++ x64/x86 build tools"** in the Visual Studio Installer, or run `. .\scripts\dev-env.ps1` in the same PowerShell session before building.

Set `SYNCAPP_HOME` to a folder to keep settings, log and history there instead (handy for testing).

## Layout

```
src/
  engine.rs   change detection, verified atomic copy, verify, restore
  service.rs  drive marking/recognition, locking, history
  config.rs   settings
  win.rs      Win32 helpers (drives, volume serials, mutexes, registry)
  log.rs      log file
  tray.rs     background service: tray icon, plug-in detection, notifications
  gui.rs      manager window (egui)
  main.rs     entry point, install, icon
tests/engine.rs
```
