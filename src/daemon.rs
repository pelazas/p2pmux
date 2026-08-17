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

/// The longest the agent waits between attempts to rejoin a session it cannot
/// reach.
///
/// This is a ceiling, not a surrender. A home session legitimately comes back —
/// the laptop hosting it is opened, the network returns — and an agent that had
/// stopped asking would leave its machine missing from the fleet until somebody
/// thought to restart it. What the ceiling ends is the *rate*: on 2026-08-16 two
/// machines chased a session neither of them hosted for four days, each asking
/// every fifteen seconds, each attempt a whole operating-system process.
///
/// Five minutes rather than the fifteen this was first written with, because the
/// ceiling is also the worst case for the thing this daemon exists to do. The
/// promise at the top of this file is that a machine is *there* when you start a
/// session somewhere else, and a quarter of an hour of not being there is a
/// regression somebody would notice and rightly dislike. Five minutes is still
/// fifty attempts across a four-hour outage where the old build made a thousand,
/// and every one of those fifty now cleans up after itself.
const MAX_RETRY_INTERVAL: Duration = Duration::from_secs(5 * 60);

/// How old a launch's leftover files must be before they are assumed to be
/// litter rather than a launch still in progress.
///
/// The launcher gives a node sixty seconds to become ready. This is several
/// times that, because being slow to tidy up costs nothing and deleting a live
/// session's bootstrap costs it the session.
const STALE_LAUNCH_FILE_AGE: Duration = Duration::from_secs(10 * 60);

/// The delay between one failed attempt and the next.
///
/// Doubling, capped, and jittered — the shape every retry loop that has had to
/// stop hurting the thing it was retrying against converges on. The jitter is
/// what keeps a fleet of machines that all lost the same coordinator from
/// coming back at it in lockstep, and it matters more here than in most places
/// because every machine in a fleet reacts to the *same* event at the *same*
/// moment.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Backoff {
    base: Duration,
    ceiling: Duration,
    failures: u32,
}

impl Backoff {
    pub fn new(base: Duration, ceiling: Duration) -> Self {
        Self {
            base,
            ceiling,
            failures: 0,
        }
    }

    /// The session is up. The next check is at the ordinary cadence again.
    pub fn succeeded(&mut self) {
        self.failures = 0;
    }

    pub fn failed(&mut self) {
        self.failures = self.failures.saturating_add(1);
    }

    /// The undithered delay, which is what a log line should quote: "retrying in
    /// 4m" is a promise the jitter would make a liar of by a few seconds.
    pub fn plain_interval(&self) -> Duration {
        if self.failures == 0 {
            return self.base;
        }
        // Saturating at 2^16 rather than relying on the ceiling to catch it: the
        // shift itself is what overflows first, and a fleet agent left running
        // for a year gets there.
        let doublings = (self.failures - 1).min(16);
        self.base
            .saturating_mul(1u32 << doublings)
            .min(self.ceiling)
    }

    /// Where in the jitter band to land, as a fraction of it.
    ///
    /// Split out from [`Self::interval`] so the arithmetic can be tested without
    /// a random number generator deciding whether the test passes.
    fn interval_at(&self, fraction: f64) -> Duration {
        let plain = self.plain_interval();
        if self.failures == 0 {
            // A healthy agent keeps a steady heartbeat. There is nothing to
            // spread out, and a wandering interval would only make the logs
            // harder to read.
            return plain;
        }
        // Equal jitter: half the delay is guaranteed, the other half is spread.
        // Full jitter (anywhere from zero to the delay) is the usual advice, but
        // it lets an unlucky draw retry almost immediately, which is the exact
        // behaviour the ceiling exists to prevent.
        let half = plain / 2;
        half + half.mul_f64(fraction.clamp(0.0, 1.0))
    }

    /// How long to wait before trying again.
    pub fn interval(&self) -> Duration {
        self.interval_at(random_fraction())
    }
}

/// A number in `[0, 1)`, from the same source as every other random value here.
///
/// Falls back to the middle of the band rather than failing: a machine whose
/// entropy source is unavailable should still back off, just without the
/// dithering.
fn random_fraction() -> f64 {
    let mut bytes = [0u8; 4];
    match getrandom::fill(&mut bytes) {
        Ok(()) => f64::from(u32::from_le_bytes(bytes)) / f64::from(u32::MAX),
        Err(_) => 0.5,
    }
}

