//! Which children a `wait4` call waits for (pure, host-tested; Linux `kernel_wait4`).

use uapi::ab::wait::*;
use uapi::errno::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Select {
    /// pid > 0: exactly this child.
    Pid(u32),
    /// pid == -1: any child.
    Any,
    /// pid == 0: any child in the caller's process group.
    OwnGroup,
    /// pid < -1: any child in group -pid.
    Group(u32),
    /// AB-12, waitid P_PIDFD: the child with this pid that is also this process (`group`: the address of its
    /// `Group`, which the pidfd keeps alive during the wait), so a reaped child's reused pid never matches.
    Exact { pid: u32, group: usize },
}

impl Select {
    /// From wait4's pid argument (an `int`); pid == INT_MIN is ESRCH as in Linux (its negation overflows).
    pub fn from_linux(pid: i32) -> Result<Select, i64> {
        Ok(match pid {
            i32::MIN => return Err(ESRCH),
            -1 => Select::Any,
            0 => Select::OwnGroup,
            p if p < 0 => Select::Group((-p) as u32),
            p => Select::Pid(p as u32),
        })
    }
    /// The group identity an `Exact` selection also asks for.
    pub fn identity(self) -> Option<usize> {
        if let Select::Exact { group, .. } = self { Some(group) } else { None }
    }
    pub fn matches(self, child_pid: u32, child_pgid: u32, caller_pgid: u32) -> bool {
        match self {
            Select::Pid(p) | Select::Exact { pid: p, .. } => child_pid == p,
            Select::Any => true,
            Select::OwnGroup => child_pgid == caller_pgid,
            Select::Group(g) => child_pgid == g,
        }
    }
}

/// wait4's options: EINVAL for bits it does not know (WEXITED is waitid's); returns WNOHANG.
pub fn check_options(options: u64) -> Result<bool, i64> {
    if options & !WAIT4_VALID != 0 {
        return Err(EINVAL);
    }
    Ok(options & WNOHANG != 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pid_argument_like_linux() {
        assert_eq!(Select::from_linux(-1), Ok(Select::Any));
        assert_eq!(Select::from_linux(0), Ok(Select::OwnGroup));
        assert_eq!(Select::from_linux(7), Ok(Select::Pid(7)));
        assert_eq!(Select::from_linux(-7), Ok(Select::Group(7)));
        assert_eq!(Select::from_linux(i32::MIN), Err(ESRCH));
        assert!(Select::Pid(7).matches(7, 1, 1) && !Select::Pid(7).matches(8, 1, 1));
        assert!(Select::OwnGroup.matches(9, 5, 5) && !Select::OwnGroup.matches(9, 6, 5));
        assert!(Select::Group(6).matches(9, 6, 5));
        let e = Select::Exact { pid: 9, group: 0x1000 };
        assert!(e.matches(9, 1, 1) && !e.matches(8, 1, 1));
        assert_eq!(e.identity(), Some(0x1000));
        assert_eq!(Select::Pid(9).identity(), None);
    }

    #[test]
    fn options_like_linux() {
        assert_eq!(check_options(0), Ok(false));
        assert_eq!(check_options(WNOHANG | WUNTRACED | WCONTINUED | WALL), Ok(true));
        assert_eq!(check_options(WEXITED), Err(EINVAL), "WEXITED is waitid's");
        assert_eq!(check_options(0x100), Err(EINVAL));
    }
}
