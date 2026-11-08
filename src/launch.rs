use std::error::Error;
use std::ffi::{CStr, CString};
use std::io;
use std::os::unix::process::CommandExt;
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
///
/// With `offline`, the app gets its own network namespace with only loopback: no DNS, TCP or UDP
/// beyond localhost, while the Wayland, X11, D-Bus and audio sockets (paths on disk) keep working. If that namespace can't be
/// created, the launch fails instead of running the app online.
pub fn launch(
    app: &str,
    args: &[String],
    socket_name: &str,
    x_display: Option<u32>,
    offline: bool,
) -> Result<Child, Box<dyn Error>> {
    let mut cmd = Command::new(app);
    cmd.args(args).env("WAYLAND_DISPLAY", socket_name);
    match x_display {
        Some(display) => cmd.env("DISPLAY", format!(":{display}")),
        // Never fall back to the parent session's X server.
        None => cmd.env_remove("DISPLAY"),
    };
    if offline {
        cut_network(&mut cmd);
    }
    cmd.spawn().map_err(|err| {
        if offline {
            format!("{app}: couldn't start offline (unprivileged user namespaces may be disabled): {err}").into()
        } else {
            Box::<dyn Error>::from(err)
        }
    })
}

/// Make `cmd` run in a new user and network namespace, keeping the user's own uid and gid
/// (Chromium refuses to run as root, and its sandbox needs a regular uid).
fn cut_network(cmd: &mut Command) {
    // Formatted here: the child runs between fork and exec of a multithreaded process, where
    // allocating can deadlock.
    let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
    let uid_map = CString::new(format!("{uid} {uid} 1")).unwrap();
    let gid_map = CString::new(format!("{gid} {gid} 1")).unwrap();
    let closure = move || {
        if unsafe { libc::unshare(libc::CLONE_NEWUSER | libc::CLONE_NEWNET) } != 0 {
            return Err(io::Error::last_os_error());
        }
        // setgroups must be denied before an unprivileged process may write its gid_map.
        write_file(c"/proc/self/setgroups", b"deny")?;
        write_file(c"/proc/self/uid_map", uid_map.as_bytes())?;
        write_file(c"/proc/self/gid_map", gid_map.as_bytes())?;
        // Must happen before exec, which drops the capabilities the new namespaces granted.
        bring_up_loopback()
    };
    unsafe { cmd.pre_exec(closure) };
}

/// Set `lo` up, so the app can still reach localhost. A new network namespace starts with it down.
/// Raw syscalls only, so it is safe to call after fork.
fn bring_up_loopback() -> io::Result<()> {
    let fd = unsafe { libc::socket(libc::AF_INET, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, 0) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    let mut ifr: libc::ifreq = unsafe { std::mem::zeroed() };
    for (dst, src) in ifr.ifr_name.iter_mut().zip(b"lo") {
        *dst = *src as libc::c_char;
    }
    let result = unsafe {
        if libc::ioctl(fd, libc::SIOCGIFFLAGS as _, &mut ifr) < 0 {
            Err(io::Error::last_os_error())
        } else {
            ifr.ifr_ifru.ifru_flags |= libc::IFF_UP as libc::c_short;
            if libc::ioctl(fd, libc::SIOCSIFFLAGS as _, &ifr) < 0 {
                Err(io::Error::last_os_error())
            } else {
                Ok(())
            }
        }
    };
    unsafe { libc::close(fd) };
    result
}

/// Write `data` to `path` with raw syscalls only, so it is safe to call after fork.
fn write_file(path: &CStr, data: &[u8]) -> io::Result<()> {
    let fd = unsafe { libc::open(path.as_ptr(), libc::O_WRONLY | libc::O_CLOEXEC) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    let written = unsafe { libc::write(fd, data.as_ptr().cast(), data.len()) };
    let result = if written == data.len() as isize {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    };
    unsafe { libc::close(fd) };
    result
}

pub fn get_args(preset: Preset) -> Vec<String> {
    let chromium_profile = std::env::var_os("XDG_DATA_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| std::path::Path::new(&home).join(".local/share")))
        .unwrap_or_else(std::env::temp_dir)
        .join("ltmi/chromium-profile");

    match preset {
        Preset::Chromium => vec![
            "--ozone-platform=wayland".to_string(),
            format!("--user-data-dir={}", chromium_profile.display()),
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Succeeds only if the shell sees no network interface other than loopback.
    const ONLY_LOOPBACK: &str = "! grep ':' /proc/net/dev | grep -vq '^ *lo:'";

    #[test]
    fn offline_app_sees_only_loopback() {
        let args = ["-c".to_string(), ONLY_LOOPBACK.to_string()];
        match launch("sh", &args, "wayland-test", None, true) {
            Ok(mut child) => assert!(
                child.wait().unwrap().success(),
                "offline app could see a network interface"
            ),
            // Without unprivileged user namespaces (some CI runners) it must refuse, never run online.
            Err(err) => assert!(err.to_string().contains("couldn't start offline"), "{err}"),
        }
    }

    /// Printed by `loopback_probe` once it has made a TCP connection over 127.0.0.1.
    const LOOPBACK_OK: &str = "loopback ok";

    #[test]
    fn offline_app_reaches_localhost() {
        // Reruns this test binary offline, limited to `loopback_probe`.
        let mut cmd = Command::new(std::env::current_exe().unwrap());
        cmd.args(["launch::tests::loopback_probe", "--exact", "--ignored", "--nocapture"]);
        cut_network(&mut cmd);
        // Can't go offline here (no unprivileged user namespaces); the test above covers that case.
        let Ok(output) = cmd.output() else { return };
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            stdout.contains(LOOPBACK_OK),
            "offline app couldn't reach localhost:\n{stdout}{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    #[ignore = "run inside the offline namespace by offline_app_reaches_localhost"]
    fn loopback_probe() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        println!("{LOOPBACK_OK}");
    }
}
