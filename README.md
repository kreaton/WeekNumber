# WeekNumber

Always know what week it is. WeekNumber shows the current week number as a small icon in the Windows notification area, and one click opens a calendar with week numbers.

Week numbers follow the ISO 8601 standard used in most of Europe, where weeks start on Monday.

## Install

**[Download the installer](https://github.com/kreaton/WeekNumber/releases/latest/download/WeekNumber-win-Setup.exe)** and run it.

The installer isn't code-signed, so Windows may warn you about it. To continue, choose **More info → Run anyway**.

Windows may hide new tray icons behind the **^** arrow on the taskbar. Drag the icon out onto the taskbar to keep it in view.

WeekNumber doesn't start with Windows by itself. To make it do so, press Win+R, type `shell:startup`, and put a shortcut to WeekNumber in the folder that opens.

To get a newer version later, click the icon and choose **Check for updates**. To remove the app, use **Settings → Apps → Installed apps** in Windows.

## How to use it

- **See the week:** the icon always shows the current week number. Hover over it for the full date.
- **Open the calendar:** left- or right-click the icon. Click the icon again, click somewhere else or press Esc to close it.
- **Change month:** use the arrows in the calendar, the mouse wheel or the keyboard.
- **Go back to today:** click the "Today" line at the bottom of the calendar, or press Home.
- **Quit:** choose **Exit** at the bottom of the calendar.

| Key | Action |
|---|---|
| Left, Up or Page Up | Previous month |
| Right, Down or Page Down | Next month |
| Home | Back to the current month |
| Esc | Close the calendar |

The calendar follows the Windows light or dark setting. The icon updates by itself when the date changes.

---

## For developers

WeekNumber is written in Rust and calls the Windows API directly through [`windows-sys`](https://crates.io/crates/windows-sys). It uses [`chrono`](https://crates.io/crates/chrono) for dates and [Velopack](https://velopack.io) for installs and updates. The comments in `src/main.rs` also explain how a Win32 program works, so it can double as a learning example.

| File | Description |
|---|---|
| `src/main.rs` | Tray icon, calendar popup and drawing |
| `src/update.rs` | Check for updates |
| `build.ps1` | Builds the app |
| `BuildRelease.ps1` | Builds, packs and publishes a release |
| `WeekNumber.wsb` | Opens the release build in Windows Sandbox. Change `HostFolder` to your own `target\release` folder first. |

### Build

You need [Rust](https://rustup.rs) with the MSVC toolchain.

```powershell
.\build.ps1
```

The result is `target\release\weeknumber.exe`. The C runtime is linked statically, so the exe runs without `vcruntime140.dll`. Add `-DynamicCrt` to link it dynamically, or `-DebugBuild` for a debug build.

### Releases

Every push to `main` builds the app on GitHub Actions and publishes a new release, one patch version higher than the last (`.github/workflows/release.yml`). To push without releasing, add `[skip ci]` to the commit message. You can also start a release by hand from the repository's Actions tab, or run `.\BuildRelease.ps1` locally. Add `-LocalOnly` to build without uploading.

The repository must stay public, because **Check for updates** reads its releases without logging in.
