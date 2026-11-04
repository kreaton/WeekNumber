//! WeekNumber — a Windows tray app that shows the current ISO 8601 week number.
//!
//! Left- or right-click the tray icon to open a calendar with week numbers.
//! Navigate months with the arrows, the mouse wheel, or Left/Right/PgUp/PgDn;
//! Home jumps back to today, Esc closes.
//!
//! # How a Win32 program works (the short version)
//!
//! Windows talks to a program by sending *messages* ("the mouse moved", "please repaint",
//! "a timer fired", …) to its *windows*. Every window belongs to a *window class*, and each
//! class has a *window procedure* — a callback function that Windows calls with each message.
//! `main` sets things up and then sits in a *message loop* that pulls messages off the queue
//! and hands them to the right window procedure. Everything the app does happens inside
//! those window procedures.
//!
//! This app has two windows:
//! - an invisible one (`tray_proc`) that owns the tray icon and receives its clicks, and
//! - the calendar popup (`popup_proc`) that we draw ourselves.
//!
//! # About `unsafe`
//!
//! We call the Windows API directly through the `windows-sys` crate. Those are C functions,
//! and the Rust compiler can't check that we use them correctly (valid pointers, correct
//! sizes, …), so every call has to be inside an `unsafe { }` block. `unsafe` doesn't make
//! the code dangerous by itself; it marks the places where *we* promise to uphold the rules.

// Without this line Windows would open a console window next to our app.
#![windows_subsystem = "windows"]

use std::cell::Cell;
use std::ffi::c_void;
use std::mem::{size_of, size_of_val, zeroed};
use std::ptr::{null, null_mut};

use chrono::{Datelike, Days, Local, Months, NaiveDate};
// `*` imports every name in a module. The Win32 API has thousands of constants and
// functions, so listing them one by one would be impractical.
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Graphics::Dwm::*;
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Registry::*;
use windows_sys::Win32::System::SystemInformation::GetTickCount64;
use windows_sys::Win32::System::Threading::CreateMutexW;
use windows_sys::Win32::UI::HiDpi::*;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::*;
use windows_sys::Win32::UI::Shell::*;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

/// Our own message number. Windows sends it to the hidden window whenever something happens
/// to the tray icon (click, mouse move, …). `WM_APP` and above are reserved for apps.
const WM_TRAY: u32 = WM_APP + 1;
/// Sent when the mouse leaves the popup. Not exported by the windows-sys modules we enable.
const WM_MOUSELEAVE: u32 = 0x02A3;
/// Identifies our icon among any others the same window might own (we only have one).
const TRAY_UID: u32 = 1;
const TIMER_ID: usize = 1;
/// How often we check whether the date has changed (milliseconds).
const TIMER_MS: u32 = 30_000;

/// Icon background colour. Windows stores colours as 0x00BBGGRR — note the reversed order.
const ICON_BG: u32 = rgb(0x00, 0x5A, 0xB4);
const ICON_FG: u32 = rgb(0xFF, 0xFF, 0xFF);

/// Packs red, green and blue into a Windows `COLORREF`.
/// `const fn` means it can be evaluated at compile time, so it can be used in `const`s above.
const fn rgb(r: u8, g: u8, b: u8) -> u32 {
    r as u32 | (g as u32) << 8 | (b as u32) << 16
}

/// Converts a Rust string into the format Windows "W" functions expect: UTF-16 code units
/// ending with a 0. Rust strings are UTF-8 and have no terminator, so this is needed whenever
/// we pass text to Windows.
fn wide(s: &str) -> Vec<u16> {
    // `chain(Some(0))` appends a single 0 after the encoded characters.
    s.encode_utf16().chain(Some(0)).collect()
}

/// Today's date in the local time zone.
fn today() -> NaiveDate {
    Local::now().date_naive()
}

fn first_of_month(d: NaiveDate) -> NaiveDate {
    // `with_day` returns an `Option` because e.g. day 31 doesn't exist in every month.
    // Day 1 always exists, so `unwrap()` (which would panic on `None`) is safe here.
    d.with_day(1).unwrap()
}

// ---------------------------------------------------------------------------
// Application state
// ---------------------------------------------------------------------------
//
// Window procedures are plain functions called by Windows, so we can't pass our own data
// into them as arguments. Instead the state lives in a global (`APP`) that they can reach.
//
// Rust is strict about globals that can change, because two threads could modify them at
// once. We avoid that problem with `thread_local!`: each thread gets its own copy, and since
// the whole app runs on one thread, there is effectively one `App`.
//
// Each field is wrapped in a `Cell`. A `Cell` lets us change a value through a shared
// reference (`&App`) by copying values in and out with `get()`/`set()`/`replace()`. That
// fits here because Windows may call back into our window procedure while we're already
// inside it (e.g. `ShowWindow` sends `WM_ACTIVATE` synchronously), and `Cell` never holds
// on to a borrow that such a re-entrant call could conflict with.

struct App {
    /// `HWND` is a "window handle": an opaque ID Windows gives us for each window.
    tray: Cell<HWND>,
    popup: Cell<HWND>,
    /// The icon currently shown in the tray; kept so we can free it when replacing it.
    icon: Cell<HICON>,
    /// The date the tray icon/tooltip was last rendered for.
    day: Cell<NaiveDate>,
    /// First day of the month currently shown in the calendar.
    shown: Cell<NaiveDate>,
    /// Time (ms since boot) when the popup was last hidden. Used to make tray clicks toggle.
    last_hide: Cell<u64>,
    /// Message number Explorer broadcasts when the taskbar is (re)created.
    taskbar_created: Cell<u32>,
    /// The clickable element currently under the mouse in the popup, for hover highlighting.
    hot: Cell<Hit>,
}

thread_local! {
    static APP: App = App {
        tray: Cell::new(null_mut()),
        popup: Cell::new(null_mut()),
        icon: Cell::new(null_mut()),
        day: Cell::new(NaiveDate::MIN),
        shown: Cell::new(first_of_month(today())),
        last_hide: Cell::new(0),
        taskbar_created: Cell::new(0),
        hot: Cell::new(Hit::None),
    };
}

