//! What differs between macOS, Windows and Linux: opening links and files,
//! showing a file in the file manager, and running helper programs.

use std::ffi::OsStr;
use std::path::Path;
use std::process::Command;

/// A helper program. On Windows it runs without flashing a console window.
pub fn command(program: impl AsRef<OsStr>) -> Command {
    #[allow(unused_mut)]
    let mut cmd = Command::new(program);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd
}

/// Opens a web page, a file or a folder with the system's default app.
pub fn open(target: impl AsRef<OsStr>) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    let mut cmd = command("open");
    #[cfg(windows)]
    let mut cmd = {
        // `start` is the shell's: its first quoted argument is a window title.
        let mut cmd = command("cmd");
        cmd.args(["/C", "start", ""]);
        cmd
    };
    #[cfg(all(unix, not(target_os = "macos")))]
    let mut cmd = command("xdg-open");
    cmd.arg(target).spawn().map(|_| ()).map_err(|e| e.to_string())
}

/// Shows a file selected in Finder or Explorer; on Linux, opens its folder.
pub fn reveal(path: &Path) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    return command("open").arg("-R").arg(path).spawn().map(|_| ()).map_err(|e| e.to_string());
    #[cfg(windows)]
    {
        let mut select = std::ffi::OsString::from("/select,");
        select.push(path);
        return command("explorer").arg(select).spawn().map(|_| ()).map_err(|e| e.to_string());
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    open(path.parent().unwrap_or(path))
}
