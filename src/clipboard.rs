//! Read an image out of the OS clipboard, cross-platform.
//!
//! Terminal paste events (`Event::Paste`) deliver *text* only — the terminal
//! emulator never ships the bytes of an image on paste. To get an image the
//! user has copied (or a screenshot they took) we have to leave the process
//! and ask the OS's clipboard directly:
//!
//! * **macOS** — `osascript`, writing `the clipboard as «class PNGf»` to a
//!   temp file. This is the same call `pngpaste` wraps, without the extra
//!   install; the AppleScript throws (non-zero exit) when the clipboard holds
//!   no image, which is exactly how we detect "nothing to paste".
//! * **Windows** — PowerShell with `System.Windows.Forms.Clipboard::GetImage()`,
//!   saved as PNG. `powershell.exe` ships with every supported Windows.
//! * **Linux** — try `wl-paste --type image/png` (Wayland) then
//!   `xclip -selection clipboard -t image/png -o` (X11). Both write the raw
//!   bytes to stdout and fail (or emit nothing) when the clipboard has no
//!   image.
//!
//! Each backend is a synchronous subprocess run on the event-loop thread. That
//! is a deliberate trade-off: a clipboard read is tens of milliseconds, and
//! the app has no async plumbing in `handle_key` to await one without a much
//! larger refactor. The one theoretical hazard is a Wayland compositor that
//! makes `wl-paste` block indefinitely waiting for a `--type` the clipboard
//! cannot supply; in practice `wl-paste --type image/png` returns immediately
//! with an error when no image is present.

use crate::llm::ImageAttachment;
use std::process::Command;

/// The image currently in the clipboard, base64-encoded as PNG, or `None` if
/// the clipboard holds no image or the platform's clipboard tool is missing.
pub fn read_image() -> Option<ImageAttachment> {
    #[cfg(target_os = "macos")]
    {
        return read_macos();
    }
    #[cfg(target_os = "windows")]
    {
        return read_windows();
    }
    #[cfg(target_os = "linux")]
    {
        return read_linux();
    }
    // Other targets have no supported clipboard tool.
    #[allow(unreachable_code)]
    None
}

/// A fresh temp path for the current process, so two running instances (or a
/// stale file from a crashed earlier run) never collide. We delete it after
/// reading; leaving a stray is harmless since it lives in the OS temp dir.
#[cfg(any(target_os = "macos", target_os = "windows"))]
fn scratch_path(ext: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!("boxcode_clip_{}.{}", std::process::id(), ext))
}

#[cfg(target_os = "macos")]
fn read_macos() -> Option<ImageAttachment> {
    let path = scratch_path("png");
    let script = format!(
        "set pngData to the clipboard as «class PNGf»\n\
         set f to open for access POSIX file \"{}\" with write permission\n\
         set eof f to 0\n\
         write pngData to f\n\
         close access f",
        path.display()
    );
    let ok = Command::new("osascript")
        .arg("-e")
        .arg(script)
        .status()
        .ok()?
        .success();
    if !ok {
        return None;
    }
    read_scratch_and_drop(&path, "image/png")
}

#[cfg(target_os = "windows")]
fn read_windows() -> Option<ImageAttachment> {
    let path = scratch_path("png");
    let script = format!(
        "Add-Type -AssemblyName System.Windows.Forms,System.Drawing; \
         $img = [System.Windows.Forms.Clipboard]::GetImage(); \
         if ($img) {{ $img.Save('{}', [System.Drawing.Imaging.ImageFormat]::Png); exit 0 }} else {{ exit 1 }}",
        path.display()
    );
    let ok = Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", &script])
        .status()
        .ok()?
        .success();
    if !ok {
        return None;
    }
    read_scratch_and_drop(&path, "image/png")
}

#[cfg(target_os = "linux")]
fn read_linux() -> Option<ImageAttachment> {
    for (tool, args) in [
        ("wl-paste", vec!["--type", "image/png"]),
        ("xclip", vec!["-selection", "clipboard", "-t", "image/png", "-o"]),
    ] {
        if let Ok(out) = Command::new(tool).args(args).output() {
            if out.status.success() && !out.stdout.is_empty() {
                return Some(ImageAttachment {
                    mime_type: "image/png".to_string(),
                    data_base64: crate::backend::base64_encode(&out.stdout),
                });
            }
        }
    }
    None
}

#[cfg(any(target_os = "macos", target_os = "windows"))]
fn read_scratch_and_drop(path: &std::path::Path, mime: &str) -> Option<ImageAttachment> {
    let bytes = std::fs::read(path).ok()?;
    let _ = std::fs::remove_file(path);
    if bytes.is_empty() {
        return None;
    }
    Some(ImageAttachment {
        mime_type: mime.to_string(),
        data_base64: crate::backend::base64_encode(&bytes),
    })
}