/// Runs `f` with access to the app state and returns whatever `f` returns.
///
/// Usage: `app(|a| a.popup.get())`. The `|a| ...` part is a *closure* (an inline,
/// anonymous function); `a` is the `&App` it receives. `R` is a generic type parameter,
/// so the closure can return any type.
fn app<R>(f: impl FnOnce(&App) -> R) -> R {
    APP.with(f)
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

fn main() {
    unsafe {
        // Tell Windows we handle display scaling ourselves. Without this, Windows would
        // render us at 100% and stretch the result on high-DPI screens, making it blurry.
        SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);

        // Single instance: a named mutex is a system-wide object. If one with this name
        // already exists, another copy of the app is running, so we quit. The `_mutex`
        // variable keeps the handle alive until `main` ends (the leading `_` just tells
        // Rust we never read it).
        let mutex_name = wide("Local\\WeekNumberTray.SingleInstance");
        let _mutex = CreateMutexW(null(), 0, mutex_name.as_ptr());
        if GetLastError() == ERROR_ALREADY_EXISTS {
            return;
        }

        // The handle of our own .exe, which several APIs want.
        let hinst = GetModuleHandleW(null());
        let tray_class = wide("WeekNumberTray");
        let popup_class = wide("WeekNumberCalendar");

        // --- Register the two window classes ---
        // Many Win32 structs must be zero-filled before use and have a `cbSize` field
        // telling Windows which version of the struct we're passing. `zeroed()` creates
        // a struct with every byte set to 0.
        let mut wc: WNDCLASSEXW = zeroed();
        wc.cbSize = size_of::<WNDCLASSEXW>() as u32;
        wc.hInstance = hinst;
        wc.hCursor = LoadCursorW(null_mut(), IDC_ARROW);
        // The window procedure: the function Windows calls with this class's messages.
        wc.lpfnWndProc = Some(tray_proc);
        wc.lpszClassName = tray_class.as_ptr();
        RegisterClassExW(&wc);

        // Reuse the same struct for the popup class, changing only what differs.
        wc.lpfnWndProc = Some(popup_proc);
        wc.style = CS_DROPSHADOW;
        wc.lpszClassName = popup_class.as_ptr();
        RegisterClassExW(&wc);

        // --- Create the windows ---
        // The tray window is never shown; it exists only to receive messages.
        let title = wide("Week Number");
        let tray = CreateWindowExW(
            0,                    // extended style
            tray_class.as_ptr(),  // window class
            title.as_ptr(),       // title
            WS_OVERLAPPED,        // style
            0,                    // x
            0,                    // y
            0,                    // width
            0,                    // height
            null_mut(),           // parent/owner window
            null_mut(),           // menu
            hinst,                // our module
            null(),               // extra creation data
        );
        // The popup: WS_POPUP = no title bar or frame; TOOLWINDOW = no taskbar button;
        // TOPMOST = stays above other windows. Its size is set each time it's shown.
        // Passing `tray` as owner makes Windows destroy the popup along with the tray window.
        let popup = CreateWindowExW(
            WS_EX_TOOLWINDOW | WS_EX_TOPMOST,
            popup_class.as_ptr(),
            title.as_ptr(),
            WS_POPUP,
            0,
            0,
            0,
            0,
            tray,
            null_mut(),
            hinst,
            null(),
        );

        // Ask Windows 11 to round the popup's corners (ignored on Windows 10).
        // Many APIs take "a pointer to some bytes plus their length", so we cast a
        // reference to our value into a raw `*const c_void` (an untyped pointer).
        let corner = DWMWCP_ROUND;
        DwmSetWindowAttribute(
            popup,
            DWMWA_WINDOW_CORNER_PREFERENCE as u32,
            &corner as *const _ as *const c_void,
            size_of_val(&corner) as u32,
        );

        // If Explorer crashes and restarts, all tray icons vanish. Explorer then broadcasts
        // this message so apps can add their icons again.
        let taskbar_created = RegisterWindowMessageW(wide("TaskbarCreated").as_ptr());
        app(|a| {
            a.tray.set(tray);
            a.popup.set(popup);
            a.taskbar_created.set(taskbar_created);
        });

        update_tray_icon(tray, NIM_ADD);
        // Wake us every 30 s (as WM_TIMER messages) to check whether the date changed.
        SetTimer(tray, TIMER_ID, TIMER_MS, None);

        // --- The message loop ---
        // `GetMessageW` waits until a message arrives, then fills in `msg`. It returns 0 once
        // `PostQuitMessage` has been called, which ends the loop and the program.
        // `DispatchMessageW` calls the window procedure that the message belongs to.
        let mut msg: MSG = zeroed();
        while GetMessageW(&mut msg, null_mut(), 0, 0) > 0 {
            TranslateMessage(&msg); // turns key presses into character messages
            DispatchMessageW(&msg);
        }
    }
}

// ---------------------------------------------------------------------------
// Tray icon
// ---------------------------------------------------------------------------

/// Window procedure for the hidden tray window.
///
/// `extern "system"` makes this function use the Windows calling convention so Windows can
/// call it. Every window procedure receives the same four arguments:
/// - `hwnd`: which window the message is for,
/// - `msg`: what happened (a `WM_*` number),
/// - `wp` / `lp` (WPARAM / LPARAM): two extra numbers whose meaning depends on `msg`.
///
/// It returns an `LRESULT`, again with a per-message meaning (usually 0 for "handled").
/// Messages we don't handle must go to `DefWindowProcW`, which does the default thing.
unsafe extern "system" fn tray_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        // `match` is like a switch statement. Each arm is `pattern => value`, and the value
        // of the chosen arm becomes the function's return value.
        match msg {
            // Something happened to the tray icon. `lp` says what: here we only care
            // about a mouse button being released over it.
            WM_TRAY => {
                match lp as u32 {
                    WM_LBUTTONUP | WM_RBUTTONUP => show_popup(),
                    _ => {} // `_` matches anything else: do nothing
                }
                0
            }
            // Timer tick, system clock changed, or the PC woke from sleep: if the date has
            // moved on, redraw the icon and (if it's open) the calendar.
            WM_TIMER | WM_TIMECHANGE | WM_POWERBROADCAST => {
                if app(|a| a.day.get()) != today() {
                    update_tray_icon(hwnd, NIM_MODIFY);
                    InvalidateRect(app(|a| a.popup.get()), null(), 0);
                }
                DefWindowProcW(hwnd, msg, wp, lp)
            }
            // Display or system settings changed: the icon size may differ now, so rebuild it.
            WM_DISPLAYCHANGE | WM_SETTINGCHANGE => {
                update_tray_icon(hwnd, NIM_MODIFY);
                DefWindowProcW(hwnd, msg, wp, lp)
            }
            // The window is being destroyed (the user clicked Exit): clean up and quit.
            WM_DESTROY => {
                KillTimer(hwnd, TIMER_ID);
                let mut nid: NOTIFYICONDATAW = zeroed();
                nid.cbSize = size_of::<NOTIFYICONDATAW>() as u32;
                nid.hWnd = hwnd;
                nid.uID = TRAY_UID;
                Shell_NotifyIconW(NIM_DELETE, &nid);
                // `replace` stores null and gives back the old value, so we can free it.
                let icon = app(|a| a.icon.replace(null_mut()));
                if !icon.is_null() {
                    DestroyIcon(icon);
                }
                // Makes `GetMessageW` in `main` return 0, ending the program.
                PostQuitMessage(0);
                0
            }
            // A *match guard*: `_ if condition` matches only when the condition is true.
            // We need this because `taskbar_created` is only known at run time, and
            // match patterns must be constants.
            _ if msg != 0 && msg == app(|a| a.taskbar_created.get()) => {
                // Explorer restarted: re-add the icon.
                update_tray_icon(hwnd, NIM_ADD);
                0
            }
            _ => DefWindowProcW(hwnd, msg, wp, lp),
        }
    }
}

