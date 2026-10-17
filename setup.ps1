# One-time setup on Windows. Run in PowerShell from this folder:
#   powershell -ExecutionPolicy Bypass -File setup.ps1
$ErrorActionPreference = "Stop"
Set-Location $PSScriptRoot

Write-Host "Installing tools (git, GitHub CLI, yt-dlp, ffmpeg, Rust)..."
foreach ($id in "Git.Git", "GitHub.cli", "yt-dlp.yt-dlp", "Gyan.FFmpeg", "Rustlang.Rustup") {
    winget install --id $id -e --silent --accept-source-agreements --accept-package-agreements
}
$env:Path = [Environment]::GetEnvironmentVariable("Path", "Machine") + ";" + [Environment]::GetEnvironmentVariable("Path", "User")

Write-Host "Log in to your devMoahmed GitHub account in the browser window..."
gh auth login --web --git-protocol https

if (-not (Test-Path .git)) {
    git init -b main
    git add -A
    git commit -m "DownloaderX: download manager + Chrome extension"
}
gh repo create downloaderx --private --source . --push

Write-Host "Building..."
cargo build --release

Write-Host "Done. Starting Claude Code here..."
claude remote-control
