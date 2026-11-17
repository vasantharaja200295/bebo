//! The parts of bebo that are different on Windows and on macOS.

use std::path::PathBuf;

use winit::window::Window;

use crate::theme::Rgb;

/// macOS draws the window buttons and resizes the window at its edges. On Windows, bebo does both itself.
pub const NATIVE_FRAME: bool = cfg!(target_os = "macos");

/// Windows can start a shell as administrator. macOS has no such action for a window.
pub const ADMIN: bool = cfg!(windows);

/// The home folder of the user.
pub fn home() -> Option<PathBuf> {
    std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" }).map(PathBuf::from)
}

/// The folder for the settings of each app: `%APPDATA%` or `~/Library/Application Support`.
pub fn app_data() -> Option<PathBuf> {
    if cfg!(windows) {
        std::env::var_os("APPDATA").map(PathBuf::from)
    } else {
        home().map(|h| h.join("Library").join("Application Support"))
    }
}

/// Opens a URL or a file with the app that the system selects. `reveal` shows the file in the file manager and does not open it.
pub fn open(target: &str, reveal: bool) -> bool {
    let mut command = std::process::Command::new(if cfg!(windows) { "explorer.exe" } else { "open" });
    match (reveal, cfg!(windows)) {
        (true, true) => command.arg(format!("/select,{target}")),
        (true, false) => command.args(["-R", target]),
        (false, _) => command.arg(target),
    };
    command.spawn().is_ok()
}

/// Local time as "07:51 AM".
pub fn clock() -> String {
    let (hour, minute) = local_time();
    let twelve = if hour % 12 == 0 { 12 } else { hour % 12 };
    format!("{twelve:02}:{minute:02} {}", if hour < 12 { "AM" } else { "PM" })
}

#[cfg(windows)]
fn local_time() -> (u16, u16) {
    // SAFETY: SYSTEMTIME is plain data, and GetLocalTime fills all of it.
    let time: windows_sys::Win32::Foundation::SYSTEMTIME = unsafe {
        let mut time = std::mem::zeroed();
        windows_sys::Win32::System::SystemInformation::GetLocalTime(&mut time);
        time
    };
    (time.wHour, time.wMinute)
}

#[cfg(unix)]
fn local_time() -> (u16, u16) {
    // SAFETY: `tm` is plain data, and localtime_r fills it from the time value.
    unsafe {
        let now = libc::time(std::ptr::null_mut());
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&now, &mut tm);
        (tm.tm_hour as u16, tm.tm_min as u16)
    }
}

/// True if the system uses the dark mode for apps.
#[cfg(windows)]
pub fn system_dark() -> bool {
    use winreg::RegKey;
    use winreg::enums::HKEY_CURRENT_USER;
    RegKey::predef(HKEY_CURRENT_USER)
        .open_subkey(r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize")
        .and_then(|key| key.get_value::<u32, _>("AppsUseLightTheme"))
        .map_or(true, |light| light == 0)
}

/// True if the system uses the dark mode for apps.
#[cfg(not(windows))]
pub fn system_dark() -> bool {
    // The key exists only in dark mode.
    std::process::Command::new("defaults")
        .args(["read", "-g", "AppleInterfaceStyle"])
        .output()
        .is_ok_and(|out| out.stdout.starts_with(b"Dark"))
}

/// True if bebo runs as administrator (Windows) or as root.
pub fn is_admin() -> bool {
    // SAFETY: both functions have no arguments and only read the token or the user id of this process.
    #[cfg(windows)]
    unsafe {
        windows_sys::Win32::UI::Shell::IsUserAnAdmin() != 0
    }
    #[cfg(unix)]
    unsafe {
        libc::geteuid() == 0
    }
}

/// Fills the window with the background color through GDI. bebo uses this before the GPU is ready.
/// On macOS, the window has a background color of its own.
#[cfg(windows)]
pub fn paint_background(window: &Window, color: Rgb) {
    use windows_sys::Win32::Foundation::RECT;
    use windows_sys::Win32::Graphics::Gdi::{CreateSolidBrush, DeleteObject, FillRect, GetDC, ReleaseDC};
    use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};

    let Ok(handle) = window.window_handle() else { return };
    let RawWindowHandle::Win32(handle) = handle.as_raw() else { return };
    let hwnd = handle.hwnd.get() as *mut core::ffi::c_void;
    let size = window.inner_size();
    let rect = RECT { left: 0, top: 0, right: size.width as i32, bottom: size.height as i32 };
    // SAFETY: `hwnd` is the handle of a window that is alive. The DC and the brush are released here.
    unsafe {
        let dc = GetDC(hwnd);
        let brush = CreateSolidBrush(color[0] as u32 | (color[1] as u32) << 8 | (color[2] as u32) << 16);
        FillRect(dc, &rect, brush);
        DeleteObject(brush);
        ReleaseDC(hwnd, dc);
    }
}

#[cfg(not(windows))]
pub fn paint_background(_window: &Window, _color: Rgb) {}

/// A shortcut label for this system. macOS uses the Command key for the bebo shortcuts.
pub fn keys(label: &'static str) -> &'static str {
    if !cfg!(target_os = "macos") {
        return label;
    }
    match label {
        "Ctrl+Shift+T" => "\u{2318}T",
        "Ctrl+Shift+W" => "\u{2318}W",
        "Ctrl+Shift+F" => "\u{2318}F",
        "Ctrl+Shift+R" => "\u{2318}R",
        "Ctrl+Shift+P" => "\u{2318}P",
        "Ctrl+Shift+C" => "\u{2318}C",
        "Ctrl+Shift+V" => "\u{2318}V",
        "Ctrl+K" => "\u{2318}K",
        "Ctrl+," => "\u{2318},",
        "Ctrl+Tab" => "\u{2303}Tab",
        "Alt+Shift++" => "\u{2318}D",
        "Alt+Shift+-" => "\u{21e7}\u{2318}D",
        "Alt+Left" => "\u{2325}\u{2318}\u{2190}",
        "Alt+Right" => "\u{2325}\u{2318}\u{2192}",
        "Alt+Up" => "\u{2325}\u{2318}\u{2191}",
        "Alt+Down" => "\u{2325}\u{2318}\u{2193}",
        _ => label,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clock_has_the_twelve_hour_form() {
        let text = clock();
        assert_eq!(text.len(), 8, "{text}");
        assert!(text.ends_with("AM") || text.ends_with("PM"), "{text}");
    }
}
