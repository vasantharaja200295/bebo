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
/// Windows: the elevation of the process token. `IsUserAnAdmin` gives the same answer, but it loads shell32.dll (about 10 ms).
pub fn is_admin() -> bool {
    #[cfg(windows)]
    // SAFETY: the token handle is closed here, and TOKEN_ELEVATION is plain data of the size that the call gets.
    unsafe {
        use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
        use windows_sys::Win32::Security::{GetTokenInformation, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation};
        use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
        let mut token: HANDLE = std::ptr::null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            return false;
        }
        let mut elevation = TOKEN_ELEVATION { TokenIsElevated: 0 };
        let mut size = 0u32;
        let size_of = std::mem::size_of::<TOKEN_ELEVATION>() as u32;
        let ok = GetTokenInformation(token, TokenElevation, (&mut elevation as *mut TOKEN_ELEVATION).cast(), size_of, &mut size);
        CloseHandle(token);
        ok != 0 && elevation.TokenIsElevated != 0
    }
    #[cfg(unix)]
    // SAFETY: the function has no arguments and only reads the user id of this process.
    unsafe {
        libc::geteuid() == 0
    }
}

/// What the first frame shows before the GPU is ready. macOS has no such frame.
#[cfg_attr(not(windows), allow(dead_code))]
pub struct FirstFrame<'a> {
    pub bg: Rgb,
    pub bar: Rgb,
    pub line: Rgb,
    pub fg: Rgb,
    pub dim: Rgb,
    pub title_color: Rgb,
    /// The display scale.
    pub scale: f32,
    /// The name of the tab in the title bar, or empty if the title bar shows tabs.
    pub title: &'a str,
}

