//! Embeds the app icon and version info as native Win32 resources so
//! `rc-host.exe` shows the right icon in Explorer, the taskbar and Alt+Tab —
//! not just the tray (which loads it explicitly at runtime).

fn main() {
    if std::env::var("CARGO_CFG_WINDOWS").is_ok() {
        let mut res = winresource::WindowsResource::new();
        // Fixed resource id "1" so the tray code can `LoadIconW` it directly
        // instead of parsing resource names at runtime.
        res.set_icon_with_id("assets/icon.ico", "1");
        res.set("ProductName", "Remote Control");
        res.set("FileDescription", "Remote Control host service");
        if let Err(e) = res.compile() {
            // Non-fatal: a dev build without the RC toolchain on PATH still
            // links, it just falls back to the default exe icon.
            println!("cargo:warning=could not embed Windows resources: {e}");
        }
    }
}
