//! "Check for updates": asks GitHub for a newer release through Velopack and installs it.
//!
//! The network calls block for a while, so they run on a separate thread to keep the message
//! loop (and with it the tray icon and calendar) responsive. That thread can't reach `APP`,
//! which is thread-local, so it gets the tray window's handle passed in instead.

use std::ptr::null_mut;
use std::sync::atomic::{AtomicBool, Ordering};

use velopack::sources::GithubSource;
use velopack::{UpdateCheck, UpdateManager};
use windows_sys::Win32::Foundation::HWND;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

use crate::wide;

/// Public GitHub repo whose releases BuildRelease.ps1 uploads to.
const REPO_URL: &str = "https://github.com/kreaton/WeekNumber";

/// True while a check is running, so clicking the menu item twice doesn't start a second one.
static RUNNING: AtomicBool = AtomicBool::new(false);

/// Shows a message box above other windows. Returns the id of the button that was clicked.
fn message(text: &str, caption: &str, flags: u32) -> i32 {
    let (text, caption) = (wide(text), wide(caption));
    unsafe { MessageBoxW(null_mut(), text.as_ptr(), caption.as_ptr(), flags | MB_TOPMOST | MB_SETFOREGROUND) }
}

/// Starts checking for updates in the background. `tray` is closed if an update gets installed.
pub fn check(tray: HWND) {
    if RUNNING.swap(true, Ordering::SeqCst) {
        return;
    }
    // A raw window handle may not cross threads as-is, but its numeric value may. Using it from
    // another thread is fine because `PostMessageW` is safe to call from any thread.
    let tray = tray as usize;
    std::thread::spawn(move || {
        if let Err(e) = run(tray as HWND) {
            message(&format!("Update failed: {e}"), "Update Error", MB_OK | MB_ICONERROR);
        }
        RUNNING.store(false, Ordering::SeqCst);
    });
}

fn run(tray: HWND) -> Result<(), velopack::Error> {
    let source = GithubSource::new(REPO_URL, None, false);
    // Creating the manager fails when the app wasn't installed by Velopack.
    let Ok(manager) = UpdateManager::new(source, None, None) else {
        message("Updates are only available in the installed app.", "Update Check", MB_OK | MB_ICONINFORMATION);
        return Ok(());
    };

    let UpdateCheck::UpdateAvailable(update) = manager.check_for_updates()? else {
        message("No updates available.", "Update Check", MB_OK | MB_ICONINFORMATION);
        return Ok(());
    };

    let question = format!(
        "A new version ({}) is available. Do you want to download and install it now?",
        update.TargetFullRelease.Version
    );
    if message(&question, "Update Available", MB_YESNO | MB_ICONQUESTION) != IDYES {
        return Ok(());
    }

    manager.download_updates(&update, None)?;

    // Install and restart once this process has exited. Closing the tray window ends it the
    // normal way, which also removes the icon (see WM_DESTROY in `tray_proc`).
    manager.wait_exit_then_apply_updates(&*update, false, true, Vec::<String>::new())?;
    unsafe { PostMessageW(tray, WM_CLOSE, 0, 0) };
    Ok(())
}
