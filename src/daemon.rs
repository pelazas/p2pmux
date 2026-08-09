//! The part of p2pmux that is running when nobody is looking.
//!
//! A machine can only be in your fleet if something on it is listening. A
//! droplet you paired six months ago is only "asleep" rather than gone because
//! this exists: it rejoins the home session at boot, stays in it, and is
//! therefore reachable when another of your machines starts a session and says
//! where it went.
//!
//! It is deliberately thin. The daemon does not host panes, hold layout, or
//! know anything about sessions — the node already does all of that. What the
//! daemon adds is that the node is *there*: started at boot, restarted when it
//! dies, and pointed at the session pairing recorded.
//!
//! The supervision itself belongs to the operating system rather than to a
//! loop in here, because the operating system is the only part that survives
//! this process being killed. `install` writes the unit that says so, and the
//! two platforms' units are generated from one description so they cannot
//! drift into promising different things.

use std::{error::Error, io, path::PathBuf, time::Duration};

/// How often the daemon checks that its session is still up.
///
/// Slow on purpose. Nothing here is latency-sensitive: the node reconnects on
/// its own when a network comes back, and this only notices the case where the
/// node process is gone entirely.
const SUPERVISION_INTERVAL: Duration = Duration::from_secs(15);

/// The name both platforms know the service by.
///
/// One constant, because an install that wrote one name and an uninstall that
/// looked for another would leave a service nobody could stop.
pub const SERVICE_LABEL: &str = "com.p2pmux.fleet";
/// The systemd unit file name, which by convention is not a reverse-DNS label.
pub const SYSTEMD_UNIT_NAME: &str = "p2pmux-fleet.service";

/// Everything both unit files are generated from.
///
/// A single description rather than two hand-written templates: the properties
/// that matter — start at boot, restart on crash — have to be true on both
/// platforms, and the only way to be sure of that is for both to be rendered
/// from the same fields.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ServiceUnit {
    /// The p2pmux binary, as an absolute path. A service that resolved this
    /// from `PATH` would start whatever happened to be installed later.
    pub program: String,
    /// Where the daemon's own output goes. Not the session's — the node writes
    /// nothing to stdout — but a service that fails to start says why here.
    pub log_path: String,
}

impl ServiceUnit {
    /// The unit for the p2pmux running now, installed for this user.
    pub fn for_current_install() -> Result<Self, Box<dyn Error>> {
        let program = std::env::current_exe()?
            .to_str()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "non-UTF-8 program path"))?
            .to_owned();
        let log_path = log_path()?
            .to_str()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "non-UTF-8 log path"))?
            .to_owned();
        Ok(Self { program, log_path })
    }

    /// A launchd agent: `RunAtLoad` starts it at login, `KeepAlive` restarts it
    /// when it dies.
    pub fn launchd_plist(&self) -> String {
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{SERVICE_LABEL}</string>
    <key>ProgramArguments</key>
    <array>
        <string>{program}</string>
        <string>daemon</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <true/>
    <key>StandardOutPath</key>
    <string>{log}</string>
    <key>StandardErrorPath</key>
    <string>{log}</string>
</dict>
</plist>
"#,
            program = self.program,
            log = self.log_path,
        )
    }

    /// A systemd *user* unit, not a system one: the fleet belongs to a user
    /// account, the pairing record lives in that account's config directory,
    /// and a system service would be running as the wrong person.
    pub fn systemd_unit(&self) -> String {
        format!(
            "[Unit]\n\
             Description=p2pmux fleet agent\n\
             After=network-online.target\n\
             \n\
             [Service]\n\
             Type=simple\n\
             ExecStart={program} daemon\n\
             Restart=always\n\
             RestartSec=5\n\
             \n\
             [Install]\n\
             WantedBy=default.target\n",
            program = self.program,
        )
    }
}

/// Where this platform's unit file goes.
pub fn unit_path() -> Result<PathBuf, Box<dyn Error>> {
    let home = std::env::var_os("HOME")
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "HOME is not set"))?;
    let home = PathBuf::from(home);
    if cfg!(target_os = "macos") {
        return Ok(home
            .join("Library")
            .join("LaunchAgents")
            .join(format!("{SERVICE_LABEL}.plist")));
    }
    Ok(std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".config"))
        .join("systemd")
        .join("user")
        .join(SYSTEMD_UNIT_NAME))
}

fn log_path() -> Result<PathBuf, Box<dyn Error>> {
    let home = std::env::var_os("HOME")
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "HOME is not set"))?;
    Ok(PathBuf::from(home).join(".p2pmux-fleet.log"))
}

/// The unit's contents for whichever platform this is.
pub fn unit_contents(unit: &ServiceUnit) -> String {
    if cfg!(target_os = "macos") {
        unit.launchd_plist()
    } else {
        unit.systemd_unit()
    }
}

