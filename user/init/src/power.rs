//! UL-7: restart and power off, the parts of init's side that need no system calls (unit-tested on the host).
//!
//! busybox init's signals (busybox init/init.c `init_main`: SIGTERM reboot, SIGUSR1 halt, SIGUSR2 power off,
//! SIGINT the ctrlaltdel action, whose default inittab line is a reboot); BitsOS has no picture-frozen halt, so
//! a halt powers off. The desktop's power menu (DS-6) asks over `POWER_SOCKET` instead, since bitscomp gives
//! root up and may not signal init. Power off only from real input (R8f): `/proc/bitsos/power` is the kernel's
//! judgement, asked before any program is ended.

pub const SIGHUP: u32 = 1;
pub const SIGINT: u32 = 2;
pub const SIGKILL: u32 = 9;
pub const SIGUSR1: u32 = 10;
pub const SIGUSR2: u32 = 12;
pub const SIGTERM: u32 = 15;
pub const SIGCONT: u32 = 18;
/// `si_code` of kill(2).
pub const SI_USER: i32 = 0;

pub const POWER_SOCKET: &str = "/run/bitsos/power.sock";
pub const POWER_FILE: &str = "/proc/bitsos/power";
/// How long programs get after SIGTERM before SIGKILL (busybox waits 1 s; bitscomp, the journal and the
/// settings registry write their state back first).
pub const TERM_WAIT_MS: u64 = 5_000;
/// How long the ends after SIGKILL may take before the write-back (B4: bounded either way).
pub const KILL_WAIT_MS: u64 = 2_000;
/// The desktop's user (bitscomp after it gave root up) and the one program of that user the socket serves.
pub const DESKTOP_UID: u32 = 1000;
pub const DESKTOP_EXE: &str = "/bin/bitscomp";
/// WY-21: the desktop on the GPU is this program (bitscomp's gpu build, beside the Mesa tree on the stick).
pub const DESKTOP_GPU_EXE: &str = "/boot/BITSOS2/bin/bitscomp-gpu";
/// A request line is at most this long ("poweroff\n" and friends).
pub const REQUEST_MAX: usize = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Restart,
    PowerOff,
}

impl Action {
    pub fn word(self) -> &'static str {
        match self {
            Action::Restart => "restart",
            Action::PowerOff => "power off",
        }
    }
}

/// The signals init takes through its signalfd (bit n-1 = signal n).
pub fn signal_mask() -> u64 {
    [SIGINT, SIGUSR1, SIGUSR2, SIGTERM].iter().fold(0, |m, s| m | 1u64 << (s - 1))
}

pub fn action_of(sig: u32) -> Option<Action> {
    match sig {
        SIGTERM | SIGINT => Some(Action::Restart),
        SIGUSR1 | SIGUSR2 => Some(Action::PowerOff),
        _ => None,
    }
}

/// A power-off signal from kill(2) with sender pid 0 comes from outside every process namespace: MCP's `kill`
/// tool (R8f: never a power-off; a restart is what MCP's `reboot` does anyway).
pub fn signal_refusal(action: Action, code: i32, pid: u32) -> Option<&'static str> {
    (action == Action::PowerOff && code == SI_USER && pid == 0).then_some("the signal came from MCP's kill tool, not from a program on the machine")
}

/// The processes of MCP's `run` that `/proc/bitsos/power` names (its line `mcp_run=1,2`): a power-off one of
/// them asks for is refused (R8f), whatever the input says.
pub fn mcp_run_pids(text: &str) -> Vec<u32> {
    text.lines()
        .find_map(|l| l.strip_prefix("mcp_run="))
        .map(|l| l.split(',').filter_map(|p| p.trim().parse().ok()).collect())
        .unwrap_or_default()
}

/// R8f for a power-off `pid` asks for (a signal's sender, the socket's peer): refused when the kernel names
/// it a process of MCP's `run`, else the kernel's input judgement.
pub fn verdict_for(text: &str, pid: u32) -> Result<(), String> {
    if mcp_run_pids(text).contains(&pid) {
        return Err(format!("pid {pid} was started by MCP's run (power off only from real input)"));
    }
    power_file_verdict(text)
}

