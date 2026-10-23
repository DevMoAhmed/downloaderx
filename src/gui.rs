//! Desktop window (WebView2) and tray icon around the local dashboard.
//!
//! Closing the window only hides it: the app keeps running in the tray so the browser
//! extension can still hand over downloads. Things a web page must not be able to do
//! (open files, exit, autostart) go through WebView IPC, which only this window has.

use std::path::PathBuf;
use std::sync::Arc;

use serde::Deserialize;
use serde_json::{Value, json};
use tao::dpi::LogicalSize;
use tao::event::{Event, StartCause, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoopBuilder};
use tao::platform::windows::WindowBuilderExtWindows;
use tao::window::{Window, WindowBuilder};
use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use wry::{NewWindowResponse, WebContext, WebView, WebViewBuilder};

use crate::queue::{Manager, Status};
use crate::win;

enum UserEvent {
    Show(Option<String>),
    Tray(TrayIconEvent),
    Menu(MenuEvent),
    Ipc(String),
}

#[derive(Deserialize)]
struct Ipc {
    /// Callback id to answer to; absent when no answer is needed.
    cb: Option<u64>,
    cmd: String,
    id: Option<u64>,
    url: Option<String>,
    on: Option<bool>,
    browser: Option<String>,
}

fn rgba(png_bytes: &[u8]) -> (Vec<u8>, u32, u32) {
    let mut dec = png::Decoder::new(std::io::Cursor::new(png_bytes)).read_info().expect("embedded icon");
    let mut buf = vec![0; dec.output_buffer_size().expect("icon size")];
    let info = dec.next_frame(&mut buf).expect("embedded icon");
    buf.truncate(info.buffer_size());
    (buf, info.width, info.height)
}

/// The app icon at `size` px from the exe's icon resource (falls back to the PNG).
fn app_icon(size: u32) -> Option<tao::window::Icon> {
    use tao::platform::windows::IconExtWindows;
    tao::window::Icon::from_resource(1, Some(tao::dpi::PhysicalSize::new(size, size))).ok().or_else(|| {
        let (px, w, h) = rgba(include_bytes!("../assets/icon-256.png"));
        tao::window::Icon::from_rgba(px, w, h).ok()
    })
}

fn show(window: &Window) {
    use tao::platform::windows::WindowExtWindows;
    window.set_visible(true);
    window.set_minimized(false);
    window.set_focus();
    // Windows keeps a background app behind the browser; take the front anyway.
    win::bring_to_front(window.hwnd());
}

pub fn run(mgr: Arc<Manager>, port: u16, hidden: bool, panel: Option<&str>) -> anyhow::Result<()> {
    let event_loop = EventLoopBuilder::<UserEvent>::with_user_event().build();
    let proxy = event_loop.create_proxy();
    {
        let p = proxy.clone();
        let _ = mgr.show_window.set(Box::new(move |panel| {
            p.send_event(UserEvent::Show(panel)).ok();
        }));
    }

    // Both window icons come from the .ico inside the exe at the sizes Windows asks for, so
    // the hand-drawn 16 px is used and the taskbar never falls back to a cached old icon.
    let (small, big) = win::icon_sizes();
    let window = WindowBuilder::new()
        .with_title("DownloaderX")
        .with_inner_size(LogicalSize::new(1180.0, 700.0))
        .with_min_inner_size(LogicalSize::new(640.0, 360.0))
        .with_window_icon(app_icon(small))
        .with_taskbar_icon(app_icon(big))
        .with_visible(!hidden)
        .build(&event_loop)?;

    // WebView2 keeps its profile here; the install folder isn't writable.
    let data = dirs::data_local_dir().unwrap_or_else(|| ".".into()).join("DownloaderX").join("WebView2");
    let mut ctx = WebContext::new(Some(data));
    let base = format!("http://127.0.0.1:{port}/");
    let webview = {
        let p = proxy.clone();
        let nav_base = base.clone();
        WebViewBuilder::new_with_web_context(&mut ctx)
            .with_url(&base)
            .with_ipc_handler(move |req| {
                p.send_event(UserEvent::Ipc(req.into_body())).ok();
            })
            // Stay on the dashboard; anything else opens in the normal browser.
            .with_navigation_handler(move |url| {
                if url.starts_with(&nav_base) || url.starts_with("about:") {
                    return true;
                }
                if url.starts_with("http://") || url.starts_with("https://") {
                    win::shell_open(&url);
                }
                false
            })
            .with_new_window_req_handler(|url, _| {
                if url.starts_with("http://") || url.starts_with("https://") {
                    win::shell_open(&url);
                }
                NewWindowResponse::Deny
            })
            .with_devtools(cfg!(debug_assertions))
            .build(&window)?
    };

    let tray_icon = match tray_icon::Icon::from_resource(1, Some((small, small))) {
        Ok(i) => i,
        Err(_) => {
            let (px, w, h) = rgba(include_bytes!("../assets/icon-32.png"));
            tray_icon::Icon::from_rgba(px, w, h)?
        }
    };
    let menu = Menu::new();
    let open_i = MenuItem::new("Open DownloaderX", true, None);
    let start_i = MenuItem::new("Start all", true, None);
    let stop_i = MenuItem::new("Stop all", true, None);
    let exit_i = MenuItem::new("Exit", true, None);
    menu.append_items(&[&open_i, &PredefinedMenuItem::separator(), &start_i, &stop_i, &PredefinedMenuItem::separator(), &exit_i])?;
    {
        let p = proxy.clone();
        TrayIconEvent::set_event_handler(Some(move |e| {
            p.send_event(UserEvent::Tray(e)).ok();
        }));
        let p = proxy.clone();
        MenuEvent::set_event_handler(Some(move |e| {
            p.send_event(UserEvent::Menu(e)).ok();
        }));
    }

    // A panel to open once the page has loaded (from --setup-extension or an early /show).
    let mut pending: Option<String> = panel.map(String::from).or_else(|| mgr.pending_panel.lock().unwrap().take());
    let mut parts = Some((menu, tray_icon));
    let mut tray = None;
    event_loop.run(move |event, _, flow| {
        let _keep_alive = &ctx;
        *flow = ControlFlow::Wait;
        match event {
            // The tray must be created once the event loop is running.
            Event::NewEvents(StartCause::Init) => {
                if let Some((menu, icon)) = parts.take() {
                    tray = TrayIconBuilder::new().with_menu(Box::new(menu)).with_tooltip("DownloaderX").with_icon(icon).build().ok();
                }
            }
            Event::WindowEvent { event: WindowEvent::CloseRequested, .. } => window.set_visible(false),
            Event::UserEvent(UserEvent::Show(panel)) => {
                show(&window);
                if let Some(p) = panel {
                    // Opens it now if the page is ready; otherwise the page collects it on load.
                    let js = format!("window.__showPanel && window.__showPanel({})", serde_json::to_string(&p).unwrap_or_default());
                    pending = Some(p);
                    webview.evaluate_script(&js).ok();
                }
            }
            Event::UserEvent(UserEvent::Tray(TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. })) => {
                show(&window)
            }
            Event::UserEvent(UserEvent::Menu(e)) => {
                if e.id == *open_i.id() {
                    show(&window);
                } else if e.id == *start_i.id() {
                    mgr.all("resume");
                } else if e.id == *stop_i.id() {
                    mgr.all("pause");
                } else if e.id == *exit_i.id() {
                    mgr.save();
                    tray.take();
                    *flow = ControlFlow::Exit;
                }
            }
            Event::UserEvent(UserEvent::Ipc(msg)) => {
                let Ok(m) = serde_json::from_str::<Ipc>(&msg) else { return };
                if m.cmd == "take_panel" {
                    if let Some(cb) = m.cb {
                        reply(&webview, cb, json!(pending.take()));
                    }
                    return;
                }
                if m.cmd == "quit" {
                    mgr.save();
                    tray.take();
                    *flow = ControlFlow::Exit;
                    return;
                }
                let answer = ipc(&mgr, &window, &m);
                if let Some(cb) = m.cb {
                    reply(&webview, cb, answer);
                }
            }
            _ => {}
        }
    })
}

