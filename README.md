# DownloaderX

Open-source, multi-connection download manager for Windows, written in Rust, with a Chrome/Edge extension.

## Features

- **Fast downloads**: splits files into parts and downloads them at the same time (8 connections by default); a connection that finishes early takes over half of the slowest part
- **Pause / resume**: survives app restarts and lost connections; `If-Range` protects against files that changed on the server
- **Queue**: limit how many run at once, set priorities (▲▼), schedule a start time, Start all / Stop all
- **Speed limit**: one shared limit for all downloads, can be changed while downloading
- **Categories**: sorts files into Video / Music / Documents / Compressed / Programs / Images
- **SHA-256 check**: paste the expected hash and the app checks the file when it finishes
- **Stream videos (HLS .m3u8)**: pick the quality, AES-128 decryption, joined into .mp4 with ffmpeg
- **Site videos (YouTube etc.)**: through [yt-dlp](https://github.com/yt-dlp/yt-dlp), with a quality list
- **Desktop app**: a game-style download-manager window (kraft, cream and ink palette, chunky block toolbar) with toolbar, categories (All / Unfinished / Finished / Queues), sortable list with real Windows file icons, right-click menu, progress window per download, Options, Scheduler and a "Download complete" notice
- **Runs in the tray**: closing the window keeps it running so the browser extension still works; starting it again brings the window back
- **Paste or drag a link** anywhere in the window to add it; batch add from a list of links

## Chrome extension

- **Floating Download button** above every video → quality menu (720p, 360p, audio only, …). Drag it anywhere (remembered per site); its × hides it on this video, on this site, or everywhere (turn it back on in the popup)
- Finds every video on the tab, including players inside iframes or shadow DOM, HLS (.m3u8) and DASH (.mpd) streams, and sites like YouTube that build the stream in the page. The badge shows the count, the popup lists them
- Right-click → *Download with DownloaderX* / *Download video from this page*
- Takes over normal browser downloads (can be turned off in the popup; small files stay in the browser)
- Sends cookies + referer so downloads that need a login still work

DRM-protected streams (Netflix, Disney+, …) are not supported.

## Install (Windows 10/11)

1. Run `DownloaderX-Setup-<version>.exe` and follow the wizard. It can add a desktop icon, start DownloaderX with Windows, and install yt-dlp + ffmpeg for you.
2. Chrome or Edge → `chrome://extensions` → turn on **Developer mode** → **Load unpacked** → pick the `extension` folder inside the install folder (Start menu → *DownloaderX browser extension folder*).

Uninstall from Windows Settings → Apps. Your download list and settings in `%APPDATA%\downloaderx` are kept.

## Command line

```
downloaderx                      # desktop app (window + tray)
downloaderx --hidden             # start in the tray only (used for autostart)
downloaderx serve --port 6801 --no-open   # no window: web dashboard at http://127.0.0.1:6801
downloaderx get <URL> [-o FILE] [-c 16] [-l 500]   # one file, -l = KiB/s limit
```

## Build

Needs Rust (MSVC toolchain) and, for the installer, [Inno Setup 6](https://jrsoftware.org/isinfo.php) (`winget install JRSoftware.InnoSetup`).

```
cargo build --release
"%LOCALAPPDATA%\Programs\Inno Setup 6\ISCC.exe" installer\downloaderx.iss   # -> dist\DownloaderX-Setup-<version>.exe
```

The window is the same page as the web dashboard, shown in WebView2 (built into Windows 10/11).

## Security

The API listens only on 127.0.0.1. It accepts only `Host: 127.0.0.1/localhost`, needs an `x-downloaderx` header, and allows CORS only from browser extensions, so websites can't add downloads. Opening files, opening folders, autostart and exit are not in the HTTP API at all: only the app's own window can do them.

## Fonts

The window uses Titan One and Nunito, bundled in `assets/fonts` under the SIL Open Font License 1.1 (see `assets/fonts/LICENSE.txt`).
