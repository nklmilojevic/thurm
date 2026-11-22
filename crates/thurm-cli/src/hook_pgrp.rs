//! The process group of the agent that runs a hook.
//!
//! Agents may start hooks detached (Claude Code: in a new session, without the terminal), in
//! their own process group on the pane's terminal (`setpgid`, e.g. Antigravity), or in a child
//! PTY, so the hook's own group is not always the agent's. The agent is the process group on the
//! pane's terminal that spawned the hook.

/// One process: its parent, its group, its controlling terminal (`tdev` and foreground `tpgid`),
/// and whether it is a shell wrapper or a PTY proxy (`atuin`, `script`).
#[derive(Debug, Clone, Copy, PartialEq)]
struct Proc {
    ppid: u32,
    pgid: u32,
    tdev: Option<u32>,
    tpgid: Option<u32>,
    shell: bool,
    proxy: bool,
}

/// The agent's group: that of the agent on its controlling terminal, else this process' own.
/// Whether it is in the foreground doesn't matter: a backgrounded agent's shell is, and is
/// farther up.
pub fn agent_pgrp() -> u32 {
    let own = unsafe { libc::getpgrp() } as u32;
    pick(&ancestry(std::process::id()), own)
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

/// The controlling terminal the agent itself runs on.
///
/// Walking bottom-up (rather than taking the outermost ancestor's terminal) stays on the inner
/// terminal when a PTY proxy (`atuin`, `script`) sits between the pane and the shell, while
/// still stepping out of a child PTY an agent spawned only to run the hook.
fn agent_tdev(chain: &[Proc], own: u32) -> Option<u32> {
    let first = chain.first()?;
    let Some(hook_tdev) = first.tdev else {
        return chain.iter().skip(1).find_map(|p| p.tdev);
    };
    let hook_only_tty = chain
        .iter()
        .skip(1)
        .filter(|p| p.tdev == Some(hook_tdev))
        .all(|p| p.shell && p.pgid == own);
    if hook_only_tty
        && let Some(parent) = chain
            .iter()
            .skip(1)
            .find(|p| p.tdev.is_some_and(|t| t != hook_tdev))
        && !parent.proxy
    {
        return parent.tdev;
    }
    Some(hook_tdev)
}

fn pick(chain: &[Proc], own: u32) -> u32 {
    let Some(agent_tdev) = agent_tdev(chain, own) else {
        return own;
    };
    let on_tty: Vec<&Proc> = chain
        .iter()
        .filter(|p| p.tdev == Some(agent_tdev))
        .collect();
    let Some(first) = on_tty.first() else {
        return own;
    };
    // Hook ran detached (`setsid`, no terminal) or in a child PTY: the nearest ancestor on the
    // agent's terminal is the agent.
    if chain.first().and_then(|p| p.tdev) != Some(agent_tdev) {
        return first.pgid;
    }
    // Hook shares the agent's terminal and is already in its foreground group.
    if first.tpgid == Some(first.pgid) {
        return first.pgid;
    }
    // Hook ran in a background group on the agent's terminal (`setpgid(0, 0)`): if a non-shell
    // ancestor holds the terminal's foreground, that is the agent that spawned it.
    if let Some(fg) = on_tty.iter().find(|p| p.tpgid == Some(p.pgid) && !p.shell) {
        return fg.pgid;
    }
    // Backgrounded agent: if every ancestor sharing the hook's own group is a shell wrapper
    // (`sh -c` under `setpgid`), the agent is the next group up on the agent's terminal.
    let wrapper_group = on_tty
        .iter()
        .skip(1)
        .filter(|p| p.pgid == own)
        .all(|p| p.shell);
    if wrapper_group && let Some(parent) = on_tty.iter().find(|p| p.pgid != own) {
        return parent.pgid;
    }
    first.pgid
}

fn is_shell_name(name: &str) -> bool {
    let base = name.rsplit('/').next().unwrap_or(name);
    let base = base.strip_prefix('-').unwrap_or(base);
    matches!(
        base,
        "sh" | "bash"
            | "zsh"
            | "fish"
            | "dash"
            | "ash"
            | "ksh"
            | "mksh"
            | "csh"
            | "tcsh"
            | "nu"
            | "elvish"
            | "xonsh"
            | "pwsh"
            | "login"
    )
}

fn is_proxy_name(name: &str) -> bool {
    let base = name.rsplit('/').next().unwrap_or(name);
    matches!(base, "atuin" | "script")
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
    if n != size {
        return None;
    }
    // No terminal: e_tdev is NODEV and e_tpgid 0.
    let has_tty = info.e_tdev != u32::MAX && info.e_tpgid != 0;
    let comm_bytes: Vec<u8> = info
        .pbi_comm
        .iter()
        .take_while(|&&c| c != 0)
        .map(|&c| c as u8)
        .collect();
    let comm = String::from_utf8_lossy(&comm_bytes);
    Some(Proc {
        ppid: info.pbi_ppid,
        pgid: info.pbi_pgid,
        tdev: has_tty.then_some(info.e_tdev),
        tpgid: has_tty.then_some(info.e_tpgid),
        shell: is_shell_name(&comm),
        proxy: is_proxy_name(&comm),
    })
}

#[cfg(target_os = "linux")]
fn proc_info(pid: u32) -> Option<Proc> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let open = stat.find('(')?;
    let close = stat.rfind(')')?;
    let comm = &stat[open + 1..close];
    // After the name: state ppid pgrp session tty_nr tpgid.
    let f: Vec<&str> = stat[close + 1..].split_whitespace().take(6).collect();
    let num = |i: usize| f.get(i)?.parse::<i64>().ok();
    let tty_nr = num(4)?;
    let tpgid = num(5)?;
    let has_tty = tty_nr != 0 && tpgid > 0;
    Some(Proc {
        ppid: num(1)? as u32,
        pgid: num(2)? as u32,
        tdev: has_tty.then_some(tty_nr as u32),
        tpgid: has_tty.then_some(tpgid as u32),
        shell: is_shell_name(comm),
        proxy: is_proxy_name(comm),
    })
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn proc_info(_pid: u32) -> Option<Proc> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    const PANE_TTY: u32 = 100;
    const PROXY_TTY: u32 = 150;
    const CHILD_TTY: u32 = 200;

    fn p(pgid: u32, tdev: Option<u32>, tpgid: Option<u32>, shell: bool) -> Proc {
        Proc {
            ppid: 0,
            pgid,
            tdev,
            tpgid,
            shell,
            proxy: false,
        }
    }

    fn proxy(pgid: u32, tdev: Option<u32>, tpgid: Option<u32>) -> Proc {
        Proc {
            proxy: true,
            ..p(pgid, tdev, tpgid, false)
        }
    }

    #[test]
    fn detached_hook_resolves_to_the_agent() {
        // thurm and `sh -c` in their own session, under claude (group 10) and its shell (5).
        let chain = vec![
            p(30, None, None, false),
            p(30, None, None, true),
            p(10, Some(PANE_TTY), Some(10), false),
            p(5, Some(PANE_TTY), Some(10), true),
        ];
        assert_eq!(pick(&chain, 30), 10);
    }

    #[test]
    fn hook_in_the_agent_group_keeps_it() {
        let chain = vec![
            p(10, Some(PANE_TTY), Some(10), false),
            p(10, Some(PANE_TTY), Some(10), false),
            p(5, Some(PANE_TTY), Some(10), true),
        ];
        assert_eq!(pick(&chain, 10), 10);
    }

    #[test]
    fn setpgid_hook_on_same_terminal_resolves_to_foreground_agent() {
        // Antigravity spawns `sh -c thurm` with Setpgid: true, keeping the pane's terminal.
        let chain = vec![
            p(30, Some(PANE_TTY), Some(10), false),
            p(30, Some(PANE_TTY), Some(10), true),
            p(10, Some(PANE_TTY), Some(10), false),
            p(5, Some(PANE_TTY), Some(10), true),
        ];
        assert_eq!(pick(&chain, 30), 10);
    }

    #[test]
    fn setpgid_hook_under_backgrounded_agent_resolves_to_agent() {
        // Antigravity (10) was suspended while its shell (5) holds the foreground.
        let chain = vec![
            p(30, Some(PANE_TTY), Some(5), false),
            p(30, Some(PANE_TTY), Some(5), true),
            p(10, Some(PANE_TTY), Some(5), false),
            p(5, Some(PANE_TTY), Some(5), true),
        ];
        assert_eq!(pick(&chain, 30), 10);
    }

    #[test]
    fn child_pty_hook_resolves_to_agent_on_pane_terminal() {
        // Hook spawned in a separate PTY under agy (10) on the pane's terminal.
        let chain = vec![
            p(30, Some(CHILD_TTY), Some(30), false),
            p(30, Some(CHILD_TTY), Some(30), true),
            p(10, Some(PANE_TTY), Some(10), false),
            p(5, Some(PANE_TTY), Some(10), true),
        ];
        assert_eq!(pick(&chain, 30), 10);
    }

    #[test]
    fn pty_proxy_between_pane_and_shell_resolves_to_inner_agent() {
        // login + atuin on PANE_TTY (group 2), fish (5) + agent (10) on PROXY_TTY.
        let outer = [
            p(10, Some(PROXY_TTY), Some(10), false),
            p(5, Some(PROXY_TTY), Some(10), true),
            proxy(2, Some(PANE_TTY), Some(2)),
            p(2, Some(PANE_TTY), Some(2), true),
        ];

        // Detached hook (Claude Code).
        let mut detached = vec![p(30, None, None, false), p(30, None, None, true)];
        detached.extend_from_slice(&outer);
        assert_eq!(pick(&detached, 30), 10);

        // setpgid hook on PROXY_TTY (Antigravity).
        let mut setpgid = vec![
            p(30, Some(PROXY_TTY), Some(10), false),
            p(30, Some(PROXY_TTY), Some(10), true),
        ];
        setpgid.extend_from_slice(&outer);
        assert_eq!(pick(&setpgid, 30), 10);

        // Child-PTY hook under agent (Antigravity / wrapper PTY).
        let mut child_pty = vec![
            p(30, Some(CHILD_TTY), Some(30), false),
            p(30, Some(CHILD_TTY), Some(30), true),
        ];
        child_pty.extend_from_slice(&outer);
        assert_eq!(pick(&child_pty, 30), 10);

        // Same-group hook (Pi).
        let mut same_group = vec![p(10, Some(PROXY_TTY), Some(10), false)];
        same_group.extend_from_slice(&outer);
        assert_eq!(pick(&same_group, 10), 10);

        // Manual invocation from a shell behind atuin does not jump up to atuin.
        let manual = vec![
            p(5, Some(PROXY_TTY), Some(5), false),
            p(5, Some(PROXY_TTY), Some(5), true),
            proxy(2, Some(PANE_TTY), Some(2)),
            p(2, Some(PANE_TTY), Some(2), true),
        ];
        assert_eq!(pick(&manual, 5), 5);
    }

    #[test]
    fn backgrounded_agent_beats_its_foreground_shell() {
        // The agent (10) was suspended; its shell (5) has the terminal's foreground.
        let chain = vec![
            p(30, None, None, false),
            p(10, Some(PANE_TTY), Some(5), false),
            p(5, Some(PANE_TTY), Some(5), true),
        ];
        assert_eq!(pick(&chain, 30), 10);

        // Same when the hook stayed in the backgrounded agent's own process group (Pi).
        let same_group = vec![
            p(10, Some(PANE_TTY), Some(5), false),
            p(10, Some(PANE_TTY), Some(5), false),
            p(5, Some(PANE_TTY), Some(5), true),
        ];
        assert_eq!(pick(&same_group, 10), 10);
    }

    #[test]
    fn no_terminal_anywhere_falls_back_to_the_own_group() {
        assert_eq!(pick(&[p(30, None, None, false)], 30), 30);
        assert_eq!(pick(&[], 30), 30);
    }

    #[test]
    fn reads_this_process() {
        let me = proc_info(std::process::id()).expect("own process info");
        assert_eq!(me.pgid, unsafe { libc::getpgrp() } as u32);
    }
}
