// Prevents additional console window on Windows in release, DO NOT REMOVE!!
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod orb_place;
mod updates;

use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use tauri::{Manager, RunEvent};
use tauri_plugin_shell::process::{CommandChild, CommandEvent};
use tauri_plugin_shell::ShellExt;
use tauri_plugin_opener::OpenerExt;

const GATEWAY: &str = "http://127.0.0.1:8767";

/// The core's process, kept so it can be stopped when the window closes (otherwise it would keep running).
struct Core(Mutex<Option<CommandChild>>);

#[derive(serde::Serialize)]
struct GatewayInfo {
    base_url: String,
    ui_token: String,
}

/// Set once the app is quitting, so the core stopping on purpose is not mistaken for a crash.
struct Quitting(AtomicBool);

/// The folder Aqua works in: `AQUA_WORKSPACE_ROOT` if set, otherwise `%LOCALAPPDATA%\Aqua`. The shell hands exactly this
/// to the core it starts, so both always look at the same data folder (the core would otherwise search upwards from
/// its own working directory for a source checkout and might settle on a different one).
fn workspace_root() -> Option<PathBuf> {
    if let Some(root) = std::env::var_os("AQUA_WORKSPACE_ROOT").filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(root));
    }
    std::env::var_os("LOCALAPPDATA").map(|l| PathBuf::from(l).join("Aqua"))
}

/// Where aqua-core keeps its data.
fn data_dir() -> Option<PathBuf> {
    workspace_root().map(|r| r.join("data"))
}

/// Append a line the core printed to `<root>\logs\core.log`, so an installed app (which has no console) leaves
/// something to read when the core does not start. The file is started afresh when it grows past 5 MB.
fn log_core(kind: &str, text: &str) {
    let Some(dir) = workspace_root().map(|r| r.join("logs")) else { return };
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("core.log");
    if std::fs::metadata(&path).map(|m| m.len() > 5 * 1024 * 1024).unwrap_or(false) {
        let _ = std::fs::rename(&path, dir.join("core.log.1"));
    }
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
        let _ = writeln!(f, "[{secs}] {kind}: {}", text.trim_end());
    }
}

/// The page asks for this once, then sends it as `Authorization: Bearer ...` on every /api request. It is an
/// error until the core has started and written its token file, and the page simply asks again.
#[tauri::command]
fn gateway_info() -> Result<GatewayInfo, String> {
    let dir = data_dir().ok_or("no data folder")?;
    let token = std::fs::read_to_string(dir.join(".aqua_ui_token")).map_err(|_| "the core has not written its token yet".to_string())?;
    let token = token.trim().to_string();
    if token.is_empty() {
        return Err("the core's token is empty".into());
    }
    Ok(GatewayInfo { base_url: GATEWAY.to_string(), ui_token: token })
}

use tauri::menu::{Menu, MenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};

/// Open a web address in the person's own browser. Only http and https, and nothing else (a webview that could be
/// told to open arbitrary paths or protocols would be a way to start programs).
#[tauri::command]
fn open_external(app: tauri::AppHandle, url: String) -> Result<(), String> {
    let lower = url.to_ascii_lowercase();
    if !(lower.starts_with("https://") || lower.starts_with("http://")) || url.len() > 2048 || url.chars().any(|c| c.is_control() || c == ' ') {
        return Err("only plain http and https addresses can be opened".into());
    }
    app.opener().open_url(url, None::<&str>).map_err(|e| e.to_string())
}

/// Put a window on top of the others. Windows does not let a background process take the foreground, so the window
/// is made always-on-top (which does raise it), and set back to normal a moment later so it does not stay pinned.
fn raise_window(w: &tauri::WebviewWindow) {
    let _ = w.unminimize();
    let _ = w.show();
    let _ = w.set_always_on_top(true);
    let _ = w.set_focus();
    let w = w.clone();
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(600));
        let _ = w.set_always_on_top(false);
    });
}

/// Bring the main window to the front. If it was closed (destroyed), build it again from tauri.conf.json.
fn show_main_window(app: &tauri::AppHandle) {
    if app.get_webview_window("main").is_none() {
        if let Some(cfg) = app.config().app.windows.iter().find(|w| w.label == "main") {
            let _ = tauri::WebviewWindowBuilder::from_config(app, cfg).and_then(|b| b.build());
        }
    }
    if let Some(w) = app.get_webview_window("main") {
        raise_window(&w);
    }
}

/// An approval is waiting for the person: bring the main window forward. The X button only hides it, so a scheduled run or
/// a long task could otherwise ask a question nobody can see, and it would time out and be denied.
#[tauri::command]
fn bring_to_front(app: tauri::AppHandle) {
    show_main_window(&app);
}