/// Draws a fresh icon for today's week and adds it to the tray (`NIM_ADD`) or replaces the
/// existing one (`NIM_MODIFY`). Also sets the hover tooltip.
unsafe fn update_tray_icon(hwnd: HWND, action: u32) {
    unsafe {
        let day = today();
        let iso = day.iso_week();
        let icon = make_icon(iso.week());

        // NOTIFYICONDATAW describes a tray icon. `uFlags` tells Windows which of the fields
        // we filled in.
        let mut nid: NOTIFYICONDATAW = zeroed();
        nid.cbSize = size_of::<NOTIFYICONDATAW>() as u32;
        nid.hWnd = hwnd; // window that receives the icon's messages…
        nid.uID = TRAY_UID;
        nid.uFlags = NIF_ICON | NIF_TIP | NIF_MESSAGE;
        nid.uCallbackMessage = WM_TRAY; // …as this message number
        nid.hIcon = icon;
        let tip = format!(
            "Week {} ({})\n{}",
            iso.week(),
            iso.year(),
            day.format("%A %-d %B %Y") // e.g. "Tuesday 13 October 2026"
        );
        // `szTip` is a fixed-size array of 128 UTF-16 units. Copy our text into it,
        // leaving at least one 0 at the end. `zip` pairs up the array slots with the
        // characters and stops when either runs out.
        for (dst, src) in nid.szTip.iter_mut().zip(tip.encode_utf16().take(127)) {
            *dst = src;
        }

        // Windows functions usually return 0 for failure. If modifying fails (the icon
        // somehow disappeared), add it instead.
        if Shell_NotifyIconW(action, &nid) == 0 && action == NIM_MODIFY {
            Shell_NotifyIconW(NIM_ADD, &nid);
        }

        // Remember the new icon and free the previous one. Windows doesn't free icons for
        // us, so forgetting this would leak a little memory every day.
        let old = app(|a| {
            a.day.set(day);
            a.icon.replace(icon)
        });
        if !old.is_null() {
            DestroyIcon(old);
        }
    }
}

/// Renders the week number into a small-icon-sized, rounded-square icon.
///
/// We draw into a 32-bit bitmap in memory, then turn that bitmap into an icon.
unsafe fn make_icon(week: u32) -> HICON {
    unsafe {
        // Size in pixels of a small icon at the current display scaling (16 at 100%,
        // 24 at 150%, …).
        let size = GetSystemMetricsForDpi(SM_CXSMICON, GetDpiForSystem()).max(16);

        // A "device context" (DC) is GDI's drawing surface: we select a bitmap, font, etc.
        // into it and then call drawing functions on it. This one draws into memory.
        let screen = GetDC(null_mut());
        let dc = CreateCompatibleDC(screen);
        ReleaseDC(null_mut(), screen);

        // Create a 32-bit bitmap whose pixels we can also access directly (`bits` points at
        // them). Each pixel is a u32 laid out as 0xAARRGGBB: alpha, red, green, blue.
        let mut bmi: BITMAPINFO = zeroed();
        bmi.bmiHeader.biSize = size_of::<BITMAPINFOHEADER>() as u32;
        bmi.bmiHeader.biWidth = size;
        bmi.bmiHeader.biHeight = -size; // negative = row 0 is the top row
        bmi.bmiHeader.biPlanes = 1;
        bmi.bmiHeader.biBitCount = 32;
        // `as _` converts to whatever type the field has; the compiler works it out.
        bmi.bmiHeader.biCompression = BI_RGB as _;
        let mut bits: *mut c_void = null_mut();
        let color = CreateDIBSection(dc, &bmi, DIB_RGB_COLORS, &mut bits, null_mut(), 0);
        // `SelectObject` returns whatever was selected before; we must put it back later.
        let old_bmp = SelectObject(dc, color);

        // The bitmap starts fully transparent. Paint the blue rounded square.
        let canvas = Canvas { bits: bits as *mut u32, width: size, height: size };
        let s = size as f32;
        canvas.round_rect(0.0, 0.0, s, s, s / 5.0, ICON_BG, true);

        // Pick the largest font (starting at 80% of the icon height) whose text fits
        // the width. Shrinking the whole font keeps the natural spacing between digits.
        let text: Vec<u16> = week.to_string().encode_utf16().collect();
        let mut em = size * 4 / 5;
        let mut font;
        let mut extent: SIZE = zeroed();
        loop {
            font = make_font(em, 0, FW_SEMIBOLD as i32, "Segoe UI", ANTIALIASED_QUALITY as u32);
            let prev = SelectObject(dc, font);
            // Measure how wide/tall the text would be with this font.
            GetTextExtentPoint32W(dc, text.as_ptr(), text.len() as i32, &mut extent);
            if extent.cx <= size - 2 || em <= 6 {
                break; // fits (or we've hit the minimum): keep this font selected
            }
            // Too wide: un-select and delete this font, then try one pixel smaller.
            SelectObject(dc, prev);
            DeleteObject(font);
            em -= 1;
        }

        // Centre the digits. A font's line box includes space above the digits ("internal
        // leading") and below the baseline, so centring the box would put the digits too
        // high. Instead we centre just the part between the internal leading and the baseline.
        let mut tm: TEXTMETRICW = zeroed();
        GetTextMetricsW(dc, &mut tm);
        let digit_h = tm.tmAscent - tm.tmInternalLeading;
        let x = (size - extent.cx) / 2;
        let y = (size - digit_h) / 2 - tm.tmInternalLeading;
        SetBkMode(dc, TRANSPARENT as _); // don't paint a box behind the text
        SetTextColor(dc, ICON_FG);
        TextOutW(dc, x, y, text.as_ptr(), text.len() as i32);
        // GDI may batch drawing commands; make sure they're done before we read the pixels.
        GdiFlush();

        // GDI is older than transparency and sets alpha to 0 on every pixel it draws text
        // on, which would make the digits see-through. All text pixels lie inside the opaque
        // square, so make them opaque again. The softened edge pixels of the square still
        // have their partial alpha and aren't touched.
        for p in canvas.pixels() {
            // `*p` reads the pixel the reference `p` points to. `>> 24` extracts alpha.
            if *p >> 24 == 0 && *p & 0x00FF_FFFF != 0 {
                *p |= 0xFF00_0000;
            }
        }

        // Clean up the drawing surface, putting the original bitmap back first.
        SelectObject(dc, old_bmp);
        DeleteObject(font);
        DeleteDC(dc);

        // An icon is built from a colour bitmap plus a 1-bit mask. Because our colour bitmap
        // has an alpha channel, Windows uses that instead and an empty mask is fine.
        let mask = CreateBitmap(size, size, 1, 1, null());
        let info = ICONINFO {
            fIcon: 1, // 1 = icon (0 would be a mouse cursor)
            xHotspot: 0,
            yHotspot: 0,
            hbmMask: mask,
            hbmColor: color,
        };
        let icon = CreateIconIndirect(&info);
        // The icon has its own copies of the bitmaps, so ours can go.
        DeleteObject(mask);
        DeleteObject(color);
        icon
    }
}

