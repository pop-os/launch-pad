// SPDX-License-Identifier: MPL-2.0

//! Detection of process deaths that `waitpid` cannot report.
//!
//! Supervising a child by awaiting its exit status covers every *ordinary*
//! death, but there is one state it cannot see. When a task takes a fatal fault
//! the kernel runs `do_exit()` on that task alone — not `do_group_exit()` — so
//! if the victim happens to be the thread-group leader, the leader becomes a
//! zombie while its sibling threads keep running.
//!
//! Such a leader can never be reaped: `delay_group_leader()` holds it until the
//! last thread in the group exits. `waitpid`/`Child::wait` therefore blocks
//! forever, and a supervisor that relies on it alone never learns the process is
//! effectively dead.
//!
//! Nothing else detects this either. Empirically, on Linux 7.0:
//!
//! | mechanism | detects a delayed group leader? |
//! |---|---|
//! | `waitpid`, `Child::wait` | no — blocks forever |
//! | `waitid` with `WNOWAIT` | no — reports nothing |
//! | `pidfd_open` + `poll` | no — never becomes readable |
//! | `/proc/<pid>/status` | **yes** |
//!
//! pidfd and `waitid` both key off reapability, so they inherit the same
//! `delay_group_leader()` behaviour. Reading procfs is the only way to see it,
//! which is why this module polls rather than waiting on a descriptor.

/// Reports whether a process is dead in a way `waitpid` will never report.
///
/// Returns the process's thread count when `pid`'s group leader has exited
/// while its siblings are still running, and `None` otherwise.
///
/// A healthy process and an *ordinary* exit both yield `None`: on a normal
/// teardown every sibling is already gone by the time the leader zombies, so
/// `Threads:` is 1 and `waitpid` returns immediately. Requiring more than one
/// thread is what keeps this from firing during routine shutdown.
///
/// Reads procfs synchronously. `/proc/<pid>/status` is generated in-kernel on
/// read with no disk or network behind it, so this does not block in the sense
/// an async runtime cares about, and keeping it sync avoids obliging every
/// consumer to enable tokio's `fs` feature.
#[cfg(target_os = "linux")]
pub fn stuck_zombie_leader(pid: u32) -> Option<u64> {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    parse_status(&status)
}

/// Always `None` off Linux, where there is no procfs to consult.
#[cfg(not(target_os = "linux"))]
pub fn stuck_zombie_leader(_pid: u32) -> Option<u64> {
    None
}

/// Splits the parsing out of the I/O so it can be tested against captured
/// `/proc/<pid>/status` text.
fn parse_status(status: &str) -> Option<u64> {
    let mut leader_is_zombie = false;
    let mut threads = 0;

    for line in status.lines() {
        if let Some(state) = line.strip_prefix("State:") {
            // "State:\tZ (zombie)"
            leader_is_zombie = state.trim_start().starts_with('Z');
        } else if let Some(count) = line.strip_prefix("Threads:") {
            threads = count.trim().parse().unwrap_or(0);
        }
    }

    (leader_is_zombie && threads > 1).then_some(threads)
}

#[cfg(test)]
mod tests {
    use super::parse_status;

    /// The pathological state: leader gone, siblings still running.
    #[test]
    fn detects_zombie_leader_with_live_threads() {
        let status = "Name:\tcosmic-comp\nState:\tZ (zombie)\nTgid:\t3237\nPid:\t3237\nThreads:\t34\n";
        assert_eq!(parse_status(status), Some(34));
    }

    /// An ordinary exit: the leader zombies only once it is alone, and
    /// `waitpid` reports it, so this must not fire.
    #[test]
    fn ignores_ordinary_zombie() {
        let status = "Name:\tsh\nState:\tZ (zombie)\nThreads:\t1\n";
        assert_eq!(parse_status(status), None);
    }

    #[test]
    fn ignores_healthy_process() {
        let status = "Name:\tcosmic-comp\nState:\tS (sleeping)\nThreads:\t31\n";
        assert_eq!(parse_status(status), None);
    }

    /// A multi-threaded process being traced or stopped is not dead.
    #[test]
    fn ignores_other_states() {
        for state in ["R (running)", "D (disk sleep)", "t (tracing stop)", "T (stopped)"] {
            let status = format!("State:\t{state}\nThreads:\t8\n");
            assert_eq!(parse_status(&status), None, "state {state} must not trip");
        }
    }

    /// Truncated or unexpected content must never panic or false-positive.
    #[test]
    fn tolerates_malformed_input() {
        assert_eq!(parse_status(""), None);
        assert_eq!(parse_status("State:\tZ (zombie)\n"), None, "no Threads line");
        assert_eq!(parse_status("Threads:\t9\n"), None, "no State line");
        assert_eq!(parse_status("State:\tZ (zombie)\nThreads:\tbogus\n"), None);
    }
}