/// Right-click menu of the voice orb: Minimize (hide the orb; the tray icon brings it back), Open Panel, Quit.
#[tauri::command]
/// `stop` says what can be stopped right now: "thinking" or "speaking" adds that action at the top of the menu, and
/// nothing running (None) leaves it out.
fn orb_menu(window: tauri::WebviewWindow, stop: Option<String>) -> Result<(), String> {
    let minimize = MenuItem::with_id(&window, "orb_minimize", "Minimize", true, None::<&str>).map_err(|e| e.to_string())?;
    let open = MenuItem::with_id(&window, "orb_open", "Open Panel", true, None::<&str>).map_err(|e| e.to_string())?;
    let text = MenuItem::with_id(&window, "orb_text", "Task in Text", true, None::<&str>).map_err(|e| e.to_string())?;
    let end = MenuItem::with_id(&window, "orb_end", "End Conversation", true, None::<&str>).map_err(|e| e.to_string())?;
    let quit = MenuItem::with_id(&window, "orb_quit", "Quit", true, None::<&str>).map_err(|e| e.to_string())?;
    let stop_item = match stop.as_deref() {
        Some("speaking") => Some(MenuItem::with_id(&window, "orb_stop", "Stop Speaking", true, None::<&str>).map_err(|e| e.to_string())?),
        Some("thinking") => Some(MenuItem::with_id(&window, "orb_stop", "Stop Thinking", true, None::<&str>).map_err(|e| e.to_string())?),
        _ => None,
    };
    let mut items: Vec<&dyn tauri::menu::IsMenuItem<tauri::Wry>> = Vec::new();
    if let Some(item) = &stop_item {
        items.push(item);
    }
    items.extend([&minimize as &dyn tauri::menu::IsMenuItem<tauri::Wry>, &open, &text, &end, &quit]);
    let menu = Menu::with_items(&window, &items).map_err(|e| e.to_string())?;
    window.popup_menu(&menu).map_err(|e| e.to_string())
}

/// The orb window grows downward to make room for the text box or an answer, and shrinks back to just the orb.
#[tauri::command]
fn orb_resize(window: tauri::WebviewWindow, height: f64, editing: Option<bool>) -> Result<(), String> {
    let height = height.clamp(300.0, 700.0);
    window.set_size(tauri::LogicalSize::new(300.0, height)).map_err(|e| e.to_string())?;
    // Only the text box needs the keyboard. A reply that arrives while the person is working in another window must not take
    // the focus: their next keystrokes would land in the orb.
    if editing.unwrap_or(false) {
        let _ = window.set_focus();
    }
    // A taller window must not hang off the bottom of the screen; a shrunk one goes back to where it was.
    if let Some(p) = window.app_handle().try_state::<orb_place::Placement>() {
        p.orb_resized(editing.unwrap_or(false));
    }
    Ok(())
}

/// Stop the core and everything it started (the model server, the browser and desktop tool servers, the speech
/// daemon). Killing only the core's own process leaves those running on Windows, so the whole tree is ended.
fn stop_core(child: CommandChild) {
    let pid = child.pid();
    #[cfg(windows)]
    {
        // /T: the process and every process it started; /F: do not wait for them to agree.
        let _ = std::process::Command::new("taskkill").args(["/PID", &pid.to_string(), "/T", "/F"]).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).status();
    }
    let _ = child.kill();
    let _ = pid;
}

/// Start the core. It is told where the data folder is and which process started it (it stops when that process is
/// gone). If it stops by itself it is started again, up to three times with a growing pause; exit code 3 means another
/// Aqua already holds the port, which a restart cannot fix.
fn start_core(app: tauri::AppHandle, attempt: u32) {
    let spawned = app
        .shell()
        .sidecar("aqua-core")
        .map_err(|e| format!("could not prepare Aqua's core: {e}"))
        .and_then(|cmd| {
            let mut cmd = cmd.env("AQUA_PARENT_PID", std::process::id().to_string());
            if let Some(root) = workspace_root() {
                cmd = cmd.env("AQUA_WORKSPACE_ROOT", root);
            }
            cmd.spawn().map_err(|e| format!("could not start Aqua's core: {e}"))
        });
    match spawned {
        Ok((mut rx, child)) => {
            *app.state::<Core>().0.lock().unwrap_or_else(|p| p.into_inner()) = Some(child);
            log_core("shell", &format!("core started (attempt {})", attempt + 1));
            tauri::async_runtime::spawn(async move {
                while let Some(event) = rx.recv().await {
                    match event {
                        CommandEvent::Stdout(line) => log_core("out", &String::from_utf8_lossy(&line)),
                        CommandEvent::Stderr(line) => log_core("err", &String::from_utf8_lossy(&line)),
                        CommandEvent::Error(e) => log_core("shell", &format!("core error: {e}")),
                        CommandEvent::Terminated(p) => {
                            log_core("shell", &format!("core exited (code {:?})", p.code));
                            if app.state::<Quitting>().0.load(Ordering::SeqCst) || p.code == Some(3) || attempt >= 3 {
                                break;
                            }
                            tokio_sleep(2 << attempt).await;
                            if !app.state::<Quitting>().0.load(Ordering::SeqCst) {
                                start_core(app.clone(), attempt + 1);
                            }
                            break;
                        }
                        _ => {}
                    }
                }
            });
        }
        Err(e) => {
            log_core("shell", &e);
            eprintln!("{e}");
        }
    }
}