/// Creates a GDI font. `height` is the em size in pixels; `width` 0 means "normal width".
/// Fonts are system resources: whoever calls this must `DeleteObject` the font afterwards.
unsafe fn make_font(height: i32, width: i32, weight: i32, face: &str, quality: u32) -> HFONT {
    unsafe {
        let mut lf: LOGFONTW = zeroed();
        lf.lfHeight = -height; // negative = em size rather than line height
        lf.lfWidth = width;
        lf.lfWeight = weight;
        lf.lfCharSet = DEFAULT_CHARSET as _;
        lf.lfQuality = quality as _;
        // Copy the font name into the fixed 32-slot array, leaving room for the final 0.
        for (dst, src) in lf.lfFaceName.iter_mut().zip(face.encode_utf16().take(31)) {
            *dst = src;
        }
        CreateFontIndirectW(&lf)
    }
}

/// Direct access to the pixels of a 32-bit bitmap (each pixel is 0xAARRGGBB).
///
/// GDI can't draw smooth (anti-aliased) shapes, so for rounded rectangles and circles we
/// compute each pixel's colour ourselves.
struct Canvas {
    /// Raw pointer to the first pixel. Raw pointers (`*mut T`) are like C pointers: Rust
    /// doesn't track what they point to, so using them requires `unsafe`.
    bits: *mut u32,
    width: i32,
    height: i32,
}

/// `impl` blocks attach methods to a type. `&self` is the instance the method is called on.
impl Canvas {
    /// Views the pixels as a normal Rust slice (`&mut [u32]`), so we can index and loop
    /// over them. Unsafe because we promise `bits` really points at width×height pixels.
    unsafe fn pixels(&self) -> &mut [u32] {
        unsafe { std::slice::from_raw_parts_mut(self.bits, (self.width * self.height) as usize) }
    }

    /// Fills an anti-aliased rounded rectangle spanning (x0, y0)–(x1, y1). It's a circle
    /// when the rectangle is square and `radius` is half its side.
    ///
    /// With `set_alpha`, the shape's coverage becomes the pixel's alpha (for icons drawn on
    /// a transparent bitmap); otherwise the colour is blended over the existing pixels.
    ///
    /// How it works: for every pixel we compute its distance to the shape's edge
    /// (negative = inside). Pixels well inside get the full colour, pixels well outside
    /// are skipped, and pixels the edge passes through get a partial blend. That partial
    /// blending is what makes the edge look smooth instead of jagged.
    unsafe fn round_rect(&self, x0: f32, y0: f32, x1: f32, y1: f32, radius: f32, color: u32, set_alpha: bool) {
        unsafe {
            GdiFlush(); // finish any pending GDI drawing before touching the pixels
            let pixels = self.pixels();
            // Split the COLORREF (0x00BBGGRR) into its red, green and blue parts.
            let (sr, sg, sb) = (color & 0xFF, (color >> 8) & 0xFF, (color >> 16) & 0xFF);
            // Centre of the shape, and half its size minus the corner radius.
            let (cx, cy) = ((x0 + x1) / 2.0, (y0 + y1) / 2.0);
            let (hx, hy) = ((x1 - x0) / 2.0 - radius, (y1 - y0) / 2.0 - radius);
            // Visit only the pixels the shape could cover, clamped to the bitmap bounds.
            // `a..b` is a range: a, a+1, …, b-1.
            for y in (y0.floor() as i32).max(0)..(y1.ceil() as i32).min(self.height) {
                for x in (x0.floor() as i32).max(0)..(x1.ceil() as i32).min(self.width) {
                    // Distance from this pixel's centre to the edge of the rounded rectangle
                    // (a standard "signed distance" formula for rounded boxes).
                    let qx = (x as f32 + 0.5 - cx).abs() - hx;
                    let qy = (y as f32 + 0.5 - cy).abs() - hy;
                    let outside = (qx.max(0.0).powi(2) + qy.max(0.0).powi(2)).sqrt();
                    let dist = outside + qx.max(qy).min(0.0) - radius;
                    // Turn distance into coverage: 1 = fully inside, 0 = fully outside,
                    // in between for pixels on the edge.
                    let a = (0.5 - dist).clamp(0.0, 1.0);
                    if a <= 0.0 {
                        continue; // skip to the next pixel
                    }
                    // `&mut` gives us a mutable reference to this one pixel.
                    let p = &mut pixels[(y * self.width + x) as usize];
                    if set_alpha {
                        let alpha = ((a * 255.0).round() as u32).max(*p >> 24);
                        *p = alpha << 24 | sr << 16 | sg << 8 | sb;
                    } else {
                        // A small closure: mix source and destination by coverage `a`.
                        let mix = |s: u32, d: u32| (s as f32 * a + d as f32 * (1.0 - a)).round() as u32;
                        let (dr, dg, db) = ((*p >> 16) & 0xFF, (*p >> 8) & 0xFF, *p & 0xFF);
                        *p = (*p & 0xFF00_0000) | mix(sr, dr) << 16 | mix(sg, dg) << 8 | mix(sb, db);
                    }
                }
            }
        }
    }

    /// Convenience wrapper taking a Windows `RECT` (integer coordinates).
    unsafe fn round_rect_i(&self, r: RECT, radius: f32, color: u32) {
        unsafe {
            self.round_rect(r.left as f32, r.top as f32, r.right as f32, r.bottom as f32, radius, color, false);
        }
    }
}

