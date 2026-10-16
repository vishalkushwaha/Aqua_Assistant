//! A picture of the screen, of the window in front, or of a part of it, taken the way a screen recorder takes one (the desktop as Windows has put it together) and
//! kept in memory only. Nothing is written to a file. Aqua's own windows are made black in it before it goes anywhere (see [`Shot::hide`]).

use super::geometry::Rect;
#[cfg(windows)]
use super::geometry::{Shot, MAX_PIXELS, MAX_SIDE};
#[cfg(not(windows))]
use super::geometry::Shot;

/// What to take a picture of.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    /// All the screens together.
    Screen,
    /// The window in front (it must be visible: what another window covers is the other window's picture).
    Window,
    /// A rectangle of the screen, in pixels (the corner can be negative on a second screen to the left or above).
    Region(Rect),
}

/// Is this program one of Aqua's own? Its windows are not part of what is read. (The name of the file of the program starts with "aqua": `aqua-desktop`,
/// `aqua-core`, `aqua-cli`.)
pub fn is_aqua_program(exe_file_name: &str) -> bool {
    exe_file_name.to_ascii_lowercase().starts_with("aqua")
}

#[cfg(not(windows))]
pub fn capture(_target: Target) -> Result<Shot, String> {
    Err("taking a picture of the screen is only available on Windows".into())
}

#[cfg(windows)]
pub fn capture(target: Target) -> Result<Shot, String> {
    sys::capture(target)
}