/// Write the service and ask the operating system to start it.
///
/// Loading it is best-effort and reported rather than fatal: the unit on disk
/// is the durable half, and a machine whose `launchctl` refused today will
/// still start the daemon at the next login.
pub fn install() -> Result<PathBuf, Box<dyn Error>> {
    let unit = ServiceUnit::for_current_install()?;
    let path = unit_path()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, unit_contents(&unit))?;
    let _ = load_service(&path, true);
    Ok(path)
}

/// Stop the service and remove its unit. Missing is success: uninstalling
/// something that is not installed is what the user asked for either way.
pub fn uninstall() -> Result<Option<PathBuf>, Box<dyn Error>> {
    let path = unit_path()?;
    if !path.exists() {
        return Ok(None);
    }
    let _ = load_service(&path, false);
    std::fs::remove_file(&path)?;
    Ok(Some(path))
}

/// Whether this machine has the service installed.
pub fn installed() -> bool {
    unit_path().map(|path| path.exists()).unwrap_or(false)
}

fn load_service(path: &std::path::Path, load: bool) -> io::Result<()> {
    let mut command = if cfg!(target_os = "macos") {
        let mut command = std::process::Command::new("launchctl");
        command.arg(if load { "load" } else { "unload" }).arg(path);
        command
    } else {
        let mut command = std::process::Command::new("systemctl");
        command.arg("--user");
        if load {
            command.arg("enable").arg("--now").arg(SYSTEMD_UNIT_NAME);
        } else {
            command.arg("disable").arg("--now").arg(SYSTEMD_UNIT_NAME);
        }
        command
    };
    command
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|_| ())
}

/// Run the fleet agent in the foreground until told to stop.
///
/// The whole job is keeping this machine's home session up. Everything else —
/// answering invitations, serving remote terminals, reporting agents — is the
/// node's, and the node is what this keeps alive.
pub async fn run() -> Result<(), Box<dyn Error>> {
    let pairing = crate::pairing::load()?;
    let Some(ticket) = pairing.ticket.clone() else {
        return Err(Box::new(io::Error::new(
            io::ErrorKind::NotFound,
            "this machine is not paired, so it has no fleet to join — run `p2pmux pair` first",
        )));
    };
    println!("p2pmux fleet agent: keeping this machine in its home session");
    let mut interrupt = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    loop {
        if let Err(error) = ensure_home_session(&ticket).await {
            // Reported and retried rather than fatal. The usual reason is a
            // network that is not up yet, and a daemon that exited on that
            // would need the operating system to restart it into the same
            // failure a second later.
            eprintln!("p2pmux fleet agent: {error}");
        }
        tokio::select! {
            _ = tokio::time::sleep(SUPERVISION_INTERVAL) => {}
            _ = interrupt.recv() => {
                println!("p2pmux fleet agent: stopping");
                return Ok(());
            }
            _ = tokio::signal::ctrl_c() => {
                println!("p2pmux fleet agent: stopping");
                return Ok(());
            }
        }
    }
}

/// Start the home session's node if it is not already running.
async fn ensure_home_session(ticket: &str) -> Result<(), Box<dyn Error>> {
    if crate::node::follow_fleet_invite(ticket)? {
        println!("p2pmux fleet agent: rejoined the home session");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{SERVICE_LABEL, ServiceUnit};

    fn unit() -> ServiceUnit {
        ServiceUnit {
            program: String::from("/usr/local/bin/p2pmux"),
            log_path: String::from("/home/me/.p2pmux-fleet.log"),
        }
    }

    /// The two properties the issue asks for, on both platforms, from one
    /// description. A unit that started at boot on one and not the other would
    /// be a fleet that is only a fleet on a Mac.
    #[test]
    fn both_platforms_start_at_boot_and_restart_on_crash() {
        let plist = unit().launchd_plist();
        assert!(plist.contains("<key>RunAtLoad</key>"), "{plist}");
        assert!(plist.contains("<key>KeepAlive</key>"), "{plist}");
        assert!(plist.contains(SERVICE_LABEL), "{plist}");

        let systemd = unit().systemd_unit();
        assert!(systemd.contains("Restart=always"), "{systemd}");
        assert!(systemd.contains("WantedBy=default.target"), "{systemd}");
    }

    /// A service that resolved p2pmux from `PATH` would start whatever was
    /// installed later, which is not the binary the user pointed at.
    #[test]
    fn both_units_name_the_binary_by_absolute_path() {
        assert!(unit().launchd_plist().contains("/usr/local/bin/p2pmux"));
        assert!(
            unit()
                .systemd_unit()
                .contains("ExecStart=/usr/local/bin/p2pmux daemon")
        );
    }

    /// The fleet belongs to a user account: the pairing record is in that
    /// account's config directory, and a system-wide service would be running
    /// as the wrong person entirely.
    #[test]
    fn the_linux_unit_is_a_user_service() {
        let systemd = unit().systemd_unit();
        assert!(
            !systemd.contains("WantedBy=multi-user.target"),
            "a system target would run this as the wrong user: {systemd}"
        );
    }
}
