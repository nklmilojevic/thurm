//! The process group of the agent that runs a hook.
//!
//! Agents may start hooks detached (Claude Code: in a new session, without the terminal), in
//! their own process group on the pane's terminal (`setpgid`, e.g. Antigravity), or in a child
//! PTY, so the hook's own group is not always the agent's. The agent is the process group on the
//! pane's terminal that spawned the hook.

use thurm_config::AgentDef;

/// One process: its parent, its group, its controlling terminal (`tdev` and foreground `tpgid`),
/// and whether it is a shell wrapper, a PTY proxy (`atuin`, `script`), or the reporting agent.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Proc {
    ppid: u32,
    pgid: u32,
    tdev: Option<u32>,
    tpgid: Option<u32>,
    shell: bool,
    proxy: bool,
    agent: bool,
}

/// The agent's group: that of the agent on its controlling terminal, else this process' own.
/// Whether it is in the foreground doesn't matter: a backgrounded agent's shell is, and is
/// farther up.
pub fn agent_pgrp(agent_kind: &str) -> u32 {
    let own = unsafe { libc::getpgrp() } as u32;
    let def = agent_def(agent_kind);
    pick(&ancestry(std::process::id(), agent_kind, def.as_ref()), own)
}

fn agent_def(kind: &str) -> Option<AgentDef> {
    if kind.is_empty() {
        return None;
    }
    let defs = thurm_config::Config::load()
        .map(|c| c.agent_defs())
        .unwrap_or_else(|_| thurm_config::builtin_agents());
    defs.into_iter().find(|d| d.kind == kind)
}

fn ancestry(mut pid: u32, agent_kind: &str, def: Option<&AgentDef>) -> Vec<Proc> {
    let self_pid = pid;
    let mut out = Vec::new();
    while pid > 1 && out.len() < 64 {
        let check_agent = pid != self_pid;
        let Some(p) = proc_info(pid, check_agent.then_some((agent_kind, def))) else {
            break;
        };
        out.push(p);
        pid = p.ppid;
    }
    out
}

/// The controlling terminal the agent runs on: the outermost terminal in `chain`, walking past
/// any outer PTY proxy layer (`atuin`, `script`, and shells) that has no non-shell, non-proxy
/// process of its own.
///
/// Starting from the outside skips any child PTY created for the hook — regardless of what
/// launched it (`sh`, `script`, `timeout`, ...) and whether the hook stayed on that child PTY
/// or detached from it — while stepping past pane PTY proxies like `atuin` and `script`.
fn agent_tdev(chain: &[Proc]) -> Option<u32> {
    let mut tdevs = Vec::new();
    for p in chain.iter().rev() {
        if let Some(td) = p.tdev
            && !tdevs.contains(&td)
        {
            tdevs.push(td);
        }
    }
    let last = *tdevs.last()?;
    for &td in &tdevs {
        if td == last {
            return Some(td);
        }
        let mut has_proxy = false;
        let mut only_shell_or_proxy = true;
        for p in chain.iter().filter(|p| p.tdev == Some(td)) {
            has_proxy |= p.proxy;
            if !p.shell && !p.proxy {
                only_shell_or_proxy = false;
                break;
            }
        }
        if !(has_proxy && only_shell_or_proxy) {
            return Some(td);
        }
    }
    Some(last)
}

