use std::process::{Command, Child};
use std::error::Error;

use smithay::reexports::wayland_server::ListeningSocket;

pub fn launch(app: &str, socket_name: &str) -> Result<Child, Box<dyn Error>> {
    let brave_profile = std::env::var_os("XDG_DATA_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| std::path::Path::new(&home).join(".local/share")))
        .unwrap_or_else(std::env::temp_dir)
        .join("ltmi/brave-profile");

    let listener = ListeningSocket::bind_auto("wayland", 1..33)?;

    Ok(
        Command::new(app)
            .args(["--ozone-platform=wayland", format!("--user-data-dir={}", brave_profile.display()).as_str(), "--no-first-run"])
            .env("WAYLAND_DISPLAY", "wayland-5")
            .env_remove("DISPLAY")
            .spawn()?
    )
}
