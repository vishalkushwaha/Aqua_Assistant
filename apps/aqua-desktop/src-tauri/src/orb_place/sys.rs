//! The Windows side of orb placement: what window is in front, where the screens are, one picture of a window, and the
//! event hooks that say when something changed. Every function here is small and can fail quietly (returns None or does
//! nothing); the caller decides what that means.

use aqua_orb_placement::{BusyMap, Monitor, Rect, BLOCK};

#[derive(Debug, Clone)]
pub struct WindowInfo {
    pub hwnd: isize,
    pub pid: u32,
    pub tid: u32,
    pub class: String,
    pub title: String,
    pub rect: Rect,
    pub iconic: bool,
    pub cloaked: bool,
    pub visible: bool,
    /// File name of the program, lower case; empty if Windows would not say (a program running as administrator).
    pub exe: String,
    pub has_caption: bool,
    /// A see-through or click-through overlay (a game overlay, a screen annotation tool): not something the person is working in.
    pub overlay: bool,
}

#[cfg(windows)]
pub use win::*;
#[cfg(not(windows))]
pub use other::*;

#[cfg(windows)]
mod win {
    use super::*;
    use super::super::{EventKind, Msg};
    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::ffi::c_void;
    use std::ptr::{null, null_mut};
    use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU32, Ordering};
    use std::sync::mpsc::Sender;
    use std::sync::Mutex;
    use windows_sys::core::BOOL;
    use windows_sys::Win32::Foundation::*;
    use windows_sys::Win32::Graphics::Dwm::*;
    use windows_sys::Win32::Graphics::Gdi::*;
    use windows_sys::Win32::Storage::Xps::PrintWindow;
    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows_sys::Win32::System::Threading::*;
    use windows_sys::Win32::UI::Accessibility::*;
    use windows_sys::Win32::UI::HiDpi::GetDpiForWindow;
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_LBUTTON, VK_RBUTTON};
    use windows_sys::Win32::UI::WindowsAndMessaging::*;

    /// The orb's own window, so the hook can tell when it is the one being dragged.
    pub static ORB: AtomicIsize = AtomicIsize::new(0);
    /// True while the person is dragging the orb (between the system's move-start and move-end events).
    pub static DRAGGING: AtomicBool = AtomicBool::new(false);
    static TX: Mutex<Option<Sender<Msg>>> = Mutex::new(None);
    static HOOK_THREAD: AtomicU32 = AtomicU32::new(0);

    const WM_REHOOK: u32 = WM_APP + 1;
    const WM_WHEEL_HOOK: u32 = WM_APP + 3;
    const FLAGS: u32 = WINEVENT_OUTOFCONTEXT;

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    fn send(msg: Msg) {
        if let Some(tx) = TX.lock().unwrap_or_else(|p| p.into_inner()).as_ref() {
            let _ = tx.send(msg);
        }
    }

    unsafe extern "system" fn on_event(_hook: HWINEVENTHOOK, event: u32, hwnd: HWND, id_object: i32, id_child: i32, _thread: u32, _time: u32) {
        if id_object != OBJID_WINDOW || id_child != 0 || hwnd.is_null() {
            return;
        }
        let kind = match event {
            EVENT_SYSTEM_FOREGROUND => EventKind::Foreground,
            EVENT_SYSTEM_MINIMIZESTART => EventKind::Minimize,
            EVENT_SYSTEM_MINIMIZEEND => EventKind::Restore,
            EVENT_SYSTEM_MOVESIZESTART => EventKind::MoveSizeStart,
            EVENT_SYSTEM_MOVESIZEEND => EventKind::MoveSizeEnd,
            EVENT_OBJECT_LOCATIONCHANGE => EventKind::Location,
            EVENT_OBJECT_NAMECHANGE => EventKind::Name,
            _ => return,
        };
        let h = hwnd as isize;
        if h == ORB.load(Ordering::Relaxed) {
            match kind {
                EventKind::MoveSizeStart => DRAGGING.store(true, Ordering::SeqCst),
                EventKind::MoveSizeEnd => DRAGGING.store(false, Ordering::SeqCst),
                _ => {}
            }
        }
        send(Msg::Event(kind, h));
    }

    static WHEEL_EPOCH: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    static LAST_WHEEL_MS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    /// Mouse hook. Only wheel messages are looked at (and only their position); everything else passes straight through.
    /// At most one message every 50 ms is passed on, so a fast scroll does not flood the worker.
    unsafe extern "system" fn on_mouse(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        if code >= 0 && (wparam as u32 == WM_MOUSEWHEEL || wparam as u32 == WM_MOUSEHWHEEL) {
            let now = WHEEL_EPOCH.get_or_init(std::time::Instant::now).elapsed().as_millis() as u64 + 1;
            if now.saturating_sub(LAST_WHEEL_MS.load(Ordering::Relaxed)) >= 50 {
                LAST_WHEEL_MS.store(now, Ordering::Relaxed);
                let info = &*(lparam as *const MSLLHOOKSTRUCT);
                send(Msg::Wheel(info.pt.x, info.pt.y));
            }
        }
        CallNextHookEx(null_mut(), code, wparam, lparam)
    }

    unsafe extern "system" fn sink_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        match msg {
            WM_DISPLAYCHANGE => send(Msg::Display),
            WM_SETTINGCHANGE if wparam as u32 == SPI_SETWORKAREA => send(Msg::Display),
            _ => {}
        }
        DefWindowProcW(hwnd, msg, wparam, lparam)
    }

    /// Start the thread that owns the event hooks. Returns Err (with the reason) if Windows would not give us them; the
    /// caller then falls back to looking at the front window every second or so.
    pub fn start_hooks(tx: Sender<Msg>) -> Result<(), String> {
        *TX.lock().unwrap_or_else(|p| p.into_inner()) = Some(tx);
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<u32, String>>();
        std::thread::Builder::new()
            .name("aqua-orb-hooks".into())
            .spawn(move || unsafe { hook_thread(ready_tx) })
            .map_err(|e| format!("could not start the hook thread: {e}"))?;
        match ready_rx.recv_timeout(std::time::Duration::from_secs(5)) {
            Ok(Ok(tid)) => {
                HOOK_THREAD.store(tid, Ordering::SeqCst);
                Ok(())
            }
            Ok(Err(e)) => Err(e),
            Err(_) => Err("the hook thread did not answer".into()),
        }
    }

    unsafe fn hook_thread(ready: Sender<Result<u32, String>>) {
        let tid = GetCurrentThreadId();
        let hooks = [
            SetWinEventHook(EVENT_SYSTEM_FOREGROUND, EVENT_SYSTEM_FOREGROUND, null_mut(), Some(on_event), 0, 0, FLAGS),
            SetWinEventHook(EVENT_SYSTEM_MOVESIZESTART, EVENT_SYSTEM_MOVESIZEEND, null_mut(), Some(on_event), 0, 0, FLAGS),
            SetWinEventHook(EVENT_SYSTEM_MINIMIZESTART, EVENT_SYSTEM_MINIMIZEEND, null_mut(), Some(on_event), 0, 0, FLAGS),
        ];
        if hooks.iter().any(|h| h.is_null()) {
            for h in hooks.iter().filter(|h| !h.is_null()) {
                UnhookWinEvent(*h);
            }
            let _ = ready.send(Err("Windows refused the window event hooks".into()));
            return;
        }
        // A hidden window that only exists to hear "the screens changed" and "the taskbar moved".
        let class = wide("AquaOrbPlacementSink");
        let hinst = GetModuleHandleW(null());
        let wc = WNDCLASSW { style: 0, lpfnWndProc: Some(sink_proc), cbClsExtra: 0, cbWndExtra: 0, hInstance: hinst, hIcon: null_mut(), hCursor: null_mut(), hbrBackground: null_mut(), lpszMenuName: null(), lpszClassName: class.as_ptr() };
        RegisterClassW(&wc);
        let sink = CreateWindowExW(0, class.as_ptr(), class.as_ptr(), WS_POPUP, 0, 0, 0, 0, null_mut(), null_mut(), hinst, null());
        let _ = ready.send(Ok(tid));
        // Windows silently removes a low-level mouse hook if the thread was too slow to answer once (easy when the whole PC is busy).
        // So the hook is put in again every half minute; this costs one wake-up per 30 seconds.
        SetTimer(null_mut(), 0, 30_000, None);

        let mut thread_hook: HWINEVENTHOOK = null_mut();
        let mut wheel_hook: HHOOK = null_mut();
        let mut msg: MSG = std::mem::zeroed();
        while GetMessageW(&mut msg, null_mut(), 0, 0) > 0 {
            if msg.hwnd.is_null() {
                match msg.message {
                    // Moves, resizes and title changes are only listened to for the window that is in front: wParam is its
                    // process and lParam its thread. Listening to every window on the system would wake this thread all the time.
                    WM_REHOOK => {
                        if !thread_hook.is_null() {
                            UnhookWinEvent(thread_hook);
                        }
                        thread_hook = SetWinEventHook(EVENT_OBJECT_LOCATIONCHANGE, EVENT_OBJECT_NAMECHANGE, null_mut(), Some(on_event), msg.wParam as u32, msg.lParam as u32, FLAGS);
                        continue;
                    }
                    WM_TIMER => {
                        if !wheel_hook.is_null() {
                            UnhookWindowsHookEx(wheel_hook);
                            wheel_hook = SetWindowsHookExW(WH_MOUSE_LL, Some(on_mouse), GetModuleHandleW(null()), 0);
                        }
                        continue;
                    }
                    WM_WHEEL_HOOK => {
                        if msg.wParam != 0 && wheel_hook.is_null() {
                            wheel_hook = SetWindowsHookExW(WH_MOUSE_LL, Some(on_mouse), GetModuleHandleW(null()), 0);
                        } else if msg.wParam == 0 && !wheel_hook.is_null() {
                            UnhookWindowsHookEx(wheel_hook);
                            wheel_hook = null_mut();
                        }
                        continue;
                    }
                    _ => {}
                }
            }
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        if !wheel_hook.is_null() {
            UnhookWindowsHookEx(wheel_hook);
        }
        if !thread_hook.is_null() {
            UnhookWinEvent(thread_hook);
        }
        for h in hooks {
            UnhookWinEvent(h);
        }
        if !sink.is_null() {
            DestroyWindow(sink);
        }
    }

    /// Listen for moves and title changes of this window's thread (called when another window comes to the front).
    pub fn listen_to(pid: u32, tid: u32) {
        let t = HOOK_THREAD.load(Ordering::SeqCst);
        if t != 0 {
            unsafe {
                PostThreadMessageW(t, WM_REHOOK, pid as usize, tid as isize);
            }
        }
    }

    pub fn thread_cpu_ms() -> u64 {
        unsafe {
            let (mut c, mut e, mut k, mut u) = (std::mem::zeroed::<FILETIME>(), std::mem::zeroed::<FILETIME>(), std::mem::zeroed::<FILETIME>(), std::mem::zeroed::<FILETIME>());
            if GetThreadTimes(GetCurrentThread(), &mut c, &mut e, &mut k, &mut u) == 0 {
                return 0;
            }
            let t = |f: FILETIME| ((u64::from(f.dwHighDateTime) << 32) | u64::from(f.dwLowDateTime)) / 10_000;
            t(k) + t(u)
        }
    }

    /// Install or remove the mouse-wheel listener. Off means no mouse hook exists at all.
    pub fn set_wheel_hook(on: bool) {
        let t = HOOK_THREAD.load(Ordering::SeqCst);
        if t != 0 {
            unsafe {
                PostThreadMessageW(t, WM_WHEEL_HOOK, on as usize, 0);
            }
        }
    }

    /// The top-level window under a screen point.
    pub fn window_at(x: i32, y: i32) -> isize {
        unsafe {
            let hit = WindowFromPoint(POINT { x, y });
            if hit.is_null() {
                return 0;
            }
            // Walk down from the top-most window at that point, skipping overlays and anything that is hidden or minimised.
            let mut cur = GetAncestor(hit, GA_ROOT);
            for _ in 0..40 {
                if cur.is_null() {
                    return 0;
                }
                let mut r: RECT = std::mem::zeroed();
                let inside = GetWindowRect(cur, &mut r) != 0 && x >= r.left && x < r.right && y >= r.top && y < r.bottom;
                if inside && IsWindowVisible(cur) != 0 && IsIconic(cur) == 0 && !is_overlay(cur) {
                    return cur as isize;
                }
                cur = GetWindow(cur, GW_HWNDNEXT);
            }
            0
        }
    }

    /// Processor time used so far by the hook thread (it runs the hook callbacks), in milliseconds.
    pub fn hook_cpu_ms() -> u64 {
        let tid = HOOK_THREAD.load(Ordering::SeqCst);
        if tid == 0 {
            return 0;
        }
        unsafe {
            let h = OpenThread(THREAD_QUERY_LIMITED_INFORMATION, 0, tid);
            if h.is_null() {
                return 0;
            }
            let (mut c, mut e, mut k, mut u) = (std::mem::zeroed::<FILETIME>(), std::mem::zeroed::<FILETIME>(), std::mem::zeroed::<FILETIME>(), std::mem::zeroed::<FILETIME>());
            let ok = GetThreadTimes(h, &mut c, &mut e, &mut k, &mut u);
            CloseHandle(h);
            if ok == 0 {
                return 0;
            }
            let t = |f: FILETIME| ((u64::from(f.dwHighDateTime) << 32) | u64::from(f.dwLowDateTime)) / 10_000;
            t(k) + t(u)
        }
    }

    pub fn foreground() -> isize {
        unsafe { GetForegroundWindow() as isize }
    }

    /// Clip a window to the union of some shapes (`(is_ellipse, x, y, w, h)` in CSS pixels, scaled by `dpr`). Everything outside
    /// is not part of the window: it is not drawn and mouse clicks go to whatever is underneath. No shapes: the whole window again.
    pub fn set_window_region(hwnd: isize, shapes: &[(bool, f64, f64, f64, f64)], dpr: f64) {
        unsafe {
            let h = hwnd as HWND;
            if shapes.is_empty() {
                SetWindowRgn(h, null_mut(), 1);
                return;
            }
            let base = CreateRectRgn(0, 0, 0, 0);
            for &(ellipse, x, y, w, hh) in shapes {
                let (l, t, r, b) = ((x * dpr).floor() as i32, (y * dpr).floor() as i32, ((x + w) * dpr).ceil() as i32, ((y + hh) * dpr).ceil() as i32);
                let part = if ellipse { CreateEllipticRgn(l, t, r, b) } else { CreateRectRgn(l, t, r, b) };
                if !part.is_null() {
                    CombineRgn(base, base, part, RGN_OR);
                    DeleteObject(part as HGDIOBJ);
                }
            }
            // The system owns the region from here on.
            SetWindowRgn(h, base, 1);
        }
    }

    fn button_down() -> bool {
        unsafe { (GetAsyncKeyState(VK_LBUTTON as i32) as u16 & 0x8000) != 0 || (GetAsyncKeyState(VK_RBUTTON as i32) as u16 & 0x8000) != 0 }
    }

    /// The person really is dragging the orb: Windows said a drag began and a mouse button is still down.
    pub fn drag_in_progress() -> bool {
        DRAGGING.load(Ordering::SeqCst) && button_down()
    }

    /// Windows said a drag began, the buttons are up, and no "drag ended" ever came (it can be missed): clear the flag so the
    /// orb is not treated as held for ever. True when that happened, so the caller can save where the orb ended up.
    pub fn stale_drag_cleared() -> bool {
        DRAGGING.load(Ordering::SeqCst) && !button_down() && DRAGGING.swap(false, Ordering::SeqCst)
    }

    thread_local! {
        static EXE_CACHE: RefCell<HashMap<u32, String>> = RefCell::new(HashMap::new());
    }

    fn exe_of(pid: u32) -> String {
        if let Some(e) = EXE_CACHE.with(|c| c.borrow().get(&pid).cloned()) {
            return e;
        }
        let exe = unsafe {
            let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if h.is_null() {
                String::new()
            } else {
                let mut buf = [0u16; 520];
                let mut len = buf.len() as u32;
                let ok = QueryFullProcessImageNameW(h, 0, buf.as_mut_ptr(), &mut len);
                CloseHandle(h);
                if ok == 0 {
                    String::new()
                } else {
                    let path = String::from_utf16_lossy(&buf[..len as usize]);
                    path.rsplit(['\\', '/']).next().unwrap_or("").to_lowercase()
                }
            }
        };
        EXE_CACHE.with(|c| {
            let mut c = c.borrow_mut();
            if c.len() > 64 {
                c.clear();
            }
            c.insert(pid, exe.clone());
        });
        exe
    }

    fn class_of(hwnd: HWND) -> String {
        let mut buf = [0u16; 128];
        let n = unsafe { GetClassNameW(hwnd, buf.as_mut_ptr(), buf.len() as i32) };
        String::from_utf16_lossy(&buf[..n.max(0) as usize])
    }

    fn title_of(hwnd: HWND) -> String {
        let mut buf = [0u16; 300];
        let n = unsafe { GetWindowTextW(hwnd, buf.as_mut_ptr(), buf.len() as i32) };
        String::from_utf16_lossy(&buf[..n.max(0) as usize])
    }

    pub fn window_rect(hwnd: isize) -> Option<Rect> {
        unsafe {
            let mut r: RECT = std::mem::zeroed();
            (GetWindowRect(hwnd as HWND, &mut r) != 0).then(|| Rect::new(r.left, r.top, r.right - r.left, r.bottom - r.top))
        }
    }

    pub fn is_visible(hwnd: isize) -> bool {
        unsafe { IsWindow(hwnd as HWND) != 0 && IsWindowVisible(hwnd as HWND) != 0 }
    }

    pub fn dpi(hwnd: isize) -> u32 {
        let d = unsafe { GetDpiForWindow(hwnd as HWND) };
        if d == 0 { 96 } else { d }
    }

    fn is_overlay(h: HWND) -> bool {
        let ex = unsafe { GetWindowLongW(h, GWL_EXSTYLE) } as u32;
        ex & WS_EX_TRANSPARENT != 0 || (ex & WS_EX_LAYERED != 0 && ex & WS_EX_NOACTIVATE != 0)
    }

    pub fn describe(hwnd: isize) -> Option<WindowInfo> {
        unsafe {
            let h = hwnd as HWND;
            if h.is_null() || IsWindow(h) == 0 {
                return None;
            }
            let rect = window_rect(hwnd)?;
            let mut pid = 0u32;
            let tid = GetWindowThreadProcessId(h, &mut pid);
            let mut cloaked: u32 = 0;
            DwmGetWindowAttribute(h, DWMWA_CLOAKED as u32, &mut cloaked as *mut u32 as *mut c_void, 4);
            let style = GetWindowLongW(h, GWL_STYLE) as u32;
            Some(WindowInfo {
                hwnd,
                pid,
                tid,
                class: class_of(h),
                title: title_of(h),
                rect,
                iconic: IsIconic(h) != 0,
                cloaked: cloaked != 0,
                visible: IsWindowVisible(h) != 0,
                exe: exe_of(pid),
                has_caption: style & WS_CAPTION == WS_CAPTION,
                overlay: is_overlay(h),
            })
        }
    }

    pub fn cursor() -> Option<(i32, i32)> {
        unsafe {
            let mut p: POINT = std::mem::zeroed();
            (GetCursorPos(&mut p) != 0).then_some((p.x, p.y))
        }
    }

    pub fn set_pos(hwnd: isize, x: i32, y: i32) {
        unsafe {
            SetWindowPos(hwnd as HWND, null_mut(), x, y, 0, 0, SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE);
        }
    }

    unsafe extern "system" fn monitor_cb(h: HMONITOR, _dc: HDC, _r: *mut RECT, lp: LPARAM) -> BOOL {
        let list = &mut *(lp as *mut Vec<Monitor>);
        let mut mi: MONITORINFOEXW = std::mem::zeroed();
        mi.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
        if GetMonitorInfoW(h, &mut mi as *mut MONITORINFOEXW as *mut MONITORINFO) != 0 {
            let r = |r: RECT| Rect::new(r.left, r.top, r.right - r.left, r.bottom - r.top);
            let n = mi.szDevice.iter().position(|c| *c == 0).unwrap_or(mi.szDevice.len());
            list.push(Monitor { id: String::from_utf16_lossy(&mi.szDevice[..n]), rect: r(mi.monitorInfo.rcMonitor), work: r(mi.monitorInfo.rcWork), primary: mi.monitorInfo.dwFlags & MONITORINFOF_PRIMARY != 0 });
        }
        1
    }

    pub fn monitors() -> Vec<Monitor> {
        let mut list: Vec<Monitor> = Vec::new();
        unsafe {
            EnumDisplayMonitors(null_mut(), null(), Some(monitor_cb), &mut list as *mut Vec<Monitor> as LPARAM);
        }
        list
    }

    /// One picture of a window, analysed. Returns Err with the reason when the window cannot be read (it runs as
    /// administrator, it is minimised, or Windows hands back an empty picture).
    pub fn capture(hwnd: isize, rect: Rect, region: Option<Rect>, background: Option<u8>) -> Result<BusyMap, String> {
        const MAX_PIXELS: i64 = 9_000_000;
        let (w, h) = (rect.w, rect.h);
        if w < 64 || h < 64 {
            return Err("the window is too small to look at".into());
        }
        if rect.area() > MAX_PIXELS {
            return Err("the window is larger than 9 million pixels, so it was not looked at".into());
        }
        unsafe {
            if IsHungAppWindow(hwnd as HWND) != 0 {
                return Err("the window is not responding".into());
            }
            let screen = GetDC(null_mut());
            if screen.is_null() {
                return Err("no display context".into());
            }
            let mem = CreateCompatibleDC(screen);
            let mut bmi: BITMAPINFO = std::mem::zeroed();
            bmi.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
            bmi.bmiHeader.biWidth = w;
            bmi.bmiHeader.biHeight = -h; // top-down
            bmi.bmiHeader.biPlanes = 1;
            bmi.bmiHeader.biBitCount = 32;
            bmi.bmiHeader.biCompression = BI_RGB;
            let mut bits: *mut c_void = null_mut();
            let dib = CreateDIBSection(screen, &bmi, DIB_RGB_COLORS, &mut bits, null_mut(), 0);
            let result = if dib.is_null() || bits.is_null() || mem.is_null() {
                Err("could not make a picture buffer".to_string())
            } else {
                let old = SelectObject(mem, dib as HGDIOBJ);
                let ok = PrintWindow(hwnd as HWND, mem, PW_RENDERFULLCONTENT);
                let r = if ok == 0 {
                    Err("Windows would not draw this window (it may run as administrator)".to_string())
                } else {
                    let px = std::slice::from_raw_parts(bits as *const u8, w as usize * h as usize * 4);
                    BusyMap::from_bgra_region(px, w as usize, h as usize, w as usize * 4, rect, BLOCK, region.as_ref(), background).ok_or_else(|| "the picture was not usable".to_string())
                };
                SelectObject(mem, old);
                r
            };
            if !dib.is_null() {
                DeleteObject(dib as HGDIOBJ);
            }
            if !mem.is_null() {
                DeleteDC(mem);
            }
            ReleaseDC(null_mut(), screen);
            result
        }
    }
}