fn pick(chain: &[Proc], own: u32) -> u32 {
    let Some(agent_tdev) = agent_tdev(chain) else {
        return own;
    };
    let on_tty: Vec<&Proc> = chain
        .iter()
        .filter(|p| p.tdev == Some(agent_tdev))
        .collect();
    let Some(first) = on_tty.first() else {
        return own;
    };
    // Hook ran detached (`setsid`, no terminal) or in a child PTY: prefer the nearest
    // recognized agent ancestor on the agent's terminal, then a foreground non-wrapper
    // ancestor (for a launcher sitting in front of an unrecognized agent), then the nearest
    // non-wrapper ancestor (skipping `sh -c` or `script`), else the nearest ancestor on it.
    if chain.first().and_then(|p| p.tdev) != Some(agent_tdev) {
        return on_tty
            .iter()
            .find(|p| p.agent)
            .or_else(|| {
                on_tty
                    .iter()
                    .find(|p| p.tpgid == Some(p.pgid) && !p.shell && !p.proxy)
            })
            .or_else(|| on_tty.iter().find(|p| !p.shell && !p.proxy))
            .map_or(first.pgid, |p| p.pgid);
    }
    // Hook shares the agent's terminal and is already in its foreground group.
    if first.tpgid == Some(first.pgid) {
        return first.pgid;
    }
    // Hook ran in a background group on the agent's terminal (`setpgid(0, 0)`): prefer a
    // recognized agent ancestor, else a non-shell, non-proxy ancestor holding the foreground.
    if let Some(agent) = on_tty.iter().skip(1).find(|p| p.agent) {
        return agent.pgid;
    }
    if let Some(fg) = on_tty
        .iter()
        .find(|p| p.tpgid == Some(p.pgid) && !p.shell && !p.proxy)
    {
        return fg.pgid;
    }
    // Backgrounded agent: if every ancestor sharing the hook's own group is a wrapper
    // (`sh -c` or `script` under `setpgid`), the agent is the next group up on the agent's
    // terminal.
    let wrapper_group = on_tty
        .iter()
        .skip(1)
        .filter(|p| p.pgid == own)
        .all(|p| p.shell || p.proxy);
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

fn matches_agent(
    pid: u32,
    comm: &str,
    shell: bool,
    proxy: bool,
    target: Option<(&str, Option<&AgentDef>)>,
) -> bool {
    if shell || proxy {
        return false;
    }
    let Some((kind, def)) = target else {
        return false;
    };
    let base = comm.rsplit('/').next().unwrap_or(comm);
    if let Some(def) = def {
        if def.matches(base, &[]) {
            return true;
        }
        let argv = proc_argv(pid).unwrap_or_default();
        return def.matches(base, &argv);
    }
    !kind.is_empty() && base == kind
}

#[cfg(target_os = "macos")]
fn proc_argv(pid: u32) -> Option<Vec<String>> {
    let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid as libc::c_int];
    let mut size: libc::size_t = 0;
    unsafe {
        if libc::sysctl(
            mib.as_mut_ptr(),
            3,
            std::ptr::null_mut(),
            &mut size,
            std::ptr::null_mut(),
            0,
        ) != 0
        {
            return None;
        }
    }
    let mut buf = vec![0u8; size];
    unsafe {
        if libc::sysctl(
            mib.as_mut_ptr(),
            3,
            buf.as_mut_ptr().cast(),
            &mut size,
            std::ptr::null_mut(),
            0,
        ) != 0
        {
            return None;
        }
    }
    buf.truncate(size);
    if buf.len() < 4 {
        return None;
    }
    let argc = i32::from_ne_bytes(buf[..4].try_into().ok()?) as usize;
    let mut rest = &buf[4..];
    let end = rest.iter().position(|&b| b == 0)?;
    rest = &rest[end..];
    let start = rest.iter().position(|&b| b != 0)?;
    rest = &rest[start..];
    Some(
        rest.split(|&b| b == 0)
            .take(argc)
            .map(|s| String::from_utf8_lossy(s).into_owned())
            .collect(),
    )
}

