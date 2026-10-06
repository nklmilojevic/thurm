//! The process group of the agent that runs a hook.
//!
//! Agents may start hooks detached (Claude Code: in a new session, without the terminal), so
//! the hook's own group is not the agent's. The agent is the nearest ancestor on the pane's
//! terminal.

/// One process: its group and its terminal's foreground group (`None`: no terminal).
#[derive(Debug, Clone, Copy, PartialEq)]
struct Proc {
    ppid: u32,
    pgid: u32,
    tpgid: Option<u32>,
}

/// The agent's group: the nearest ancestor (or this process) in its terminal's foreground,
/// else the nearest one on a terminal (the agent left the foreground before the hook came),
/// else this process' own group.
pub fn agent_pgrp() -> u32 {
    let own = unsafe { libc::getpgrp() } as u32;
    pick(ancestry(std::process::id()), own)
}

fn ancestry(mut pid: u32) -> Vec<Proc> {
    let mut out = Vec::new();
    while pid > 1 && out.len() < 64 {
        let Some(p) = proc_info(pid) else { break };
        out.push(p);
        pid = p.ppid;
    }
    out
}

fn pick(chain: Vec<Proc>, own: u32) -> u32 {
    chain
        .iter()
        .find(|p| p.tpgid == Some(p.pgid))
        .or_else(|| chain.iter().find(|p| p.tpgid.is_some()))
        .map_or(own, |p| p.pgid)
}

#[cfg(target_os = "macos")]
fn proc_info(pid: u32) -> Option<Proc> {
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of_val(&info) as i32;
    let n = unsafe {
        libc::proc_pidinfo(
            pid as i32,
            libc::PROC_PIDTBSDINFO,
            0,
            (&mut info as *mut libc::proc_bsdinfo).cast(),
            size,
        )
    };
    // No terminal: e_tdev is NODEV and e_tpgid 0.
    let tty = info.e_tdev != u32::MAX && info.e_tpgid != 0;
    (n == size).then_some(Proc {
        ppid: info.pbi_ppid,
        pgid: info.pbi_pgid,
        tpgid: tty.then_some(info.e_tpgid),
    })
}

#[cfg(target_os = "linux")]
fn proc_info(pid: u32) -> Option<Proc> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // After the name: state ppid pgrp session tty_nr tpgid.
    let f: Vec<&str> = stat
        .rsplit_once(')')?
        .1
        .split_whitespace()
        .take(6)
        .collect();
    let num = |i: usize| f.get(i)?.parse::<i64>().ok();
    let tty = num(4)? != 0;
    let tpgid = num(5)?;
    Some(Proc {
        ppid: num(1)? as u32,
        pgid: num(2)? as u32,
        tpgid: (tty && tpgid > 0).then_some(tpgid as u32),
    })
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn proc_info(_pid: u32) -> Option<Proc> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(pgid: u32, tpgid: Option<u32>) -> Proc {
        Proc {
            ppid: 0,
            pgid,
            tpgid,
        }
    }

    #[test]
    fn detached_hook_resolves_to_the_foreground_agent() {
        // thurm and `sh -c` in their own session, under claude (group 10) on the terminal.
        let chain = vec![p(30, None), p(30, None), p(10, Some(10)), p(5, Some(10))];
        assert_eq!(pick(chain, 30), 10);
    }

    #[test]
    fn hook_in_the_agent_group_keeps_it() {
        let chain = vec![p(10, Some(10)), p(10, Some(10)), p(5, Some(10))];
        assert_eq!(pick(chain, 10), 10);
    }

    #[test]
    fn agent_out_of_the_foreground_is_the_nearest_on_the_terminal() {
        // `git pull` (group 40) took the foreground before the hook arrived.
        let chain = vec![p(30, None), p(10, Some(40)), p(5, Some(40))];
        assert_eq!(pick(chain, 30), 10);
    }

    #[test]
    fn no_terminal_anywhere_falls_back_to_the_own_group() {
        assert_eq!(pick(vec![p(30, None)], 30), 30);
        assert_eq!(pick(Vec::new(), 30), 30);
    }

    #[test]
    fn reads_this_process() {
        let me = proc_info(std::process::id()).expect("own process info");
        assert_eq!(me.pgid, unsafe { libc::getpgrp() } as u32);
    }
}