// ---------------------------------------------------------------------------
// Calendar popup
// ---------------------------------------------------------------------------

/// The colours used to draw the calendar, one set each for light and dark mode.
struct Theme {
    bg: u32,
    fg: u32,
    /// Days outside the shown month.
    dim: u32,
    weekend: u32,
    /// Week-number column.
    week_bg: u32,
    week_fg: u32,
    /// Today's circle and the "Today" link.
    accent: u32,
    accent_fg: u32,
    /// Highlight behind the current week.
    row_bg: u32,
    line: u32,
    hover: u32,
    dark: bool,
}

fn theme() -> Theme {
    // `if` is an expression in Rust: the whole `if/else` produces a value, which is
    // returned because it's the last expression in the function (no `return` needed).
    if is_dark_mode() {
        Theme {
            bg: rgb(0x20, 0x20, 0x20),
            fg: rgb(0xF0, 0xF0, 0xF0),
            dim: rgb(0x6E, 0x6E, 0x6E),
            weekend: rgb(0xB8, 0xB8, 0xB8),
            week_bg: rgb(0x2A, 0x2A, 0x2A),
            week_fg: rgb(0x60, 0xCD, 0xFF),
            accent: rgb(0x60, 0xCD, 0xFF),
            accent_fg: rgb(0x00, 0x00, 0x00),
            row_bg: rgb(0x2B, 0x33, 0x3D),
            line: rgb(0x3A, 0x3A, 0x3A),
            hover: rgb(0x33, 0x33, 0x33),
            dark: true,
        }
    } else {
        Theme {
            bg: rgb(0xFF, 0xFF, 0xFF),
            fg: rgb(0x1A, 0x1A, 0x1A),
            dim: rgb(0xA8, 0xA8, 0xA8),
            weekend: rgb(0x5A, 0x5A, 0x5A),
            week_bg: rgb(0xF2, 0xF5, 0xFA),
            week_fg: rgb(0x00, 0x5A, 0xB4),
            accent: rgb(0x00, 0x67, 0xC0),
            accent_fg: rgb(0xFF, 0xFF, 0xFF),
            row_bg: rgb(0xE8, 0xF0, 0xFB),
            line: rgb(0xE5, 0xE5, 0xE5),
            hover: rgb(0xF0, 0xF0, 0xF0),
            dark: false,
        }
    }
}

/// Reads the "Choose your app mode" setting from the registry. A value of 0 means dark.
fn is_dark_mode() -> bool {
    let key = wide("Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize");
    let value = wide("AppsUseLightTheme");
    let mut data: u32 = 1; // default to light if the value is missing
    let mut len = size_of::<u32>() as u32;
    // `unsafe` can also be an expression; here its value (the return code) goes into `rc`.
    let rc = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            key.as_ptr(),
            value.as_ptr(),
            RRF_RT_REG_DWORD,
            null_mut(),
            // Windows writes the value into `data` through this pointer.
            &mut data as *mut u32 as *mut c_void,
            &mut len,
        )
    };
    rc == ERROR_SUCCESS && data == 0
}

/// Positions and sizes of everything in the popup, in physical pixels.
///
/// All measurements are designed at 100% scaling (96 DPI) and multiplied up for higher
/// scaling, so the popup looks the same size on every screen. Keeping the geometry in one
/// place means drawing (`draw_calendar`) and mouse hit-testing (`hit`) always agree.
struct Layout {
    pad: i32,
    header_h: i32,
    /// Height of the "Wk Mo Tu …" row.
    dow_h: i32,
    row_h: i32,
    /// Height of the "Today: …" line.
    footer_h: i32,
    /// Space around the separator line above Exit.
    sep_h: i32,
    /// Height of the Exit menu item.
    item_h: i32,
    cell_w: i32,
    width: i32,
    height: i32,
    /// The DPI this layout was computed for.
    scale: i32,
}

const ROWS: i32 = 6; // enough weeks for any month
const COLS: i32 = 8; // week number + 7 days

impl Layout {
    /// `Self` is shorthand for the type being implemented (`Layout`).
    fn new(dpi: u32) -> Self {
        // A closure that scales a 96-DPI measurement to the actual DPI.
        let s = |v: i32| v * dpi as i32 / 96;
        // Tuple destructuring: assigns several variables at once.
        let (pad, header_h, dow_h, row_h, footer_h, cell_w) = (s(12), s(36), s(26), s(30), s(34), s(36));
        let (sep_h, item_h, bottom) = (s(9), s(34), s(4));
        // When a variable has the same name as a field, `Layout { pad, … }` is
        // shorthand for `Layout { pad: pad, … }`.
        Layout {
            pad,
            header_h,
            dow_h,
            row_h,
            footer_h,
            sep_h,
            item_h,
            cell_w,
            width: 2 * pad + COLS * cell_w,
            height: pad + header_h + dow_h + ROWS * row_h + footer_h + sep_h + item_h + bottom,
            scale: dpi as i32,
        }
    }
    /// Scales a 96-DPI measurement for this layout.
    fn px(&self, v: i32) -> i32 {
        v * self.scale / 96
    }
    /// The ‹ (previous month) button.
    fn prev(&self) -> RECT {
        rect(self.pad, self.pad, self.pad + self.cell_w, self.pad + self.header_h)
    }
    /// The › (next month) button.
    fn next(&self) -> RECT {
        let r = self.width - self.pad;
        rect(r - self.cell_w, self.pad, r, self.pad + self.header_h)
    }
    /// The "October 2026" title between the arrows.
    fn title(&self) -> RECT {
        rect(self.pad + self.cell_w, self.pad, self.width - self.pad - self.cell_w, self.pad + self.header_h)
    }
    /// Top of the day-of-week row.
    fn dow_top(&self) -> i32 {
        self.pad + self.header_h
    }
    /// Top of the first week row.
    fn grid_top(&self) -> i32 {
        self.dow_top() + self.dow_h
    }
    /// One grid cell. Column 0 is the week number; columns 1–7 are Monday–Sunday.
    fn cell(&self, col: i32, row: i32) -> RECT {
        let x = self.pad + col * self.cell_w;
        let y = self.grid_top() + row * self.row_h;
        rect(x, y, x + self.cell_w, y + self.row_h)
    }
    /// Top of the "Today: …" line, just below the grid.
    fn footer_top(&self) -> i32 {
        self.grid_top() + ROWS * self.row_h
    }
    /// The clickable "Today: …" line.
    fn today_btn(&self) -> RECT {
        let t = self.footer_top() + self.px(2);
        rect(self.pad, t, self.width - self.pad, t + self.footer_h - self.px(4))
    }
    /// Y coordinate of the menu-style separator line above the Exit item.
    fn separator_y(&self) -> i32 {
        self.footer_top() + self.footer_h + self.sep_h / 2
    }
    /// The Exit item; nearly full-width like a menu item.
    fn exit_btn(&self) -> RECT {
        let t = self.footer_top() + self.footer_h + self.sep_h;
        let inset = self.px(4);
        rect(inset, t, self.width - inset, t + self.item_h)
    }
    /// The rounded highlight drawn behind a hovered element.
    ///
    /// Returns an `Option`: `Some(rect)` if there's something to highlight, `None` if not.
    /// Rust has no null for ordinary values; `Option` is how "maybe a value" is expressed.
    fn hover_rect(&self, hit: Hit) -> Option<RECT> {
        // The arrows get a square highlight centred in their cell.
        let square = |r: RECT| {
            let d = (r.right - r.left).min(r.bottom - r.top) - self.px(4);
            let (cx, cy) = ((r.left + r.right) / 2, (r.top + r.bottom) / 2);
            rect(cx - d / 2, cy - d / 2, cx - d / 2 + d, cy - d / 2 + d)
        };
        match hit {
            Hit::Prev => Some(square(self.prev())),
            Hit::Next => Some(square(self.next())),
            Hit::Today => Some(self.today_btn()),
            Hit::Exit => Some(self.exit_btn()),
            Hit::None => None,
        }
    }
    /// Which clickable element (if any) is at client coordinates (x, y)?
    fn hit(&self, x: i32, y: i32) -> Hit {
        let inside = |r: RECT| x >= r.left && x < r.right && y >= r.top && y < r.bottom;
        if inside(self.prev()) {
            Hit::Prev
        } else if inside(self.next()) {
            Hit::Next
        } else if inside(self.exit_btn()) {
            Hit::Exit
        } else if inside(self.today_btn()) {
            Hit::Today
        } else {
            Hit::None
        }
    }
}