#[cfg(windows)]
mod sys {
    use super::{is_aqua_program, Rect, Shot, Target, MAX_PIXELS, MAX_SIDE};
    use std::ffi::c_void;
    use std::ptr::null_mut;
    use windows_sys::core::BOOL;
    use windows_sys::Win32::Foundation::{CloseHandle, HWND, LPARAM, RECT};
    use windows_sys::Win32::Graphics::Gdi::{
        BitBlt, CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, GetDC, ReleaseDC, SelectObject, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, CAPTUREBLT, DIB_RGB_COLORS, HGDIOBJ, SRCCOPY,
    };
    use windows_sys::Win32::System::Threading::{OpenProcess, QueryFullProcessImageNameW, PROCESS_QUERY_LIMITED_INFORMATION};
    use windows_sys::Win32::UI::HiDpi::{SetThreadDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2};
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        EnumWindows, GetForegroundWindow, GetSystemMetrics, GetWindowRect, GetWindowThreadProcessId, IsIconic, IsWindowVisible, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN,
        SM_YVIRTUALSCREEN,
    };

    /// The file name of the program that has this process id, lower case ("" when it cannot be told).
    fn exe_of(pid: u32) -> String {
        unsafe {
            let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if h.is_null() {
                return String::new();
            }
            let mut buf = [0u16; 520];
            let mut len = buf.len() as u32;
            let ok = QueryFullProcessImageNameW(h, 0, buf.as_mut_ptr(), &mut len);
            CloseHandle(h);
            if ok == 0 {
                return String::new();
            }
            String::from_utf16_lossy(&buf[..len as usize]).replace('\\', "/").rsplit('/').next().unwrap_or("").to_lowercase()
        }
    }

    fn rect_of(window: HWND) -> Option<Rect> {
        let mut r = RECT { left: 0, top: 0, right: 0, bottom: 0 };
        let ok = unsafe { GetWindowRect(window, &mut r) } != 0;
        (ok && r.right > r.left && r.bottom > r.top).then(|| Rect::new(r.left, r.top, (r.right - r.left) as u32, (r.bottom - r.top) as u32))
    }

    fn is_aqua_window(window: HWND) -> bool {
        let mut pid = 0u32;
        unsafe { GetWindowThreadProcessId(window, &mut pid) };
        pid == std::process::id() || is_aqua_program(&exe_of(pid))
    }

    unsafe extern "system" fn collect(window: HWND, lparam: LPARAM) -> BOOL {
        let list = &mut *(lparam as *mut Vec<Rect>);
        if IsWindowVisible(window) != 0 && IsIconic(window) == 0 && is_aqua_window(window) {
            if let Some(r) = rect_of(window) {
                list.push(r);
            }
        }
        1
    }

    /// Where Aqua's own windows are on the screen now (visible ones, not minimised).
    fn aqua_windows() -> Vec<Rect> {
        let mut list: Vec<Rect> = Vec::new();
        unsafe {
            EnumWindows(Some(collect), &mut list as *mut Vec<Rect> as LPARAM);
        }
        list
    }

    pub fn capture(target: Target) -> Result<Shot, String> {
        // In pixels of the display, whatever the scale of the screen is set to; the thread's earlier setting is put back.
        let previous = unsafe { SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
        let result = capture_here(target);
        if !previous.is_null() {
            unsafe { SetThreadDpiAwarenessContext(previous) };
        }
        result
    }

    fn capture_here(target: Target) -> Result<Shot, String> {
        let screen = unsafe { Rect::new(GetSystemMetrics(SM_XVIRTUALSCREEN), GetSystemMetrics(SM_YVIRTUALSCREEN), GetSystemMetrics(SM_CXVIRTUALSCREEN).max(0) as u32, GetSystemMetrics(SM_CYVIRTUALSCREEN).max(0) as u32) };
        if screen.w == 0 || screen.h == 0 {
            return Err("there is no screen to take a picture of".into());
        }
        let wanted = match target {
            Target::Screen => screen,
            Target::Window => {
                let front = unsafe { GetForegroundWindow() };
                if front.is_null() {
                    return Err("no window is in front".into());
                }
                if unsafe { IsIconic(front) } != 0 {
                    return Err("the window in front is minimised, so there is nothing of it on the screen".into());
                }
                if is_aqua_window(front) {
                    return Err("the window in front is Aqua's own, which is not read: bring the window to be read to the front".into());
                }
                rect_of(front).ok_or("the window in front has no size")?
            }
            Target::Region(r) => r,
        };
        // Only what is on a screen can be taken.
        let area = wanted.intersect(&screen).ok_or("that part is not on the screen")?;
        if area.w > MAX_SIDE || area.h > MAX_SIDE || area.area() > MAX_PIXELS {
            return Err(format!("{} by {} pixels is more than is read at once (at most {} on a side and {} million pixels): ask for a smaller part", area.w, area.h, MAX_SIDE, MAX_PIXELS / 1_000_000));
        }
        let (w, h) = (area.w as i32, area.h as i32);
        let mut bgra = unsafe {
            let display = GetDC(null_mut());
            if display.is_null() {
                return Err("Windows would not give the screen to be read".into());
            }
            let mem = CreateCompatibleDC(display);
            let mut bmi: BITMAPINFO = std::mem::zeroed();
            bmi.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
            bmi.bmiHeader.biWidth = w;
            bmi.bmiHeader.biHeight = -h; // top-down
            bmi.bmiHeader.biPlanes = 1;
            bmi.bmiHeader.biBitCount = 32;
            bmi.bmiHeader.biCompression = BI_RGB;
            let mut bits: *mut c_void = null_mut();
            let dib = CreateDIBSection(display, &bmi, DIB_RGB_COLORS, &mut bits, null_mut(), 0);
            let result = if dib.is_null() || bits.is_null() || mem.is_null() {
                Err("could not make room for the picture".to_string())
            } else {
                let old = SelectObject(mem, dib as HGDIOBJ);
                // CAPTUREBLT: windows that are drawn on top of the others without being part of them (menus, tooltips) are in the picture.
                let ok = BitBlt(mem, 0, 0, w, h, display, area.x, area.y, SRCCOPY | CAPTUREBLT);
                let copied = if ok == 0 { Err("Windows would not copy the screen (it may be locked)".to_string()) } else { Ok(std::slice::from_raw_parts(bits as *const u8, w as usize * h as usize * 4).to_vec()) };
                SelectObject(mem, old);
                copied
            };
            if !dib.is_null() {
                DeleteObject(dib as HGDIOBJ);
            }
            if !mem.is_null() {
                DeleteDC(mem);
            }
            ReleaseDC(null_mut(), display);
            result?
        };
        // What Windows leaves in the fourth byte is nothing; a picture whose alpha is 0 reads as empty.
        bgra.chunks_exact_mut(4).for_each(|px| px[3] = 255);
        let mut shot = Shot::new(area, bgra)?;
        shot.hide(&aqua_windows());
        Ok(shot)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aquas_own_programs_are_told_by_the_name_of_their_file() {
        for yes in ["aqua-desktop.exe", "AQUA-CORE.EXE", "aqua_vision_adapter-3f2a.exe", "aqua-cli.exe"] {
            assert!(is_aqua_program(yes), "{yes}");
        }
        for no in ["notepad.exe", "chrome.exe", "squaresville.exe", "", "an-aqua-fan.exe"] {
            assert!(!is_aqua_program(no), "{no}");
        }
    }

    #[cfg(windows)]
    #[test]
    fn a_part_of_the_screen_comes_back_the_size_that_was_asked_for_with_every_pixel_opaque_and_what_is_not_on_the_screen_is_refused() {
        let shot = capture(Target::Region(Rect::new(0, 0, 64, 48))).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!((shot.area.w, shot.area.h), (64, 48));
        assert_eq!(shot.bgra.len(), 64 * 48 * 4);
        assert!(shot.bgra.chunks_exact(4).all(|px| px[3] == 255), "alpha is opaque");
        // A part that is partly off the screen is cut to what is on it (a second screen to the left or above makes more of it count, so the least that can be said is that it
        // is not larger than asked); one that is wholly off is refused.
        let cut = capture(Target::Region(Rect::new(-30, -20, 100, 100))).unwrap();
        assert!(cut.area.w > 0 && cut.area.w <= 100 && cut.area.h > 0 && cut.area.h <= 100, "{:?}", cut.area);
        assert_eq!(cut.bgra.len(), cut.area.area() as usize * 4);
        let off = capture(Target::Region(Rect::new(1_000_000, 1_000_000, 10, 10))).unwrap_err();
        assert!(off.contains("not on the screen"), "{off}");
        // The whole screen is the size of the screens together, and its picture has that many bytes.
        let all = capture(Target::Screen).unwrap();
        assert_eq!(all.bgra.len(), all.area.area() as usize * 4);
    }
}