/// `/proc/bitsos/power`'s answer: Ok when it allows a power-off, else its reason in words.
pub fn power_file_verdict(text: &str) -> Result<(), String> {
    let mut lines = text.lines();
    let first = lines.next().unwrap_or("");
    if first.split_whitespace().next() == Some("poweroff=allowed") {
        return Ok(());
    }
    let why = lines.next().map(str::trim).filter(|s| !s.is_empty()).unwrap_or("the kernel did not say why");
    Err(if first.is_empty() { String::from("no answer from the kernel") } else { why.to_string() })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Request {
    Do(Action),
    Status,
}

/// One request line of the power socket: `restart` (or `reboot`), `poweroff` (or `halt`), `status`.
pub fn parse_request(line: &str) -> Option<Request> {
    match line.trim() {
        "restart" | "reboot" => Some(Request::Do(Action::Restart)),
        "poweroff" | "halt" => Some(Request::Do(Action::PowerOff)),
        "status" => Some(Request::Status),
        _ => None,
    }
}

/// Who may ask over the socket: root, and the desktop user only through bitscomp, either build (which asks only after
/// real input, R8f; the kernel's judgement still decides a power-off).
pub fn peer_allowed(uid: u32, exe: Option<&str>) -> Result<(), String> {
    match (uid, exe) {
        (0, _) => Ok(()),
        (DESKTOP_UID, Some(DESKTOP_EXE | DESKTOP_GPU_EXE)) => Ok(()),
        (DESKTOP_UID, e) => Err(format!("user {DESKTOP_UID} asks only through {DESKTOP_EXE} (this is {})", e.unwrap_or("unknown"))),
        (u, _) => Err(format!("user {u} may not restart or power off")),
    }
}

/// The state letter of a `/proc/<pid>/stat` line (after the last ')', which ends the name).
pub fn stat_state(text: &str) -> Option<char> {
    text.rsplit_once(')')?.1.trim_start().chars().next()
}

/// Whether a process in that state still runs (zombies and dead ones only wait to be collected).
pub fn running(state: char) -> bool {
    !matches!(state, 'Z' | 'X' | 'x')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn busybox_signals() {
        assert_eq!(action_of(SIGTERM), Some(Action::Restart));
        assert_eq!(action_of(SIGINT), Some(Action::Restart));
        assert_eq!(action_of(SIGUSR1), Some(Action::PowerOff));
        assert_eq!(action_of(SIGUSR2), Some(Action::PowerOff));
        assert_eq!(action_of(SIGKILL), None);
        assert_eq!(signal_mask(), (1 << 1) | (1 << 9) | (1 << 11) | (1 << 14));
        assert!(signal_refusal(Action::PowerOff, SI_USER, 0).is_some());
        assert_eq!(signal_refusal(Action::Restart, SI_USER, 0), None);
        assert_eq!(signal_refusal(Action::PowerOff, SI_USER, 42), None);
    }

    #[test]
    fn kernel_answer() {
        assert_eq!(power_file_verdict("poweroff=allowed age_ms=3 cad=1\nthe last Enter\n"), Ok(()));
        assert_eq!(power_file_verdict("poweroff=refused why=injected-confirm cad=1\nthe Enter came from MCP\n"), Err("the Enter came from MCP".into()));
        assert_eq!(power_file_verdict("poweroff=refused why=x\n"), Err("the kernel did not say why".into()));
        assert_eq!(power_file_verdict(""), Err("no answer from the kernel".into()));
        assert!(power_file_verdict("poweroff=allowedx\n").is_err());
        let t = "poweroff=allowed age_ms=3 cad=1\nthe last Enter\nmcp_run=7,12\n";
        assert_eq!(mcp_run_pids(t), vec![7, 12]);
        assert_eq!(mcp_run_pids("poweroff=allowed\nx\nmcp_run=\n"), Vec::<u32>::new());
        assert_eq!(mcp_run_pids(""), Vec::<u32>::new());
        assert_eq!(verdict_for(t, 9), Ok(()));
        assert!(verdict_for(t, 12).is_err_and(|e| e.contains("MCP's run")));
        assert!(verdict_for("poweroff=refused why=stale\nold\nmcp_run=\n", 9).is_err());
    }

    #[test]
    fn socket_requests_and_peers() {
        assert_eq!(parse_request("poweroff\n"), Some(Request::Do(Action::PowerOff)));
        assert_eq!(parse_request(" reboot "), Some(Request::Do(Action::Restart)));
        assert_eq!(parse_request("status"), Some(Request::Status));
        assert_eq!(parse_request("shutdown -h now"), None);
        assert_eq!(peer_allowed(0, None), Ok(()));
        assert_eq!(peer_allowed(1000, Some("/bin/bitscomp")), Ok(()));
        assert_eq!(peer_allowed(1000, Some("/boot/BITSOS2/bin/bitscomp-gpu")), Ok(()));
        assert!(peer_allowed(1000, Some("/bin/sh")).is_err());
        assert!(peer_allowed(1000, None).is_err());
        assert!(peer_allowed(65534, Some("/bin/bitscomp")).is_err());
    }

    #[test]
    fn proc_stat_states() {
        assert_eq!(stat_state("12 (sh) S 1 12 12"), Some('S'));
        assert_eq!(stat_state("13 (a) b) Z 1"), Some('Z'));
        assert_eq!(stat_state("garbage"), None);
        assert!(running('S') && running('R') && !running('Z') && !running('X'));
    }
}