/// The clickable parts of the popup.
///
/// An `enum` is a type that is exactly one of a fixed set of variants. The `derive` line
/// asks the compiler to generate the ability to copy values (`Clone, Copy`) and compare
/// them with `==` (`PartialEq`).
#[derive(Clone, Copy, PartialEq)]
enum Hit {
    Prev,
    Next,
    Today,
    Exit,
    None,
}

/// Shorthand for building a Windows `RECT`.
fn rect(left: i32, top: i32, right: i32, bottom: i32) -> RECT {
    RECT { left, top, right, bottom }
}

/// Opens the calendar next to the mouse pointer, or closes it if it's already open.
unsafe fn show_popup() {
    unsafe {
        // A closure can return a tuple to fetch several values at once.
        let (popup, last_hide) = app(|a| (a.popup.get(), a.last_hide.get()));
        if IsWindowVisible(popup) != 0 {
            hide_popup();
            return;
        }
        // When the popup is open and you click the tray icon, the popup first loses focus
        // and hides itself (see WM_ACTIVATE), and *then* the click arrives here. Without
        // this check the click would immediately reopen it. If it was hidden a moment ago,
        // treat the click as "close".
        if GetTickCount64().saturating_sub(last_hide) < 300 {
            return;
        }

        // Always open on the current month.
        app(|a| a.shown.set(first_of_month(today())));

        // Find the monitor the mouse is on: its usable area (excluding the taskbar)
        // and its scaling.
        let mut pt: POINT = zeroed();
        GetCursorPos(&mut pt);
        let mon = MonitorFromPoint(pt, MONITOR_DEFAULTTONEAREST);
        let mut mi: MONITORINFO = zeroed();
        mi.cbSize = size_of::<MONITORINFO>() as u32;
        GetMonitorInfoW(mon, &mut mi);
        let (mut dpi_x, mut dpi_y) = (96u32, 96u32);
        GetDpiForMonitor(mon, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y);

        // Place the popup centred above the pointer, then nudge it so it stays inside the
        // usable area. `min`/`max` clamp the value between the edges.
        let l = Layout::new(dpi_x);
        let wa = mi.rcWork;
        let gap = l.px(8);
        let x = (pt.x - l.width / 2).min(wa.right - l.width - gap).max(wa.left + gap);
        let y = (pt.y - l.height - gap).min(wa.bottom - l.height - gap).max(wa.top + gap);

        // Make the window border match light/dark mode.
        let dark: i32 = theme().dark as i32;
        DwmSetWindowAttribute(
            popup,
            DWMWA_USE_IMMERSIVE_DARK_MODE as u32,
            &dark as *const _ as *const c_void,
            size_of::<i32>() as u32,
        );

        // Move, resize and show it in one call, then give it keyboard focus. Focus matters:
        // it's how we find out when the user clicks elsewhere (we get WM_ACTIVATE).
        SetWindowPos(popup, HWND_TOPMOST, x, y, l.width, l.height, SWP_SHOWWINDOW);
        SetForegroundWindow(popup);
        // "Invalidate" = mark the window as needing a repaint; Windows then sends WM_PAINT.
        InvalidateRect(popup, null(), 0);
    }
}

unsafe fn hide_popup() {
    unsafe {
        let popup = app(|a| {
            a.last_hide.set(GetTickCount64());
            a.hot.set(Hit::None);
            a.popup.get()
        });
        ShowWindow(popup, SW_HIDE);
    }
}

/// Moves the calendar `delta` months forward (positive) or back (negative).
fn shift_month(delta: i32) {
    app(|a| {
        let d = a.shown.get();
        let n = Months::new(delta.unsigned_abs());
        // chrono has separate add/subtract functions. Both return `None` if the result
        // would be out of range, so `if let Some(d)` only updates when we got a date.
        let d = if delta < 0 { d.checked_sub_months(n) } else { d.checked_add_months(n) };
        if let Some(d) = d {
            a.shown.set(d);
        }
    });
}

