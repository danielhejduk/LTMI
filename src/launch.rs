use std::error::Error;
use std::process::{Child, Command};

pub enum Args {
    Chromium,
}

pub fn launch(app: &str, args: &[String], socket_name: &str) -> Result<Child, Box<dyn Error>> {
    Ok(Command::new(app)
        .args(args)
        .env("WAYLAND_DISPLAY", socket_name)
        .env_remove("DISPLAY")
        .spawn()?)
}

pub fn get_args(app: Args) -> Vec<String> {
    let brave_profile = std::env::var_os("XDG_DATA_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| std::path::Path::new(&home).join(".local/share")))
        .unwrap_or_else(std::env::temp_dir)
        .join("ltmi/brave-profile");

    match app {
        Args::Chromium => vec![
            "--ozone-platform=wayland".to_string(),
            format!("--user-data-dir={}", brave_profile.display()),
            "--no-first-run".to_string(),
        ],
    }
}