/// Draws the first frame with GDI, about 2 ms after the window opens: the background, the title bar, the icon, the name,
/// the tab name, and the window buttons, at the same places as the GPU frame. The GPU frame replaces it about 250 ms later.
/// The terminal text comes only with the GPU frame.
#[cfg(windows)]
pub fn paint_first_frame(window: &Window, f: &FirstFrame) {
    use windows_sys::Win32::Foundation::{POINT, RECT, SIZE};
    use windows_sys::Win32::Graphics::Gdi::*;
    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows_sys::Win32::UI::WindowsAndMessaging::{DI_NORMAL, DestroyIcon, DrawIconEx, IMAGE_ICON, LoadImageW};
    use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};

    static FONTS: std::sync::Once = std::sync::Once::new();
    let Ok(handle) = window.window_handle() else { return };
    let RawWindowHandle::Win32(handle) = handle.as_raw() else { return };
    let hwnd = handle.hwnd.get() as *mut core::ffi::c_void;
    let size = window.inner_size();
    let (w, h) = (size.width as i32, size.height as i32);
    let s = f.scale;
    let px = |v: f32| (v * s).round() as i32;
    let color = |c: Rgb| c[0] as u32 | (c[1] as u32) << 8 | (c[2] as u32) << 16;
    let wide = |t: &str| t.encode_utf16().collect::<Vec<u16>>();
    let bar = px(30.0);
    let mid = bar / 2;
    // SAFETY: `hwnd` is the handle of a window that is alive. Each GDI object is selected out and deleted here,
    // and the DC is released. The font data lives in the program file for the life of the process.
    unsafe {
        // The interface font Geist is inside bebo. GDI gets it once, for this process only.
        FONTS.call_once(|| {
            for data in [&include_bytes!("../assets/fonts/Geist-Regular.ttf")[..], &include_bytes!("../assets/fonts/Geist-SemiBold.ttf")[..]] {
                let count = 0u32;
                AddFontMemResourceEx(data.as_ptr().cast(), data.len() as u32, std::ptr::null(), &count);
            }
        });
        let dc = GetDC(hwnd);
        let fill = |r: RECT, c: Rgb| {
            let brush = CreateSolidBrush(color(c));
            FillRect(dc, &r, brush);
            DeleteObject(brush);
        };
        fill(RECT { left: 0, top: 0, right: w, bottom: h }, f.bg);
        fill(RECT { left: 0, top: 0, right: w, bottom: bar }, f.bar);
        fill(RECT { left: 0, top: bar - 1, right: w, bottom: bar }, f.line);

        // The icon of the program file: 1 is for dark mode, 2 for light mode (see build.rs). The color of the bar tells which.
        let dark = f.bar.iter().map(|c| *c as u32).sum::<u32>() < 384;
        let icon_size = px(16.0);
        let icon = LoadImageW(GetModuleHandleW(std::ptr::null()), if dark { 1 } else { 2 } as usize as *const u16, IMAGE_ICON, icon_size, icon_size, 0);
        if !icon.is_null() {
            DrawIconEx(dc, px(10.0) - px(1.0), mid - icon_size / 2, icon, icon_size, icon_size, 0, std::ptr::null_mut(), DI_NORMAL);
            DestroyIcon(icon);
        }

        SetBkMode(dc, TRANSPARENT as i32);
        let font = |weight: i32| {
            let face = wide("Geist\0");
            let mut lf: LOGFONTW = std::mem::zeroed();
            lf.lfHeight = -px(12.0);
            lf.lfWeight = weight;
            lf.lfQuality = CLEARTYPE_QUALITY;
            lf.lfFaceName[..face.len()].copy_from_slice(&face);
            CreateFontIndirectW(&lf)
        };
        // Text with its left side at `x` and its middle at the middle of the bar. Returns the width.
        let text = |x: i32, t: &str, c: Rgb, weight: i32| -> i32 {
            let font = font(weight);
            let old = SelectObject(dc, font);
            SetTextColor(dc, color(c));
            let t = wide(t);
            let mut extent = SIZE { cx: 0, cy: 0 };
            GetTextExtentPoint32W(dc, t.as_ptr(), t.len() as i32, &mut extent);
            TextOutW(dc, x, mid - extent.cy / 2, t.as_ptr(), t.len() as i32);
            SelectObject(dc, old);
            DeleteObject(font);
            extent.cx
        };
        let mut x = px(31.0);
        x += text(x, "bebo", f.fg, 600) + px(10.0);
        if !f.title.is_empty() {
            text(x, "/", f.dim, 400);
            // The icon of the shell comes with the GPU frame.
            x += px(14.0);
            x += px(27.0) + text(x + px(21.0), f.title, f.title_color, 400);
        }

        // Thin lines at the size of the icons of the GPU frame (Lucide, 2 px strokes on a 24 px grid).
        let pen = |c: Rgb| CreatePen(PS_SOLID, px(1.0).max(1), color(c));
        let line = |x0: i32, y0: i32, x1: i32, y1: i32| {
            MoveToEx(dc, x0, y0, std::ptr::null_mut::<POINT>());
            LineTo(dc, x1, y1);
        };
        let (h4, h3) = (px(4.0), px(3.0));
        // New tab (+) and the profile menu (chevron), 26 px apart after the tab name.
        let dim_pen = pen(f.title_color);
        let old = SelectObject(dc, dim_pen);
        let plus = x + px(14.0);
        line(plus - h4, mid, plus + h4 + 1, mid);
        line(plus, mid - h4, plus, mid + h4 + 1);
        let chevron = plus + px(26.0);
        line(chevron - h3, mid - px(1.5), chevron, mid + px(1.5));
        line(chevron, mid + px(1.5), chevron + h3 + 1, mid - px(1.5) - 1);
        SelectObject(dc, old);
        DeleteObject(dim_pen);

        // Minimize, maximize, and close.
        let fg_pen = pen(f.fg);
        let old = SelectObject(dc, fg_pen);
        let button = px(46.0);
        let center = |i: i32| w - (3 - i) * button + button / 2;
        line(center(0) - h4, mid, center(0) + h4 + 1, mid);
        let c = center(1);
        line(c - h4, mid - h4, c + h4, mid - h4);
        line(c + h4, mid - h4, c + h4, mid + h4);
        line(c + h4, mid + h4, c - h4, mid + h4);
        line(c - h4, mid + h4, c - h4, mid - h4);
        let c = center(2);
        line(c - h3, mid - h3, c + h3 + 1, mid + h3 + 1);
        line(c + h3, mid - h3, c - h3 - 1, mid + h3 + 1);
        SelectObject(dc, old);
        DeleteObject(fg_pen);
        ReleaseDC(hwnd, dc);
    }
}

/// On macOS, the window has a background color of its own.
#[cfg(not(windows))]
pub fn paint_first_frame(_window: &Window, _f: &FirstFrame) {}

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