/// Window procedure for the calendar popup. See `tray_proc` for how these work.
unsafe extern "system" fn popup_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            // Windows asks us to draw the window's contents.
            WM_PAINT => {
                paint(hwnd);
                0
            }
            // Windows wants to clear the background before painting. We paint every pixel
            // ourselves, so skip it (returning 1 = "done"); this avoids flicker.
            WM_ERASEBKGND => 1,
            // The window gained or lost focus. If it lost focus (the user clicked
            // elsewhere), close it, like a menu does.
            WM_ACTIVATE => {
                // `wp & 0xFFFF` takes the low 16 bits, which hold the activation state.
                if (wp & 0xFFFF) as u32 == WA_INACTIVE as u32 && IsWindowVisible(hwnd) != 0 {
                    hide_popup();
                }
                0
            }
            // Left mouse button released: act on whatever was clicked.
            WM_LBUTTONUP => {
                // The mouse position is packed into `lp`: x in the low 16 bits, y in the
                // high 16 bits. `as i16` keeps the sign for negative coordinates.
                let (x, y) = (lp as i16 as i32, (lp >> 16) as i16 as i32);
                match Layout::new(GetDpiForWindow(hwnd)).hit(x, y) {
                    Hit::Prev => shift_month(-1),
                    Hit::Next => shift_month(1),
                    Hit::Today => app(|a| a.shown.set(first_of_month(today()))),
                    Hit::Exit => {
                        // Destroying the tray window triggers its WM_DESTROY handler,
                        // which removes the icon and ends the message loop.
                        DestroyWindow(app(|a| a.tray.get()));
                        return 0;
                    }
                    Hit::None => return 0,
                }
                InvalidateRect(hwnd, null(), 0);
                0
            }
            // Mouse moved: update which element is highlighted, repainting only if it
            // changed (`replace` returns the previous value to compare against).
            WM_MOUSEMOVE => {
                let (x, y) = (lp as i16 as i32, (lp >> 16) as i16 as i32);
                let hit = Layout::new(GetDpiForWindow(hwnd)).hit(x, y);
                if app(|a| a.hot.replace(hit)) != hit {
                    InvalidateRect(hwnd, null(), 0);
                }
                // Windows only tells us the mouse has left the window if we ask for it,
                // and the request has to be renewed each time.
                let mut tme = TRACKMOUSEEVENT {
                    cbSize: size_of::<TRACKMOUSEEVENT>() as u32,
                    dwFlags: TME_LEAVE,
                    hwndTrack: hwnd,
                    dwHoverTime: 0,
                };
                TrackMouseEvent(&mut tme);
                0
            }
            // The mouse left the popup: remove any hover highlight.
            WM_MOUSELEAVE => {
                if app(|a| a.hot.replace(Hit::None)) != Hit::None {
                    InvalidateRect(hwnd, null(), 0);
                }
                0
            }
            // Scroll wheel: the high 16 bits of `wp` hold the scroll amount (positive = up).
            WM_MOUSEWHEEL => {
                let delta = (wp >> 16) as i16;
                shift_month(if delta > 0 { -1 } else { 1 });
                InvalidateRect(hwnd, null(), 0);
                0
            }
            // Key pressed: `wp` holds the virtual-key code.
            WM_KEYDOWN => {
                match wp as u16 {
                    VK_ESCAPE => {
                        hide_popup();
                        return 0;
                    }
                    VK_LEFT | VK_PRIOR | VK_UP => shift_month(-1),
                    VK_RIGHT | VK_NEXT | VK_DOWN => shift_month(1),
                    VK_HOME => app(|a| a.shown.set(first_of_month(today()))),
                    _ => return 0,
                }
                InvalidateRect(hwnd, null(), 0);
                0
            }
            // Windows asks which mouse pointer to show. Use the hand over clickable things.
            WM_SETCURSOR => {
                let mut pt: POINT = zeroed();
                GetCursorPos(&mut pt);
                ScreenToClient(hwnd, &mut pt); // screen coordinates → window coordinates
                if Layout::new(GetDpiForWindow(hwnd)).hit(pt.x, pt.y) != Hit::None {
                    SetCursor(LoadCursorW(null_mut(), IDC_HAND));
                    return 1;
                }
                DefWindowProcW(hwnd, msg, wp, lp)
            }
            // The popup moved to a monitor with different scaling. Its size was already
            // computed for that monitor in `show_popup`, so just repaint.
            WM_DPICHANGED => {
                InvalidateRect(hwnd, null(), 0);
                0
            }
            _ => DefWindowProcW(hwnd, msg, wp, lp),
        }
    }
}

/// Handles WM_PAINT for the popup.
///
/// We draw the whole calendar into an off-screen bitmap first and then copy it to the
/// window in one go ("double buffering"). Drawing straight to the window would show each
/// step as it happens, which flickers.
unsafe fn paint(hwnd: HWND) {
    unsafe {
        // BeginPaint/EndPaint must bracket all painting in WM_PAINT; they give us the
        // window's DC and tell Windows the window is up to date afterwards.
        let mut ps: PAINTSTRUCT = zeroed();
        let hdc = BeginPaint(hwnd, &mut ps);
        let mut rc: RECT = zeroed();
        GetClientRect(hwnd, &mut rc); // window's inner size; left/top are always 0

        // Off-screen bitmap, created the same way as in `make_icon` so that `Canvas` can
        // draw smooth shapes into its pixels.
        let mem = CreateCompatibleDC(hdc);
        let mut bmi: BITMAPINFO = zeroed();
        bmi.bmiHeader.biSize = size_of::<BITMAPINFOHEADER>() as u32;
        bmi.bmiHeader.biWidth = rc.right;
        bmi.bmiHeader.biHeight = -rc.bottom; // top-down
        bmi.bmiHeader.biPlanes = 1;
        bmi.bmiHeader.biBitCount = 32;
        bmi.bmiHeader.biCompression = BI_RGB as _;
        let mut bits: *mut c_void = null_mut();
        let bmp = CreateDIBSection(mem, &bmi, DIB_RGB_COLORS, &mut bits, null_mut(), 0);
        if bmp.is_null() {
            // Couldn't create the bitmap (e.g. zero-sized window): give up on this paint.
            DeleteDC(mem);
            EndPaint(hwnd, &ps);
            return;
        }
        let old = SelectObject(mem, bmp);

        let canvas = Canvas { bits: bits as *mut u32, width: rc.right, height: rc.bottom };
        draw_calendar(mem, &canvas, &rc, &Layout::new(GetDpiForWindow(hwnd)));

        // Copy the finished picture to the window, then clean up.
        BitBlt(hdc, 0, 0, rc.right, rc.bottom, mem, 0, 0, SRCCOPY);
        SelectObject(mem, old);
        DeleteObject(bmp);
        DeleteDC(mem);
        EndPaint(hwnd, &ps);
    }
}