#[cfg(target_os = "linux")]
fn proc_argv(pid: u32) -> Option<Vec<String>> {
    let raw = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    Some(
        raw.split(|&b| b == 0)
            .filter(|s| !s.is_empty())
            .map(|s| String::from_utf8_lossy(s).into_owned())
            .collect(),
    )
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn proc_argv(_pid: u32) -> Option<Vec<String>> {
    None
}

#[cfg(target_os = "macos")]
fn proc_info(pid: u32, target: Option<(&str, Option<&AgentDef>)>) -> Option<Proc> {
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
    let shell = is_shell_name(&comm);
    let proxy = is_proxy_name(&comm);
    Some(Proc {
        ppid: info.pbi_ppid,
        pgid: info.pbi_pgid,
        tdev: has_tty.then_some(info.e_tdev),
        tpgid: has_tty.then_some(info.e_tpgid),
        shell,
        proxy,
        agent: matches_agent(pid, &comm, shell, proxy, target),
    })
}

#[cfg(target_os = "linux")]
fn proc_info(pid: u32, target: Option<(&str, Option<&AgentDef>)>) -> Option<Proc> {
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
    let shell = is_shell_name(comm);
    let proxy = is_proxy_name(comm);
    Some(Proc {
        ppid: num(1)? as u32,
        pgid: num(2)? as u32,
        tdev: has_tty.then_some(tty_nr as u32),
        tpgid: has_tty.then_some(tpgid as u32),
        shell,
        proxy,
        agent: matches_agent(pid, comm, shell, proxy, target),
    })
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn proc_info(_pid: u32, _target: Option<(&str, Option<&AgentDef>)>) -> Option<Proc> {
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
            agent: false,
        }
    }

    fn proxy(pgid: u32, tdev: Option<u32>, tpgid: Option<u32>) -> Proc {
        Proc {
            proxy: true,
            ..p(pgid, tdev, tpgid, false)
        }
    }

    fn agent(pgid: u32, tdev: Option<u32>, tpgid: Option<u32>) -> Proc {
        Proc {
            agent: true,
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
    fn detached_hook_under_child_pty_resolves_to_agent() {
        // Agent runs a hook shell in a child PTY, and that hook starts `thurm` with `setsid`.
        let chain = vec![
            p(40, None, None, false),
            p(30, Some(CHILD_TTY), Some(30), true),
            p(10, Some(PANE_TTY), Some(10), false),
            p(5, Some(PANE_TTY), Some(10), true),
        ];
        assert_eq!(pick(&chain, 40), 10);

        // Same when the shell itself runs behind a pane PTY proxy (`atuin`).
        let proxied = vec![
            p(40, None, None, false),
            p(30, Some(CHILD_TTY), Some(30), true),
            p(10, Some(PROXY_TTY), Some(10), false),
            p(5, Some(PROXY_TTY), Some(10), true),
            proxy(2, Some(PANE_TTY), Some(2)),
            p(2, Some(PANE_TTY), Some(2), true),
        ];
        assert_eq!(pick(&proxied, 40), 10);
    }

    #[test]
    fn script_launched_child_pty_hook_resolves_to_agent() {
        // Agent invokes its hook through `script` to give the hook a child PTY.
        let chain = vec![
            p(30, Some(CHILD_TTY), Some(30), false),
            p(30, Some(CHILD_TTY), Some(30), true),
            proxy(20, Some(PANE_TTY), Some(10)),
            p(10, Some(PANE_TTY), Some(10), false),
            p(5, Some(PANE_TTY), Some(10), true),
        ];
        assert_eq!(pick(&chain, 30), 10);

        // Same behind a pane PTY proxy (`atuin`), even if `script` runs in its own group while
        // the agent is backgrounded.
        let proxied_bg = vec![
            p(30, Some(CHILD_TTY), Some(30), false),
            p(30, Some(CHILD_TTY), Some(30), true),
            proxy(20, Some(PROXY_TTY), Some(5)),
            p(10, Some(PROXY_TTY), Some(5), false),
            p(5, Some(PROXY_TTY), Some(5), true),
            proxy(2, Some(PANE_TTY), Some(2)),
            p(2, Some(PANE_TTY), Some(2), true),
        ];
        assert_eq!(pick(&proxied_bg, 30), 10);
    }

    #[test]
    fn non_shell_child_pty_launcher_resolves_to_agent() {
        // Non-shell launcher (e.g. `timeout`) running the hook inside a child PTY.
        let chain = vec![
            p(30, Some(CHILD_TTY), Some(30), false),
            p(30, Some(CHILD_TTY), Some(30), false),
            p(10, Some(PANE_TTY), Some(10), false),
            p(5, Some(PANE_TTY), Some(10), true),
        ];
        assert_eq!(pick(&chain, 30), 10);

        // Same behind a pane PTY proxy (`atuin`), even if `thurm` also detached from the child PTY.
        let proxied_detached = vec![
            p(40, None, None, false),
            p(30, Some(CHILD_TTY), Some(30), false),
            p(10, Some(PROXY_TTY), Some(10), false),
            p(5, Some(PROXY_TTY), Some(10), true),
            proxy(2, Some(PANE_TTY), Some(2)),
            p(2, Some(PANE_TTY), Some(2), true),
        ];
        assert_eq!(pick(&proxied_detached, 40), 10);

        // Non-shell launcher in a background group on the agent's terminal spawning a child PTY.
        let launcher_on_agent_tty = vec![
            p(30, Some(CHILD_TTY), Some(30), false),
            p(20, Some(PROXY_TTY), Some(10), false),
            p(10, Some(PROXY_TTY), Some(10), false),
            p(5, Some(PROXY_TTY), Some(10), true),
            proxy(2, Some(PANE_TTY), Some(2)),
            p(2, Some(PANE_TTY), Some(2), true),
        ];
        assert_eq!(pick(&launcher_on_agent_tty, 30), 10);
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
    fn backgrounded_agent_beats_foreground_non_shell_parent() {
        // A foreground non-shell launcher (group 5, e.g. `timeout`) spawned an agent in
        // background group 10; detached, child-PTY, and setpgid hooks all resolve to 10.
        let detached = vec![
            p(30, None, None, false),
            agent(10, Some(PANE_TTY), Some(5)),
            p(5, Some(PANE_TTY), Some(5), false),
        ];
        assert_eq!(pick(&detached, 30), 10);

        let child_pty = vec![
            p(30, Some(CHILD_TTY), Some(30), false),
            p(30, Some(CHILD_TTY), Some(30), true),
            agent(10, Some(PANE_TTY), Some(5)),
            p(5, Some(PANE_TTY), Some(5), false),
        ];
        assert_eq!(pick(&child_pty, 30), 10);

        let setpgid = vec![
            p(30, Some(PANE_TTY), Some(5), false),
            p(30, Some(PANE_TTY), Some(5), true),
            agent(10, Some(PANE_TTY), Some(5)),
            p(5, Some(PANE_TTY), Some(5), false),
        ];
        assert_eq!(pick(&setpgid, 30), 10);
    }

    #[test]
    fn no_terminal_anywhere_falls_back_to_the_own_group() {
        assert_eq!(pick(&[p(30, None, None, false)], 30), 30);
        assert_eq!(pick(&[], 30), 30);
    }

    #[test]
    fn reads_this_process() {
        let me = proc_info(std::process::id(), None).expect("own process info");
        assert_eq!(me.pgid, unsafe { libc::getpgrp() } as u32);
    }
}