fn reply(webview: &WebView, cb: u64, value: Value) {
    webview.evaluate_script(&format!("window.__ipcReply && window.__ipcReply({cb}, {value})")).ok();
}

fn ipc(mgr: &Manager, window: &Window, m: &Ipc) -> Value {
    let item = || m.id.and_then(|id| mgr.items().into_iter().find(|i| i.id == id));
    match m.cmd.as_str() {
        "hide" => window.set_visible(false),
        "open" => {
            // Only finished files: never run something half-downloaded.
            if let Some(p) = item().filter(|i| i.status == Status::Done).and_then(|i| i.path) {
                return json!(win::shell_open(&p));
            }
            return json!(false);
        }
        "folder" => {
            match item().and_then(|i| i.path) {
                Some(p) => win::reveal(&p),
                None => {
                    let dir = mgr.settings().download_dir;
                    std::fs::create_dir_all(&dir).ok();
                    win::shell_open(&dir);
                }
            }
        }
        "url" => {
            if let Some(u) = m.url.as_deref().filter(|u| u.starts_with("https://") || u.starts_with("http://")) {
                win::shell_open(u);
            }
        }
        // Guided "Load unpacked": copy the folder path, open the browser's extensions page.
        "ext_open" => {
            let dir = extension_dir().display().to_string();
            let copied = win::set_clipboard(&dir);
            let (exe, page) = match m.browser.as_deref() {
                Some("edge") => ("msedge.exe", "edge://extensions"),
                _ => ("chrome.exe", "chrome://extensions"),
            };
            return json!({ "path": dir, "copied": copied, "opened": win::launch(exe, page) });
        }
        "extension_path" => return json!(extension_dir().display().to_string()),
        "ext_copy" => {
            let dir = extension_dir().display().to_string();
            return json!({ "path": dir.clone(), "copied": win::set_clipboard(&dir) });
        }
        "extension_folder" => {
            let dir = extension_dir();
            win::shell_open(&dir);
            return json!(dir.display().to_string());
        }
        "autostart" => {
            if let Some(on) = m.on {
                win::set_autostart(on);
            }
            return json!(win::autostart_enabled());
        }
        _ => return Value::Null,
    }
    Value::Null
}

/// The installer puts the Chrome extension next to the exe; in a dev build it's in the repo.
fn extension_dir() -> PathBuf {
    let exe = std::env::current_exe().unwrap_or_default();
    let mut dir = exe.parent().map(|d| d.to_path_buf()).unwrap_or_default();
    loop {
        if dir.join("extension").join("manifest.json").exists() {
            return dir.join("extension");
        }
        if !dir.pop() {
            return exe.parent().map(|d| d.join("extension")).unwrap_or_default();
        }
    }
}
