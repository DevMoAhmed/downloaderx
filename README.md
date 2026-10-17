# DownloaderX

Open-source, multi-connection download manager in Rust, with a Chrome extension.

## Features

- **Fast downloads**: splits files into parts and downloads them at the same time (8 connections by default); a connection that finishes early takes over half of the slowest part
- **Pause / resume**: survives app restarts and lost connections; `If-Range` protects against files that changed on the server
- **Queue**: limit how many run at once, set priorities (▲▼), schedule a start time, Start all / Stop all
- **Speed limit**: one shared limit for all downloads, can be changed while downloading
- **Categories**: sorts files into Video / Music / Documents / Compressed / Programs / Images
- **SHA-256 check**: paste the expected hash and the app checks the file when it finishes
- **Stream videos (HLS .m3u8)**: pick the quality, AES-128 decryption, joined into .mp4 with ffmpeg
- **Site videos (YouTube etc.)**: through [yt-dlp](https://github.com/yt-dlp/yt-dlp), with a quality list
- **Dashboard** at http://127.0.0.1:6801: shows each connection's progress, speed and time left; paste a link anywhere to add it

## Chrome extension

- **Floating Download button** on every video → quality menu (720p, 360p, audio only, …)
- Finds media on the tab (badge shows count), listed in the popup
- Right-click → *Download with DownloaderX* / *Download video from this page*
- Takes over normal browser downloads (can be turned off in the popup; small files stay in the browser)
- Sends cookies + referer so downloads that need a login still work

DRM-protected streams (Netflix, Disney+, …) are not supported.

## Install (Windows)

1. Run `downloaderx.exe`. The dashboard opens in your browser.
2. Chrome → `chrome://extensions` → turn on **Developer mode** → **Load unpacked** → pick the `extension` folder.
3. Optional: install `yt-dlp` and `ffmpeg` (e.g. `winget install yt-dlp ffmpeg`) for YouTube-style sites and .mp4 output.

## Command line

```
downloaderx                      # run app + dashboard (same as `serve`)
downloaderx serve --port 6801 --no-open
downloaderx get <URL> [-o FILE] [-c 16] [-l 500]   # one file, -l = KiB/s limit
```

## Build

```
cargo build --release
cargo build --release --target x86_64-pc-windows-gnu   # Windows exe from Linux (needs mingw-w64)
```

## Security

The API listens only on 127.0.0.1. It accepts only `Host: 127.0.0.1/localhost`, needs an `x-downloaderx` header, and allows CORS only from browser extensions, so websites can't add downloads.
