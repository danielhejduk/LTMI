use std::error::Error;
use std::process::{Child, Command};

/// Extra arguments ltmi passes to the app, chosen with `--preset`.
#[derive(clap::ValueEnum, Clone, Copy, Debug, Default)]
pub enum Preset {
    /// Chromium-based browsers (Brave, Chrome, Chromium): native Wayland, separate profile
    #[default]
    Chromium,
    /// No extra arguments
    Blank,
}

/// Start `app` on ltmi's Wayland socket and, if Xwayland is running, on its X display.
pub fn launch(app: &str, args: &[String], socket_name: &str, x_display: Option<u32>) -> Result<Child, Box<dyn Error>> {
    let mut cmd = Command::new(app);
    cmd.args(args).env("WAYLAND_DISPLAY", socket_name);
    match x_display {
        Some(display) => cmd.env("DISPLAY", format!(":{display}")),
        // Never fall back to the parent session's X server.
        None => cmd.env_remove("DISPLAY"),
    };
    Ok(cmd.spawn()?)
}

pub fn get_args(preset: Preset) -> Vec<String> {
    let brave_profile = std::env::var_os("XDG_DATA_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| std::path::Path::new(&home).join(".local/share")))
        .unwrap_or_else(std::env::temp_dir)
        .join("ltmi/brave-profile");

    match preset {
        Preset::Chromium => vec![
            "--ozone-platform=wayland".to_string(),
            format!("--user-data-dir={}", brave_profile.display()),
            "--no-first-run".to_string(),
        ],
        Preset::Blank => vec![],
    }
}

/// Arguments for restarting the app after it closed itself while locked.
pub fn get_relaunch_args(preset: Preset) -> Vec<String> {
    let mut args = get_args(preset);
    if let Preset::Chromium = preset {
        // Brings back the tabs the user just closed.
        args.push("--restore-last-session".to_string());
    }
    args
}