/// The name both platforms know the service by.
///
/// One constant, because an install that wrote one name and an uninstall that
/// looked for another would leave a service nobody could stop.
pub const SERVICE_LABEL: &str = "com.p2pmux.fleet";
/// The systemd unit file name, which by convention is not a reverse-DNS label.
pub const SYSTEMD_UNIT_NAME: &str = "p2pmux-fleet.service";

/// The most memory the agent and everything it starts may hold, in megabytes.
///
/// Chosen against what the parts actually cost: the daemon is 8MB, an idle node
/// is 24MB, and a node hosting several panes with their scrollback has never
/// been seen past about 150MB. Half a gigabyte is three times the most
/// legitimate reading and a small fraction of the smallest machine anybody runs
/// this on.
///
/// The point is not the number, it is that there *is* one. Without it the
/// kernel's own out-of-memory killer picks the victim, and it picks by size
/// across the whole machine: on 2026-08-16 it took a trading bot, an API server
/// and a message gateway before it got to the p2pmux processes that had eaten
/// the memory. Inside a limit, the cgroup's killer only ever chooses from the
/// processes that are over it.
pub(crate) const MEMORY_MAX_MB: u32 = 512;
/// Where the kernel starts reclaiming rather than killing. A leak crosses this
/// first and gets slower, which is a symptom somebody can see coming.
const MEMORY_HIGH_MB: u32 = 256;
/// Processes and threads. A daemon with one node uses about fourteen.
const TASKS_MAX: u32 = 64;

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

    /// A launchd agent: `RunAtLoad` starts it at login, and `KeepAlive` brings
    /// it back when it *fails*.
    ///
    /// `KeepAlive` as a bare `true` is the shape to avoid. It restarts the job
    /// whatever the exit status, so an agent asked to stop comes straight back —
    /// which reads, correctly, as software the user cannot turn off. The
    /// dictionary form restarts only the inverse of a successful exit, and the
    /// daemon exits zero when it is signalled, so `launchctl unload` means what
    /// it says.
    ///
    /// macOS has no cgroups, so there is no honest equivalent of the memory
    /// ceiling the Linux unit gets. `ProcessType` is the documented substitute
    /// for the resource-limit keys — Apple's own guidance prefers it — and it
    /// says what this actually is: work the user did not directly ask for,
    /// which the system may hold back to protect the machine's responsiveness.
    /// The hard ceiling on this platform has to live in the process itself.
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
    <dict>
        <key>SuccessfulExit</key>
        <false/>
    </dict>
    <key>ThrottleInterval</key>
    <integer>{throttle}</integer>
    <key>ProcessType</key>
    <string>Background</string>
    <key>LowPriorityIO</key>
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
            throttle = SUPERVISION_INTERVAL.as_secs(),
        )
    }

    /// A systemd *user* unit, not a system one: the fleet belongs to a user
    /// account, the pairing record lives in that account's config directory,
    /// and a system service would be running as the wrong person.
    ///
    /// `Restart=on-failure` rather than `always`, for the same reason launchd
    /// gets a `KeepAlive` dictionary: the daemon exits zero when it is asked to
    /// stop, and a service that restarts through that is a service the user
    /// cannot stop.
    ///
    /// The resource stanza is the part that makes a bug in everything above it
    /// survivable. `MemoryMax` is enforced by the unit's own cgroup, so when it
    /// is reached the kernel kills something *inside this unit* rather than
    /// hunting the machine for the biggest process it can find — which is how a
    /// p2pmux leak came to kill a trading bot. It needs the memory controller
    /// delegated to the user manager, which systemd has done by default for
    /// years and which is worth nothing if untrue: a limit that silently does
    /// not apply is worse than no limit, so `daemon install` checks and says so.
    ///
    /// `CPUWeight` rather than `CPUQuota`: a hard cap would make a terminal
    /// somebody is typing in stutter, while a weight only yields when something
    /// else on the machine wants the processor. It is the same intent as
    /// launchd's `ProcessType=Background`.
    ///
    /// `StartLimitIntervalSec` and `StartLimitBurst` are `[Unit]` keys, not
    /// `[Service]` ones — systemd moved them in v229 and logs `Unknown key
    /// name … in section 'Service', ignoring` for the old spelling. Written
    /// under `[Service]` the ceiling did not exist, which is the failure this
    /// unit's own `MemoryMax` note calls worse than no limit at all.
    ///
    /// It matters most where it is least visible. On systemd v254 and newer the
    /// escalating `RestartSteps`/`RestartMaxDelaySec` backoff reaches five
    /// minutes on its own, so ten starts per five minutes is a ceiling nothing
    /// ordinarily touches. Below v254 those two keys are themselves ignored and
    /// every restart waits a flat `RestartSec` — twenty starts in the same
    /// window — so on exactly the systems with no backoff, this is the only
    /// thing that stops a crash loop.
    pub fn systemd_unit(&self) -> String {
        format!(
            "[Unit]\n\
             Description=p2pmux fleet agent\n\
             After=network-online.target\n\
             StartLimitIntervalSec=300\n\
             StartLimitBurst=10\n\
             \n\
             [Service]\n\
             Type=simple\n\
             ExecStart={program} daemon\n\
             Restart=on-failure\n\
             RestartSec={restart_sec}\n\
             RestartSteps=6\n\
             RestartMaxDelaySec={restart_max}\n\
             MemoryHigh={memory_high}M\n\
             MemoryMax={memory_max}M\n\
             TasksMax={tasks_max}\n\
             CPUWeight=20\n\
             OOMPolicy=stop\n\
             KillMode=control-group\n\
             TimeoutStopSec=10\n\
             \n\
             [Install]\n\
             WantedBy=default.target\n",
            program = self.program,
            restart_sec = SUPERVISION_INTERVAL.as_secs(),
            restart_max = MAX_RETRY_INTERVAL.as_secs(),
            memory_high = MEMORY_HIGH_MB,
            memory_max = MEMORY_MAX_MB,
            tasks_max = TASKS_MAX,
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
    if let Some(warning) = memory_limit_warning() {
        eprintln!("p2pmux: {warning}");
    }
    Ok(path)
}

/// Whether the memory ceiling in the unit will actually be enforced, and what to
/// say if it will not.
///
/// A user unit can only be held to `MemoryMax` if the memory controller has been
/// delegated to the user's own systemd manager. It has been by default for
/// years, and on the machine this was written against it is — but a limit that
/// silently does not apply is worse than no limit at all, because it is the one
/// thing standing between a leak and the rest of the machine. So it is checked
/// rather than assumed, once, at the moment somebody is reading the output.
///
/// Not an error: the agent is still worth installing without it, and the
/// alternative — refusing to install over a kernel setting the user did not
/// choose — helps nobody.
fn memory_limit_warning() -> Option<String> {
    if cfg!(target_os = "macos") {
        // Nothing to check. macOS has no cgroups, the plist promises no ceiling,
        // and the node carries its own. See `TETHERED_MEMORY_CEILING`.
        return None;
    }
    let controllers = std::fs::read_to_string(format!(
        "/sys/fs/cgroup/user.slice/user-{}.slice/user@{}.service/cgroup.controllers",
        current_uid()?,
        current_uid()?
    ))
    .ok()?;
    if controllers.split_whitespace().any(|name| name == "memory") {
        return None;
    }
    Some(format!(
        "this system does not delegate the memory controller to user services, so the \
         agent's {MEMORY_MAX_MB}MB limit will not be enforced. It will still start at \
         boot and restart on failure."
    ))
}

fn current_uid() -> Option<u32> {
    // Read from the runtime directory rather than by calling libc, which is not
    // a direct dependency: `XDG_RUNTIME_DIR` is `/run/user/<uid>` on every
    // system that has a user manager to delegate anything in the first place.
    std::env::var_os("XDG_RUNTIME_DIR")?
        .to_str()?
        .rsplit('/')
        .next()?
        .parse()
        .ok()
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

/// What the agent should do on this tick.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Step {
    /// Make sure this machine is in that session.
    Follow(String),
    /// There is no fleet on this machine yet. Keep running and look again.
    WaitForPairing,
}

/// Read one tick's decision off the pairing record.
///
/// Split out so the loop below stays a loop and this stays a fact about the
/// record: an agent with nothing to join waits, and only a ticket makes it act.
pub fn next_step(pairing: &crate::pairing::Pairing) -> Step {
    match pairing.ticket.as_deref() {
        Some(ticket) => Step::Follow(ticket.to_owned()),
        None => Step::WaitForPairing,
    }
}

/// Run the fleet agent in the foreground until told to stop.
///
/// The whole job is keeping this machine's home session up. Everything else —
/// answering invitations, serving remote terminals, reporting agents — is the
/// node's, and the node is what this keeps alive.
pub async fn run() -> Result<(), Box<dyn Error>> {
    println!("p2pmux fleet agent: keeping this machine in its home session");
    // Whatever the last run of this machine left behind. The launcher tidies up
    // after itself now, so this only ever finds the files of a process that was
    // killed outright -- which is exactly what happened here, and left 1014 of
    // them. Done once at start rather than on the timer: it is a directory scan,
    // and nothing produces litter while the agent is behaving.
    if let Ok(store) = crate::session_store::SessionStore::for_current_user() {
        let swept = store.sweep_stale_launch_files(STALE_LAUNCH_FILE_AGE);
        if swept > 0 {
            println!("p2pmux fleet agent: cleared {swept} files left by a previous run");
        }
    }
    let mut interrupt = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let mut waiting = false;
    let mut backoff = Backoff::new(SUPERVISION_INTERVAL, MAX_RETRY_INTERVAL);
    // What the last attempt failed with, and how long the agent said it would
    // wait. Both are kept so the *next* failure can decide whether it is worth
    // mentioning: a reason that has not changed and a delay that has not moved
    // is a line the journal has already got.
    let mut reported: Option<(String, Duration)> = None;
    loop {
        // Before anything else, because everything else depends on it: a node is
        // launched by re-running this program, and there is no point deciding
        // which session to join with a binary that can no longer be started.
        if program_was_replaced(&std::env::current_exe()) {
            println!(
                "p2pmux fleet agent: the p2pmux binary was replaced — stopping so the \
                 service manager starts the new one"
            );
            return Err(io::Error::other("the p2pmux binary was replaced").into());
        }
        // Asked every tick rather than once at start, because both answers can
        // change under a service that outlives them: a machine paired after the
        // agent was installed, and one re-paired into a different session.
        let delay = match next_step(&crate::pairing::load_or_empty()) {
            Step::Follow(ticket) => {
                if waiting {
                    println!("p2pmux fleet agent: paired — joining its home session");
                    waiting = false;
                }
                match ensure_home_session(&ticket).await {
                    Ok(()) => {
                        if reported.take().is_some() {
                            println!("p2pmux fleet agent: back in its home session");
                        }
                        backoff.succeeded();
                    }
                    // Reported and retried rather than fatal. The usual reason is a
                    // network that is not up yet, and a daemon that exited on that
                    // would need the operating system to restart it into the same
                    // failure a second later.
                    Err(error) => {
                        backoff.failed();
                        let reason = error.to_string();
                        let waited = backoff.plain_interval();
                        // Once per distinct reason and once per change of pace,
                        // rather than once per attempt. The same sentence 1014
                        // times in one journal is how this failure hid: nothing
                        // in it said the agent was doing anything unusual, and
                        // the count was the only part that mattered.
                        if reported.as_ref() != Some(&(reason.clone(), waited)) {
                            eprintln!(
                                "p2pmux fleet agent: {reason} (retrying every {})",
                                describe(waited)
                            );
                            reported = Some((reason, waited));
                        }
                    }
                }
                backoff.interval()
            }
            // Not an error, and above all not an exit. `Restart=always` turns an
            // exit here into a process every five seconds for as long as the
            // machine is up -- which is what an unpaired box got, silently,
            // after `p2pmux daemon install` told it everything was fine.
            // Waiting is also the more useful behaviour: install the agent
            // whenever, pair whenever, and it starts working at the later of
            // the two.
            Step::WaitForPairing => {
                if !waiting {
                    println!(
                        "p2pmux fleet agent: this machine is not in a fleet yet — waiting for \
                         `p2pmux pair`"
                    );
                    waiting = true;
                }
                // Nothing has failed, so nothing is backing off: an unpaired
                // machine is idle, not broken, and should notice a `p2pmux pair`
                // promptly.
                backoff.succeeded();
                reported = None;
                SUPERVISION_INTERVAL
            }
        };
        tokio::select! {
            _ = tokio::time::sleep(delay) => {}
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

/// Whether the program this process is running from is still on disk.
///
/// An upgrade replaces the binary under the running agent, and the agent keeps
/// executing the old image — which on Linux is unlinked the moment it is
/// replaced, so every attempt to launch a node fails with `ENOENT` for as long
/// as the process lives. That is not a hypothetical: on 2026-08-16 the binary
/// was replaced at 12:45:52 and the agent that survived it made 1014 doomed
/// attempts over the next four hours, one every fifteen seconds, without ever
/// recovering. `Restart=` cannot help, because the process never exits.
///
/// So the agent notices and stands down, and the service manager starts it
/// again from whatever is at that path now. Both platforms are covered by the
/// same question for different reasons: Linux reports the running image's path
/// as `<path> (deleted)` once it is unlinked, whichever way it was replaced;
/// macOS reports the real path, and Homebrew removes the whole versioned
/// directory it lived in.
///
/// Takes the path rather than asking, so the decision can be tested without an
/// upgrade. An unanswerable question is answered "no": an agent that exited
/// because it could not tell would be a restart loop.
fn program_was_replaced(program: &io::Result<PathBuf>) -> bool {
    match program {
        Ok(path) => !path.exists(),
        Err(_) => false,
    }
}

/// A delay as somebody reading a log would say it.
fn describe(delay: Duration) -> String {
    let seconds = delay.as_secs();
    if seconds < 60 {
        return format!("{seconds}s");
    }
    let minutes = seconds / 60;
    match seconds % 60 {
        0 => format!("{minutes}m"),
        rest => format!("{minutes}m{rest}s"),
    }
}

/// Start the home session's node if it is not already running.
async fn ensure_home_session(ticket: &str) -> Result<(), Box<dyn Error>> {
    // Tethered: this node is the machine's presence in the fleet, and the agent
    // is the only thing watching it. One that outlived the agent would be
    // exactly the orphan this whole file is now shaped around not producing.
    if crate::node::follow_fleet_invite(ticket, crate::node::Tether::ToLauncher)? {
        println!("p2pmux fleet agent: rejoined the home session");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{
        Backoff, MAX_RETRY_INTERVAL, MEMORY_HIGH_MB, MEMORY_MAX_MB, SERVICE_LABEL,
        SUPERVISION_INTERVAL, ServiceUnit, Step, describe, next_step,
    };

    /// An upgrade replaces the binary under the running agent, and the old
    /// image cannot launch anything ever again.
    ///
    /// This is how the incident started: the binary was replaced at 12:45:52 and
    /// the agent that survived it spent the next four hours failing every
    /// fifteen seconds with `No such file or directory`. `Restart=` never fired,
    /// because the process was healthy — it just could not do the one thing it
    /// existed to do.
    #[test]
    fn an_agent_whose_binary_was_replaced_stands_down() {
        let dir = std::env::temp_dir().join(format!("p2pmux-exe-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("scratch dir");
        let program = dir.join("p2pmux");
        std::fs::write(&program, b"#!/bin/true\n").expect("fixture");

        assert!(
            !super::program_was_replaced(&Ok(program.clone())),
            "a binary that is still there is not a reason to stop"
        );

        // Linux reports the running image as "<path> (deleted)" once it has been
        // replaced, whichever way; macOS just stops having the path, because
        // Homebrew removes the whole versioned directory.
        std::fs::remove_file(&program).expect("replace it");
        assert!(super::program_was_replaced(&Ok(program.clone())));
        assert!(super::program_was_replaced(&Ok(std::path::PathBuf::from(
            format!("{} (deleted)", program.display())
        ))));

        // And an unanswerable question is not a reason to exit: an agent that
        // stood down because it could not tell would be a restart loop.
        assert!(!super::program_was_replaced(&Err(std::io::Error::other(
            "no /proc"
        ))));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The check has to come before the work, not after it.
    #[test]
    fn the_binary_check_is_the_first_thing_each_tick() {
        let source = include_str!("daemon.rs");
        let loop_body = source
            .split_once("pub async fn run()")
            .expect("the agent loop")
            .1
            .split_once("#[cfg(test)]")
            .expect("the tests, which are not the loop")
            .0;
        let checked = loop_body.find("program_was_replaced(").expect("the check");
        let acted = loop_body.find("next_step(").expect("the work");
        assert!(
            checked < acted,
            "deciding which session to join with a binary that cannot start is wasted work"
        );
    }

    /// A session that cannot be reached must not be asked about at the same rate
    /// forever.
    ///
    /// The number that matters is the one at the end: on 2026-08-16 a pair of
    /// machines chasing a session neither of them hosted made 1014 attempts in
    /// four hours, one process each. Under this the same four hours is fewer
    /// than thirty.
    #[test]
    fn a_session_that_cannot_be_reached_is_asked_about_less_and_less() {
        let mut backoff = Backoff::new(SUPERVISION_INTERVAL, MAX_RETRY_INTERVAL);
        assert_eq!(
            backoff.plain_interval(),
            SUPERVISION_INTERVAL,
            "a healthy agent keeps the ordinary cadence"
        );

        let mut seen = Vec::new();
        for _ in 0..12 {
            backoff.failed();
            seen.push(backoff.plain_interval());
        }

        assert_eq!(seen[0], SUPERVISION_INTERVAL, "the first retry is prompt");
        assert!(
            seen.windows(2).all(|pair| pair[1] >= pair[0]),
            "the delay never shrinks while the failure persists: {seen:?}"
        );
        assert_eq!(
            *seen.last().expect("a delay"),
            MAX_RETRY_INTERVAL,
            "and it settles at the ceiling rather than growing without bound"
        );

        // How many attempts four hours buys, which is the whole point.
        let mut elapsed = Duration::ZERO;
        let mut attempts = 0;
        let mut counting = Backoff::new(SUPERVISION_INTERVAL, MAX_RETRY_INTERVAL);
        while elapsed < Duration::from_secs(4 * 60 * 60) {
            counting.failed();
            elapsed += counting.plain_interval();
            attempts += 1;
        }
        assert!(
            attempts < 60,
            "four hours of an unreachable session should cost tens of attempts, not \
             a thousand: got {attempts}"
        );
        // And the worst case a returning session waits before being noticed,
        // which is the cost side of the same number.
        assert!(
            MAX_RETRY_INTERVAL <= Duration::from_secs(5 * 60),
            "a machine missing from its fleet for longer than this is a regression \
             in the thing the daemon is for"
        );
    }

    /// A session that comes back is noticed at the ordinary cadence, not at
    /// whatever the agent had backed off to.
    #[test]
    fn a_session_that_returns_resets_the_pace() {
        let mut backoff = Backoff::new(SUPERVISION_INTERVAL, MAX_RETRY_INTERVAL);
        for _ in 0..10 {
            backoff.failed();
        }
        assert!(backoff.plain_interval() > SUPERVISION_INTERVAL);

        backoff.succeeded();
        assert_eq!(backoff.plain_interval(), SUPERVISION_INTERVAL);
    }

    /// Every machine in a fleet loses the same coordinator at the same instant,
    /// so an undithered delay would bring them all back at once.
    #[test]
    fn the_delay_is_spread_out_but_never_collapses() {
        let mut backoff = Backoff::new(SUPERVISION_INTERVAL, MAX_RETRY_INTERVAL);
        for _ in 0..5 {
            backoff.failed();
        }
        let plain = backoff.plain_interval();

        let earliest = backoff.interval_at(0.0);
        let latest = backoff.interval_at(1.0);
        assert!(
            earliest >= plain / 2,
            "an unlucky draw must not retry almost immediately: {earliest:?} of {plain:?}"
        );
        assert!(latest <= plain, "and must not exceed the delay it dithers");
        assert!(earliest < latest, "there is a band to land in");

        // A healthy agent has nothing to spread out and keeps a steady beat.
        backoff.succeeded();
        assert_eq!(backoff.interval_at(0.0), backoff.interval_at(1.0));
    }

    #[test]
    fn delays_are_written_the_way_somebody_would_say_them() {
        assert_eq!(describe(Duration::from_secs(15)), "15s");
        assert_eq!(describe(Duration::from_secs(60)), "1m");
        assert_eq!(describe(Duration::from_secs(90)), "1m30s");
        assert_eq!(describe(MAX_RETRY_INTERVAL), "5m");
    }

    /// The agent says what it is doing once, not once per attempt.
    ///
    /// The failure that took a machine down wrote the same sentence to the
    /// journal 1014 times, and nothing in it said anything was wrong.
    #[test]
    fn a_repeated_failure_is_not_repeated_into_the_log() {
        let source = include_str!("daemon.rs");
        // Cut at the test module, or this reads its own assertions back and
        // every one of them passes for the wrong reason.
        let loop_body = source
            .split_once("pub async fn run()")
            .expect("the agent loop")
            .1
            .split_once("#[cfg(test)]")
            .expect("the tests, which are not the loop")
            .0;
        assert!(
            loop_body.contains("if reported.as_ref() != Some(&(reason.clone(), waited))"),
            "the same reason at the same pace must not be reported twice"
        );
        assert!(
            loop_body.contains("backoff.interval()"),
            "the loop has to actually wait for the backoff it computed"
        );
        assert!(
            !loop_body.contains("tokio::time::sleep(SUPERVISION_INTERVAL)"),
            "a fixed sleep would make the backoff decorative"
        );
    }

    fn unit() -> ServiceUnit {
        ServiceUnit {
            program: String::from("/usr/local/bin/p2pmux"),
            log_path: String::from("/home/me/.p2pmux-fleet.log"),
        }
    }

    /// An agent with no fleet to join waits for one. It does not exit.
    ///
    /// The unit says `Restart=always`, so exiting is not "stopping" -- it is a
    /// process every five seconds until somebody notices. A fresh droplet that
    /// ran `p2pmux daemon install` before `p2pmux pair`, which is the order the
    /// install output invites, got exactly that: four restarts in the first
    /// thirty seconds, while the command that installed it had said "This
    /// machine now rejoins its home session at boot".
    #[test]
    fn an_unpaired_machine_waits_instead_of_exiting() {
        let mut pairing = crate::pairing::Pairing::default();
        assert_eq!(next_step(&pairing), Step::WaitForPairing);

        // And picks the fleet up when one appears, without being reinstalled.
        pairing.ticket = Some(String::from("p2pmux-v3:whatever"));
        assert_eq!(
            next_step(&pairing),
            Step::Follow(String::from("p2pmux-v3:whatever")),
            "a machine paired after the agent was installed is still its fleet"
        );
    }

    /// A limit that silently does not apply is worse than no limit, because it
    /// is the one thing between a leak and the rest of the machine.
    ///
    /// `MemoryMax` in a user unit needs the memory controller delegated to the
    /// user's own systemd manager. That has been the default for years, which is
    /// exactly the kind of assumption that is worth one file read to stop
    /// assuming.
    #[test]
    fn the_install_says_so_when_the_ceiling_will_not_be_enforced() {
        let source = include_str!("daemon.rs");
        let install = source
            .split_once("pub fn install()")
            .expect("the installer")
            .1
            .split_once("\n}")
            .expect("the end of it")
            .0;
        assert!(
            install.contains("memory_limit_warning()"),
            "the install has to check, not assume: {install}"
        );

        // And say it rather than fail on it: an agent without a ceiling is still
        // worth installing, and refusing over a kernel setting the user did not
        // choose helps nobody.
        assert!(
            !install.contains("return Err") && !install.contains("?;\n    Err"),
            "a missing controller is a warning, not a refusal: {install}"
        );
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
        assert!(systemd.contains("Restart=on-failure"), "{systemd}");
        assert!(systemd.contains("WantedBy=default.target"), "{systemd}");
    }

    /// A service the user cannot stop is the single loudest thing software can
    /// do to say it is not on their side.
    ///
    /// The daemon exits zero when it is signalled. `Restart=always` and a bare
    /// `KeepAlive` both restart through that, so `systemctl --user stop` and
    /// `launchctl unload` were suggestions.
    #[test]
    fn asking_the_agent_to_stop_stops_it() {
        let systemd = unit().systemd_unit();
        assert!(
            !systemd.contains("Restart=always"),
            "a clean exit must stay stopped: {systemd}"
        );

        let plist = unit().launchd_plist();
        let keep_alive = plist
            .split_once("<key>KeepAlive</key>")
            .expect("the restart policy")
            .1;
        assert!(
            keep_alive.trim_start().starts_with("<dict>"),
            "a bare <true/> restarts a job that was asked to stop: {plist}"
        );
        assert!(
            keep_alive.contains("<key>SuccessfulExit</key>"),
            "the dictionary has to say which exits are worth restarting: {plist}"
        );
    }

    /// The ceiling that makes every bug above it survivable.
    ///
    /// Without it the kernel's out-of-memory killer chooses a victim from the
    /// whole machine by size. On 2026-08-16 that meant p2pmux ate the memory and
    /// a trading bot, an API server and a message gateway were killed for it.
    /// Inside `MemoryMax` the cgroup's own killer can only choose from the
    /// processes that are over the limit.
    #[test]
    fn the_linux_unit_cannot_take_the_machine_down_with_it() {
        let systemd = unit().systemd_unit();
        for required in [
            "MemoryMax=512M",
            "MemoryHigh=256M",
            "TasksMax=64",
            "OOMPolicy=stop",
            "KillMode=control-group",
        ] {
            assert!(
                systemd.contains(required),
                "missing {required} from:\n{systemd}"
            );
        }
        assert!(
            systemd.contains("MemoryHigh=256M") && MEMORY_HIGH_MB < MEMORY_MAX_MB,
            "the soft limit has to be crossed first, or it is not a warning"
        );
        // A hard processor cap would make a terminal somebody is typing in
        // stutter. A weight only yields when something else wants the CPU.
        assert!(systemd.contains("CPUWeight="), "{systemd}");
        assert!(
            !systemd.contains("CPUQuota="),
            "a quota would throttle interactive work: {systemd}"
        );
    }

    /// macOS has no cgroups, so the plist cannot promise a memory ceiling — and
    /// must not pretend to. What it can do is declare what this work *is*.
    ///
    /// `NumberOfProcesses` is the trap here: it looks like a per-job task limit
    /// and is in fact `RLIMIT_NPROC`, which is per *user*. Setting it to
    /// something sensible for one agent would cap the whole login session.
    #[test]
    fn the_macos_plist_declares_itself_background_and_sets_no_per_user_limits() {
        let plist = unit().launchd_plist();
        assert!(
            plist.contains("<key>ProcessType</key>")
                && plist.contains("<string>Background</string>"),
            "{plist}"
        );
        assert!(plist.contains("<key>ThrottleInterval</key>"), "{plist}");
        assert!(
            !plist.contains("NumberOfProcesses"),
            "that rlimit is per-uid and would cap the user's whole session: {plist}"
        );
    }

    /// The restart policy has to be slower than the thing it restarts.
    ///
    /// A five-second `RestartSec` under a daemon whose own supervision tick is
    /// fifteen meant a crash loop ran three times faster than the work.
    #[test]
    fn the_restart_pace_matches_the_agents_own() {
        let systemd = unit().systemd_unit();
        assert!(
            systemd.contains(&format!("RestartSec={}", SUPERVISION_INTERVAL.as_secs())),
            "{systemd}"
        );
        assert!(
            systemd.contains(&format!(
                "RestartMaxDelaySec={}",
                MAX_RETRY_INTERVAL.as_secs()
            )),
            "a crash loop should back off the same way a failed join does: {systemd}"
        );
        let plist = unit().launchd_plist();
        assert!(
            plist.contains(&format!(
                "<integer>{}</integer>",
                SUPERVISION_INTERVAL.as_secs()
            )),
            "launchd throttles at 10s by default, which is faster than the tick: {plist}"
        );
    }

    /// Which `[section]` each key of the generated unit was written under.
    fn systemd_sections() -> Vec<(String, String)> {
        let unit = unit().systemd_unit();
        let mut section = String::new();
        let mut placed = Vec::new();
        for line in unit.lines().map(str::trim) {
            if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
                section = name.to_owned();
            } else if let Some((key, _)) = line.split_once('=') {
                placed.push((section.clone(), key.to_owned()));
            }
        }
        placed
    }

    /// systemd ignores a key written under the wrong section, and says so only
    /// in the journal of whoever installed it.
    ///
    /// `StartLimitIntervalSec` and `StartLimitBurst` sat under `[Service]`,
    /// where systemd has not read them since v229. Both were accepted by the
    /// old assertion — it asked whether the text appeared anywhere in the file,
    /// which is true of a key in a section that ignores it. So the rate limit
    /// the 16 Aug leak fix leaned on was never enforced on any machine, and
    /// every `daemon install` logged `Unknown key name 'StartLimitIntervalSec'
    /// in section 'Service', ignoring`.
    ///
    /// Asserted as a whole table rather than for those two keys: the next key
    /// added to this unit is the one that will be misplaced, and it should fail
    /// here rather than in somebody's journal.
    #[test]
    fn every_unit_key_is_written_under_the_section_systemd_reads_it_from() {
        // From systemd.unit(5), systemd.service(5) and systemd.resource-control(5).
        let expected = [
            ("Description", "Unit"),
            ("After", "Unit"),
            ("StartLimitIntervalSec", "Unit"),
            ("StartLimitBurst", "Unit"),
            ("Type", "Service"),
            ("ExecStart", "Service"),
            ("Restart", "Service"),
            ("RestartSec", "Service"),
            ("RestartSteps", "Service"),
            ("RestartMaxDelaySec", "Service"),
            ("MemoryHigh", "Service"),
            ("MemoryMax", "Service"),
            ("TasksMax", "Service"),
            ("CPUWeight", "Service"),
            ("OOMPolicy", "Service"),
            ("KillMode", "Service"),
            ("TimeoutStopSec", "Service"),
            ("WantedBy", "Install"),
        ];
        let placed = systemd_sections();
        for (section, key) in &placed {
            let Some((_, wanted)) = expected.iter().find(|(name, _)| name == key) else {
                panic!(
                    "{key} is new to this unit; add it to the table with the section \
                     systemd.unit(5) says it belongs in"
                );
            };
            assert_eq!(
                section, wanted,
                "systemd reads {key} from [{wanted}] and ignores it under [{section}]"
            );
        }
        for (key, _) in expected {
            assert!(
                placed.iter().any(|(_, placed)| placed == key),
                "{key} is in the table but no longer in the unit"
            );
        }
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