#[cfg(not(windows))]
mod other {
    use super::*;
    use super::super::Msg;
    use std::sync::mpsc::Sender;

    pub static ORB: std::sync::atomic::AtomicIsize = std::sync::atomic::AtomicIsize::new(0);
    pub static DRAGGING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    pub fn start_hooks(_tx: Sender<Msg>) -> Result<(), String> {
        Err("orb placement is only available on Windows".into())
    }
    pub fn listen_to(_pid: u32, _tid: u32) {}
    pub fn set_wheel_hook(_on: bool) {}
    pub fn window_at(_x: i32, _y: i32) -> isize {
        0
    }
    pub fn thread_cpu_ms() -> u64 {
        0
    }
    pub fn hook_cpu_ms() -> u64 {
        0
    }
    pub fn foreground() -> isize {
        0
    }
    pub fn set_window_region(_h: isize, _s: &[(bool, f64, f64, f64, f64)], _dpr: f64) {}
    pub fn drag_in_progress() -> bool {
        false
    }
    pub fn stale_drag_cleared() -> bool {
        false
    }
    pub fn window_rect(_h: isize) -> Option<Rect> {
        None
    }
    pub fn is_visible(_h: isize) -> bool {
        false
    }
    pub fn dpi(_h: isize) -> u32 {
        96
    }
    pub fn describe(_h: isize) -> Option<WindowInfo> {
        None
    }
    pub fn cursor() -> Option<(i32, i32)> {
        None
    }
    pub fn set_pos(_h: isize, _x: i32, _y: i32) {}
    pub fn monitors() -> Vec<Monitor> {
        Vec::new()
    }
    pub fn capture(_h: isize, _r: Rect, _region: Option<Rect>, _bg: Option<u8>) -> Result<BusyMap, String> {
        Err("not supported on this system".into())
    }
}
