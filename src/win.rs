//! Small Win32 helpers: shell icons, opening files, autostart, console for CLI use.

use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::process::CommandExt;
use std::path::Path;
use std::process::Command;

use windows_sys::Win32::Graphics::Gdi::{
    BI_RGB, BITMAP, BITMAPINFO, BITMAPINFOHEADER, DIB_RGB_COLORS, DeleteObject, GetDC, GetDIBits, GetObjectW, ReleaseDC,
};
use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_NORMAL;
use windows_sys::Win32::System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx};
use windows_sys::Win32::System::Console::{ATTACH_PARENT_PROCESS, AttachConsole};
use windows_sys::Win32::System::Registry::{HKEY, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ, RegGetValueW};
use windows_sys::Win32::UI::Shell::{
    SHFILEINFOW, SHGFI_ICON, SHGFI_LARGEICON, SHGFI_USEFILEATTRIBUTES, SHGetFileInfoW, ShellExecuteW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{DestroyIcon, GetIconInfo, ICONINFO, SW_SHOWNORMAL};

pub const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const RUN_KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";

fn wide(s: impl AsRef<OsStr>) -> Vec<u16> {
    s.as_ref().encode_wide().chain(Some(0)).collect()
}

/// The GUI exe has no console; `downloaderx get …` from a terminal still prints there.
pub fn attach_console() {
    unsafe { AttachConsole(ATTACH_PARENT_PROCESS) };
}

/// Add PATH entries from the registry that this process didn't inherit, e.g. yt-dlp and
/// ffmpeg that the installer just put in with winget. Call before any threads start.
pub fn refresh_path() {
    let read = |root: HKEY, key: &str| -> Option<String> {
        let (k, v) = (wide(key), wide("Path"));
        let mut len = 0u32;
        // RRF_RT_REG_SZ also accepts REG_EXPAND_SZ and expands %VARS%.
        let get = |buf: *mut u16, len: &mut u32| unsafe {
            RegGetValueW(root, k.as_ptr(), v.as_ptr(), RRF_RT_REG_SZ, std::ptr::null_mut(), buf.cast(), len)
        };
        if get(std::ptr::null_mut(), &mut len) != 0 {
            return None;
        }
        let mut buf = vec![0u16; len as usize / 2 + 1];
        if get(buf.as_mut_ptr(), &mut len) != 0 {
            return None;
        }
        buf.truncate(len as usize / 2);
        Some(String::from_utf16_lossy(&buf).trim_end_matches(char::from(0)).to_string())
    };
    let current = std::env::var("PATH").unwrap_or_default();
    let mut parts: Vec<String> = current.split(';').filter(|p| !p.is_empty()).map(String::from).collect();
    let machine = read(HKEY_LOCAL_MACHINE, r"SYSTEM\CurrentControlSet\Control\Session Manager\Environment");
    let user = read(HKEY_CURRENT_USER, "Environment");
    for p in machine.iter().chain(user.iter()).flat_map(|s| s.split(';')) {
        if !p.is_empty() && !parts.iter().any(|x| x.eq_ignore_ascii_case(p)) {
            parts.push(p.to_string());
        }
    }
    // SAFETY: called at the very start of main, before other threads exist.
    unsafe { std::env::set_var("PATH", parts.join(";")) };
}

/// Open a file with its default app, or a URL in the default browser.
pub fn shell_open(target: impl AsRef<OsStr>) -> bool {
    let (op, t) = (wide("open"), wide(target));
    let r = unsafe { ShellExecuteW(std::ptr::null_mut(), op.as_ptr(), t.as_ptr(), std::ptr::null(), std::ptr::null(), SW_SHOWNORMAL) };
    r as usize > 32
}

/// Open Explorer with the file selected (or the folder, if the file is gone).
pub fn reveal(path: &Path) {
    let mut c = Command::new("explorer");
    if path.exists() {
        c.raw_arg(format!("/select,\"{}\"", path.display()));
    } else if let Some(dir) = path.parent() {
        c.arg(dir);
    }
    c.spawn().ok();
}

pub fn autostart_enabled() -> bool {
    Command::new("reg")
        .args(["query", RUN_KEY, "/v", "DownloaderX"])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .is_ok_and(|o| o.status.success())
}

pub fn set_autostart(on: bool) -> bool {
    let mut c = Command::new("reg");
    if on {
        let exe = std::env::current_exe().unwrap_or_default();
        c.args(["add", RUN_KEY, "/v", "DownloaderX", "/t", "REG_SZ", "/f", "/d"]).arg(format!("\"{}\" --hidden", exe.display()));
    } else {
        c.args(["delete", RUN_KEY, "/v", "DownloaderX", "/f"]);
    }
    c.creation_flags(CREATE_NO_WINDOW).output().is_ok_and(|o| o.status.success())
}

/// 32x32 shell icon as PNG: for an existing `file` its own icon, otherwise the icon for `ext`.
pub fn shell_icon(file: Option<&Path>, ext: &str) -> Option<Vec<u8>> {
    unsafe { CoInitializeEx(std::ptr::null(), COINIT_APARTMENTTHREADED as u32) };
    let mut info: SHFILEINFOW = unsafe { std::mem::zeroed() };
    let (name, flags) = match file {
        Some(p) => (wide(p), SHGFI_ICON | SHGFI_LARGEICON),
        None => (wide(format!("file.{}", if ext.is_empty() { "bin" } else { ext })), SHGFI_ICON | SHGFI_LARGEICON | SHGFI_USEFILEATTRIBUTES),
    };
    let ok = unsafe { SHGetFileInfoW(name.as_ptr(), FILE_ATTRIBUTE_NORMAL, &mut info, size_of::<SHFILEINFOW>() as u32, flags) };
    if ok == 0 || info.hIcon.is_null() {
        return None;
    }
    let rgba = unsafe { icon_rgba(info.hIcon) };
    unsafe { DestroyIcon(info.hIcon) };
    let (w, h, px) = rgba?;
    let mut out = Vec::new();
    let mut enc = png::Encoder::new(&mut out, w, h);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    enc.write_header().ok()?.write_image_data(&px).ok()?;
    Some(out)
}

unsafe fn icon_rgba(icon: windows_sys::Win32::UI::WindowsAndMessaging::HICON) -> Option<(u32, u32, Vec<u8>)> {
    unsafe {
        let mut ii: ICONINFO = std::mem::zeroed();
        if GetIconInfo(icon, &mut ii) == 0 {
            return None;
        }
        let result = (|| {
            let mut bm: BITMAP = std::mem::zeroed();
            if GetObjectW(ii.hbmColor, size_of::<BITMAP>() as i32, &mut bm as *mut _ as *mut _) == 0 {
                return None;
            }
            let (w, h) = (bm.bmWidth, bm.bmHeight);
            let read = |bmp| {
                let mut bi: BITMAPINFO = std::mem::zeroed();
                bi.bmiHeader = BITMAPINFOHEADER {
                    biSize: size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: w,
                    biHeight: -h, // top-down rows
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB,
                    ..std::mem::zeroed()
                };
                let mut buf = vec![0u8; (w * h * 4) as usize];
                let dc = GetDC(std::ptr::null_mut());
                let n = GetDIBits(dc, bmp, 0, h as u32, buf.as_mut_ptr().cast(), &mut bi, DIB_RGB_COLORS);
                ReleaseDC(std::ptr::null_mut(), dc);
                (n != 0).then_some(buf)
            };
            let mut px = read(ii.hbmColor)?;
            // Old-style icons have no alpha channel: take transparency from the mask.
            if px.chunks(4).all(|p| p[3] == 0) {
                let mask = read(ii.hbmMask)?;
                for (p, m) in px.chunks_mut(4).zip(mask.chunks(4)) {
                    p[3] = if m[0] == 0 { 255 } else { 0 };
                }
            }
            for p in px.chunks_mut(4) {
                p.swap(0, 2); // BGRA -> RGBA
            }
            Some((w as u32, h as u32, px))
        })();
        DeleteObject(ii.hbmColor);
        DeleteObject(ii.hbmMask);
        result
    }
}