async fn tokio_sleep(secs: u64) {
    tauri::async_runtime::spawn_blocking(move || std::thread::sleep(std::time::Duration::from_secs(secs))).await.ok();
}

fn main() {
    let app = tauri::Builder::default()
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .manage(Core(Mutex::new(None)))
        .manage(Quitting(AtomicBool::new(false)))
        .on_window_event(|window, event| {
            // The X on the main window hides it instead of destroying it: the orb and the tray keep the app running,
            // and "Open Panel" / the tray icon need a window to bring back. Quit lives in the orb menu and the tray.
            if window.label() == "main" {
                if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                    api.prevent_close();
                    let _ = window.hide();
                }
            }
        })
        .on_menu_event(|app, event| match event.id().as_ref() {
            "orb_stop" => {
                if let Some(w) = app.get_webview_window("voice-overlay") {
                    let _ = w.eval("window.__orbStop && window.__orbStop()");
                }
            }
            "orb_end" => {
                if let Some(w) = app.get_webview_window("voice-overlay") {
                    let _ = w.eval("window.__orbEnd && window.__orbEnd()");
                }
            }
            "orb_minimize" => {
                if let Some(w) = app.get_webview_window("voice-overlay") {
                    // A hidden orb must not keep the microphone open: this ends any conversation and pauses the wake word.
                    let _ = w.eval("window.__orbMinimized && window.__orbMinimized()");
                    let _ = w.hide();
                }
            }
            "orb_open" => show_main_window(app),
            "orb_text" => {
                if let Some(w) = app.get_webview_window("voice-overlay") {
                    let _ = w.show();
                    if let Some(p) = app.try_state::<orb_place::Placement>() {
                        p.orb_shown();
                    }
                    let _ = w.eval("window.__orbText && window.__orbText()");
                }
            }
            "orb_quit" => app.exit(0),
            _ => {}
        })
        .invoke_handler(tauri::generate_handler![gateway_info, open_external, orb_menu, orb_resize, bring_to_front, orb_place::orb_place_state, orb_place::orb_place_set, orb_place::orb_place_move, orb_place::orb_place_forget, orb_place::orb_place_recheck, orb_place::orb_hit_regions, updates::update_status, updates::check_for_update, updates::install_update, updates::update_progress])
        .setup(|app| {
            // The app icon is dark navy and disappears on Windows' dark tray, so the tray uses a light-backed copy
            // (32x32 raw RGBA, see icons/tray.png). We decode the PNG so Tauri handles the RGBA/BGRA conversion.
            let icon = tauri::image::Image::from_bytes(include_bytes!("../icons/tray.png")).unwrap();
            let quit = MenuItem::with_id(app, "quit", "Quit Aqua", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&quit])?;

            let tray = TrayIconBuilder::new()
                .icon(icon)
                .tooltip("Aqua Assistant")
                .menu(&menu)
                .show_menu_on_left_click(false)
                .on_menu_event(|app, event| {
                    if event.id().as_ref() == "quit" {
                        app.exit(0);
                    }
                })
                .on_tray_icon_event(|tray, event| match event {
                    TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. } => {
                        let app = tray.app_handle();
                        // The orb only; the admin panel opens from the orb's right-click menu ("Open Panel").
                        if let Some(overlay) = app.get_webview_window("voice-overlay") {
                            let _ = overlay.show();
                            let _ = overlay.eval("window.__orbShow && window.__orbShow()");
                            if let Some(p) = app.try_state::<orb_place::Placement>() {
                                p.orb_shown();
                            }
                        }
                    }
                    _ => {}
                })
                .build(app)
                .expect("Failed to build tray icon");
            
            // We MUST store the tray icon in the app state, otherwise Rust drops and destroys it immediately!
            // (Nothing reads it: holding it is the point.)
            #[allow(dead_code)]
            struct TrayState(Mutex<Option<tauri::tray::TrayIcon>>);
            app.manage(TrayState(Mutex::new(Some(tray))));
            // Where the orb sits, and moving it out of the way of what is on screen (see orb_place).
            app.manage(orb_place::Placement::start(app.handle()));
            // Bring the admin panel to the front once the windows have finished opening (a raise during setup is too early).
            let handle = app.handle().clone();
            std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_millis(2500));
                show_main_window(&handle);
                // The orb opens with the app. A start state handed down by whoever launched the app (hidden or
                // minimized) can leave a window hidden, so it is shown explicitly once everything is open.
                if let Some(orb) = handle.get_webview_window("voice-overlay") {
                    // An empty title keeps Windows from drawing a "Voice Assistant" tooltip across the top of the orb.
                    let _ = orb.set_title("");
                    let _ = orb.show();
                }
            });

            // Launch the core automatically; if it fails or stops, the shell says so in the log and starts it again.
            start_core(app.handle().clone(), 0);
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("error while building the Aqua desktop app");

    app.run(|handle, event| {
        if let RunEvent::Exit = event {
            handle.state::<Quitting>().0.store(true, Ordering::SeqCst);
            if let Some(child) = handle.state::<Core>().0.lock().unwrap_or_else(|p| p.into_inner()).take() {
                stop_core(child);
            }
        }
    });
}
