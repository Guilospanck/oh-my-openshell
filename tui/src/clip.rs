//! Host clipboard images into a sandbox: read a PNG from the host clipboard, upload it, and
//! hand back the path inside the sandbox.

use crate::cli;
use std::{
    fs,
    io::Write,
    os::unix::fs::{DirBuilderExt, OpenOptionsExt},
    path::PathBuf,
    process::{Command, Stdio},
    time::{SystemTime, UNIX_EPOCH},
};

/// Larger clipboard images are not sent.
const MAX_IMAGE: usize = 20 * 1024 * 1024;
const PNG_MAGIC: &[u8] = b"\x89PNG\r\n\x1a\n";
/// Where pasted images land, under the sandbox user's HOME (the only place uploads are allowed).
pub const SANDBOX_DIR: &str = ".osh-paste";

/// The host clipboard's tools, tried in order: `pngpaste` (macOS), `wl-paste` (Wayland), `xclip` (X11).
const READERS: &[(&str, &[&str])] = &[
    ("pngpaste", &["-"]),
    ("wl-paste", &["--type", "image/png"]),
    (
        "xclip",
        &["-selection", "clipboard", "-t", "image/png", "-o"],
    ),
];

/// A PNG that fits the size cap.
fn valid_png(b: &[u8]) -> bool {
    b.len() > PNG_MAGIC.len() && b.len() <= MAX_IMAGE && b.starts_with(PNG_MAGIC)
}

/// The image on the host clipboard as PNG bytes; `None` when it holds no image, or no tool
/// to read it is installed. Called only when the user asks to paste.
pub fn host_image() -> Option<Vec<u8>> {
    READERS.iter().find_map(|(prog, args)| {
        let out = Command::new(prog)
            .args(*args)
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .ok()?;
        (out.status.success() && valid_png(&out.stdout)).then_some(out.stdout)
    })
}

/// Why pasting found nothing, when the cause is a missing clipboard tool.
pub fn missing_tool_hint() -> Option<String> {
    if READERS.iter().any(|(prog, _)| cli::have(prog)) {
        return None;
    }
    let host = crate::platform::RealHost;
    let os = if cfg!(target_os = "macos") {
        crate::platform::Os::MacOs
    } else {
        crate::platform::Os::Linux
    };
    let wayland = std::env::var_os("WAYLAND_DISPLAY").is_some_and(|d| !d.is_empty());
    Some(crate::setup::hint(
        "clipboard",
        os,
        crate::platform::detect_pkg(&host, os),
        wayland,
    ))
}

/// The sandbox user's HOME, on `gateway`.
fn sandbox_home(gateway: &str, sandbox: &str) -> Result<String, String> {
    let mut args: Vec<String> = Vec::new();
    if !gateway.is_empty() {
        args.extend(["-g".to_string(), gateway.to_string()]);
    }
    args.extend(
        [
            "sandbox",
            "exec",
            "-n",
            sandbox,
            "--timeout",
            "30",
            "--",
            "sh",
            "-c",
            "printf %s \"$HOME\"",
        ]
        .map(String::from),
    );
    let home = cli::run("openshell", &args, &[])?.trim().to_string();
    if home.starts_with('/') {
        Ok(home)
    } else {
        Err(format!("no home directory in {sandbox}"))
    }
}

/// Upload `png` to a fresh name under the sandbox's paste directory; returns the path inside it.
/// The host copy lives in a private temp directory, removed before returning.
pub fn upload(gateway: &str, sandbox: &str, png: &[u8]) -> Result<String, String> {
    let home = sandbox_home(gateway, sandbox)?;
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let name = format!("{nanos:x}.png");
    let dir: PathBuf =
        std::env::temp_dir().join(format!("osh-paste-{}-{nanos:x}", std::process::id()));
    fs::DirBuilder::new()
        .mode(0o700)
        .create(&dir)
        .map_err(|e| e.to_string())?;
    let local = dir.join(&name);
    let remote = format!("{home}/{SANDBOX_DIR}/{name}");
    let result = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&local)
        .and_then(|mut f| f.write_all(png))
        .map_err(|e| e.to_string())
        .and_then(|_| {
            let mut args = Vec::new();
            if !gateway.is_empty() {
                args.extend(["-g".to_string(), gateway.to_string()]);
            }
            args.extend(
                [
                    "sandbox",
                    "upload",
                    sandbox,
                    &local.to_string_lossy(),
                    &remote,
                    "--no-git-ignore",
                ]
                .map(String::from),
            );
            cli::run("openshell", &args, &[])
        });
    let _ = fs::remove_dir_all(&dir);
    result.map(|_| remote)
}

/// Delete the sandbox's paste directory; best effort, the sandbox may be gone.
pub fn clean(gateway: &str, sandbox: &str) {
    let mut args = Vec::new();
    if !gateway.is_empty() {
        args.extend(["-g".to_string(), gateway.to_string()]);
    }
    args.extend(
        [
            "sandbox",
            "exec",
            "-n",
            sandbox,
            "--timeout",
            "30",
            "--",
            "sh",
            "-c",
            &format!("rm -rf \"$HOME/{SANDBOX_DIR}\""),
        ]
        .map(String::from),
    );
    let _ = cli::run("openshell", &args, &[]);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_png_within_the_cap_is_sent() {
        let mut png = PNG_MAGIC.to_vec();
        png.extend_from_slice(b"data");
        assert!(valid_png(&png));
        assert!(!valid_png(PNG_MAGIC));
        assert!(!valid_png(b"GIF89a-not-a-png"));
        let mut big = PNG_MAGIC.to_vec();
        big.resize(MAX_IMAGE + 1, 0);
        assert!(!valid_png(&big));
    }
}