/// Draws the whole calendar into `dc` (whose pixels `canvas` also points at).
///
/// Order matters: backgrounds and shapes first, text on top.
unsafe fn draw_calendar(dc: HDC, canvas: &Canvas, client: &RECT, l: &Layout) {
    unsafe {
        let t = theme();
        let today = today();
        let (shown, hot) = app(|a| (a.shown.get(), a.hot.get()));
        // The grid starts on the Monday on or before the 1st of the month.
        // `Days::new(n)` is a number of days that can be added to or subtracted from a date.
        let start = shown - Days::new(shown.weekday().num_days_from_monday() as u64);
        let radius = l.px(4) as f32;

        // Create the fonts we need. ClearType gives the sharpest text on LCD screens.
        let q = CLEARTYPE_QUALITY as u32;
        let font = make_font(l.px(14), 0, FW_NORMAL as i32, "Segoe UI", q);
        let bold = make_font(l.px(14), 0, FW_SEMIBOLD as i32, "Segoe UI", q);
        let title_font = make_font(l.px(17), 0, FW_SEMIBOLD as i32, "Segoe UI", q);
        let arrow_font = make_font(l.px(22), 0, FW_NORMAL as i32, "Segoe UI", q);
        let small = make_font(l.px(12), 0, FW_NORMAL as i32, "Segoe UI", q);
        let old_font = SelectObject(dc, font);
        SetBkMode(dc, TRANSPARENT as _);

        // --- Backgrounds ---
        fill(dc, client, t.bg);
        // Shaded week-number column.
        canvas.round_rect_i(rect(l.pad, l.dow_top(), l.pad + l.cell_w, l.footer_top()), radius, t.week_bg);
        // Thin line under the day names.
        fill(dc, &rect(l.pad, l.grid_top() - 1, l.width - l.pad, l.grid_top()), t.line);
        // Full-width separator above Exit, like in a Windows menu.
        let sep = l.separator_y();
        fill(dc, &rect(0, sep, l.width, sep + 1), t.line);
        // `if let` runs the block only if `hover_rect` returned `Some`, binding the value to `r`.
        if let Some(r) = l.hover_rect(hot) {
            canvas.round_rect_i(r, radius, t.hover);
        }
        // Highlight the row containing today (if it's visible) and circle today's date.
        for row in 0..ROWS {
            let monday = start + Days::new(row as u64 * 7);
            if today >= monday && today < monday + Days::new(7) {
                let (r0, r1) = (l.cell(1, row), l.cell(7, row));
                canvas.round_rect_i(rect(r0.left, r0.top + 1, r1.right, r1.bottom - 1), radius, t.row_bg);
                // A circle is a rounded square whose corner radius is half its size.
                let c = l.cell(today.weekday().num_days_from_monday() as i32 + 1, row);
                let d = ((c.right - c.left).min(c.bottom - c.top) - l.px(4)) as f32;
                let (cx, cy) = ((c.left + c.right) as f32 / 2.0, (c.top + c.bottom) as f32 / 2.0);
                canvas.round_rect(cx - d / 2.0, cy - d / 2.0, cx + d / 2.0, cy + d / 2.0, d / 2.0, t.accent, false);
            }
        }

        // --- Text ---
        // Header: ‹ Month Year ›   (`\u{2039}` is the Unicode code for ‹)
        SelectObject(dc, arrow_font);
        draw_text(dc, "\u{2039}", l.prev(), t.fg, DT_CENTER);
        draw_text(dc, "\u{203A}", l.next(), t.fg, DT_CENTER);
        SelectObject(dc, title_font);
        draw_text(dc, &shown.format("%B %Y").to_string(), l.title(), t.fg, DT_CENTER);

        // Day-of-week header.
        let dow_y = l.dow_top();
        SelectObject(dc, small);
        let names = ["Wk", "Mo", "Tu", "We", "Th", "Fr", "Sa", "Su"];
        // `enumerate()` yields (index, item) pairs.
        for (i, name) in names.iter().enumerate() {
            let x = l.pad + i as i32 * l.cell_w;
            let color = match i {
                0 => t.week_fg,
                6 | 7 => t.dim, // Saturday, Sunday
                _ => t.weekend,
            };
            draw_text(dc, name, rect(x, dow_y, x + l.cell_w, dow_y + l.dow_h), color, DT_CENTER);
        }

        // Grid: one row per week, starting with its ISO week number.
        for row in 0..ROWS {
            let monday = start + Days::new(row as u64 * 7);
            SelectObject(dc, bold);
            let wk = monday.iso_week().week();
            draw_text(dc, &wk.to_string(), l.cell(0, row), t.week_fg, DT_CENTER);

            for col in 0..7 {
                let day = monday + Days::new(col as u64);
                let r = l.cell(col + 1, row);
                let in_month = day.month() == shown.month();
                let label = day.day().to_string();
                if day == today {
                    // Drawn on top of the circle painted earlier.
                    SelectObject(dc, bold);
                    draw_text(dc, &label, r, t.accent_fg, DT_CENTER);
                } else {
                    SelectObject(dc, font);
                    let color = if !in_month {
                        t.dim
                    } else if col >= 5 {
                        t.weekend
                    } else {
                        t.fg
                    };
                    draw_text(dc, &label, r, color, DT_CENTER);
                }
            }
        }

        // Footer: today line, then the Exit item below the separator.
        SelectObject(dc, font);
        let today_text = format!(
            "Today: {} \u{00B7} Week {}",
            today.format("%a %-d %b %Y"), // e.g. "Tue 13 Oct 2026"
            today.iso_week().week()
        );
        // `RECT` is `Copy`, so `l.today_btn()` hands us our own copy that we can adjust.
        let mut tr = l.today_btn();
        tr.left += l.px(8);
        draw_text(dc, &today_text, tr, t.accent, DT_LEFT);
        let mut er = l.exit_btn();
        er.left = l.pad + l.px(8);
        draw_text(dc, "Exit", er, t.fg, DT_LEFT);

        // Put the original font back before deleting ours (GDI won't delete a font that's
        // still selected). An array of handles can be looped over directly.
        SelectObject(dc, old_font);
        for f in [font, bold, title_font, arrow_font, small] {
            DeleteObject(f);
        }
    }
}

/// Fills a rectangle with a solid colour (no anti-aliasing needed for straight edges).
unsafe fn fill(dc: HDC, r: &RECT, color: u32) {
    unsafe {
        let brush = CreateSolidBrush(color);
        FillRect(dc, r, brush);
        DeleteObject(brush);
    }
}

/// Draws one line of text inside `r` with the currently selected font.
///
/// `mut r` makes our copy of the rectangle changeable, because `DrawTextW` takes a mutable
/// pointer to it.
unsafe fn draw_text(dc: HDC, s: &str, mut r: RECT, color: u32, align: u32) {
    unsafe {
        // DrawTextW takes a length, so the text doesn't need a terminating 0 here.
        let mut w: Vec<u16> = s.encode_utf16().collect();
        SetTextColor(dc, color);
        DrawTextW(
            dc,
            w.as_mut_ptr(),
            w.len() as i32,
            &mut r,
            // Single line, centred vertically; NOPREFIX stops "&" from becoming an underline.
            align | DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX,
        );
    }
}
