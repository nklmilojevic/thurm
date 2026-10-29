//! Process inspection: name, argv and working directory of a pid.

use thurm_proto::ProcessInfo;

pub fn process_info(pid: u32) -> Option<ProcessInfo> {
    let argv = argv(pid).unwrap_or_default();
    let name = name(pid).or_else(|| argv.first().map(|a| basename(a).to_owned()))?;
    Some(ProcessInfo { pid, name, argv })
}

fn basename(s: &str) -> &str {
    s.rsplit('/').next().unwrap_or(s)
}

/// The executable `p` runs, resolved through symlinks (a `vim` linked to Neovim is `nvim`),
/// which names the program; `name` stays what it was started as on Linux.
#[cfg(target_os = "linux")]
pub fn executable(p: &ProcessInfo) -> String {
    std::fs::read_link(format!("/proc/{}/exe", p.pid))
        .ok()
        .and_then(|e| e.file_name().map(|n| n.to_string_lossy().into_owned()))
        .unwrap_or_else(|| p.name.clone())
}

/// The executable `p` runs: `name` already is (`proc_pidpath` resolves symlinks).
#[cfg(not(target_os = "linux"))]
pub fn executable(p: &ProcessInfo) -> String {
    p.name.clone()
}

#[cfg(target_os = "linux")]
fn name(pid: u32) -> Option<String> {
    let comm = std::fs::read_to_string(format!("/proc/{pid}/comm")).ok()?;
    Some(comm.trim_end().to_owned())
}

#[cfg(target_os = "linux")]
fn argv(pid: u32) -> Option<Vec<String>> {
    let raw = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    Some(
        raw.split(|&b| b == 0)
            .filter(|s| !s.is_empty())
            .map(|s| String::from_utf8_lossy(s).into_owned())
            .collect(),
    )
}

/// `name` in the environment `pid` was started with (not what it changed since).
#[cfg(target_os = "linux")]
pub fn env_var(pid: u32, name: &str) -> Option<String> {
    let raw = std::fs::read(format!("/proc/{pid}/environ")).ok()?;
    find_var(raw.split(|&b| b == 0), name)
}

fn find_var<'a>(entries: impl Iterator<Item = &'a [u8]>, name: &str) -> Option<String> {
    entries.into_iter().find_map(|e| {
        let rest = e.strip_prefix(name.as_bytes())?.strip_prefix(b"=")?;
        Some(String::from_utf8_lossy(rest).into_owned())
    })
}

#[cfg(target_os = "linux")]
pub fn cwd(pid: u32) -> Option<String> {
    std::fs::read_link(format!("/proc/{pid}/cwd"))
        .ok()
        .map(|p| p.display().to_string())
}

#[cfg(target_os = "macos")]
fn name(pid: u32) -> Option<String> {
    let mut buf = vec![0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
    let n = unsafe { libc::proc_pidpath(pid as i32, buf.as_mut_ptr().cast(), buf.len() as u32) };
    if n <= 0 {
        return None;
    }
    buf.truncate(n as usize);
    let path = String::from_utf8_lossy(&buf).into_owned();
    Some(basename(&path).to_owned())
}

#[cfg(target_os = "macos")]
fn argv(pid: u32) -> Option<Vec<String>> {
    let (argc, strings) = procargs(pid)?;
    Some(
        strings
            .split(|&b| b == 0)
            .take(argc)
            .map(|s| String::from_utf8_lossy(s).into_owned())
            .collect(),
    )
}

/// `name` in the environment `pid` was started with (not what it changed since).
#[cfg(target_os = "macos")]
pub fn env_var(pid: u32, name: &str) -> Option<String> {
    let (argc, strings) = procargs(pid)?;
    find_var(strings.split(|&b| b == 0).skip(argc), name)
}

/// argc, and the NUL-separated argv then environment of `pid`.
#[cfg(target_os = "macos")]
fn procargs(pid: u32) -> Option<(usize, Vec<u8>)> {
    // KERN_PROCARGS2: argc (i32), exec path, NUL padding, argv..., env...
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
    // Skip exec path.
    let end = rest.iter().position(|&b| b == 0)?;
    rest = &rest[end..];
    // Skip NUL padding.
    let start = rest.iter().position(|&b| b != 0)?;
    rest = &rest[start..];
    Some((argc, rest.to_vec()))
}

#[cfg(target_os = "macos")]
pub fn cwd(pid: u32) -> Option<String> {
    let mut info: libc::proc_vnodepathinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_vnodepathinfo>() as i32;
    let n = unsafe {
        libc::proc_pidinfo(
            pid as i32,
            libc::PROC_PIDVNODEPATHINFO,
            0,
            (&mut info as *mut libc::proc_vnodepathinfo).cast(),
            size,
        )
    };
    if n != size {
        return None;
    }
    let path = &info.pvi_cdir.vip_path;
    // libc models `char[MAXPATHLEN]` as nested arrays; flatten it.
    let bytes: Vec<u8> = path
        .iter()
        .flatten()
        .take_while(|&&c| c != 0)
        .map(|&c| c as u8)
        .collect();
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn name(_pid: u32) -> Option<String> {
    None
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn argv(_pid: u32) -> Option<Vec<String>> {
    None
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub fn cwd(_pid: u32) -> Option<String> {
    None
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub fn env_var(_pid: u32, _name: &str) -> Option<String> {
    let _ = find_var;
    None
}

/// Shells whose presence in the foreground means "nothing is running".
pub fn is_shell(name: &str) -> bool {
    let n = name.trim_start_matches('-');
    matches!(
        n,
        "zsh"
            | "bash"
            | "fish"
            | "sh"
            | "dash"
            | "ksh"
            | "tcsh"
            | "csh"
            | "nu"
            | "xonsh"
            | "elvish"
            | "login"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn own_process() {
        let me = std::process::id();
        let info = process_info(me).expect("self info");
        assert!(!info.name.is_empty());
        assert!(!info.argv.is_empty());
        assert!(cwd(me).is_some());
        let birth = process_birth(me).expect("self process birth");
        assert_eq!(process_birth(me), Some(birth));
        assert!(process_birth(u32::MAX).is_none());
        assert_eq!(parent(me), Some(unsafe { libc::getppid() } as u32));
        assert!(parent(u32::MAX).is_none());
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn children_and_job() {
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let kids = children(std::process::id());
        let _ = child.kill();
        let _ = child.wait();
        assert!(kids.contains(&child.id()), "{kids:?}");
        assert!(!kids.contains(&0), "{kids:?}");
        let me = job(std::process::id()).expect("own job");
        assert_eq!(me.pgrp, unsafe { libc::getpgrp() } as u32);
        assert!(job(u32::MAX).is_none());
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn stdin_device() {
        use std::os::unix::fs::MetadataExt;
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .stdin(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let device = stdin_tty(child.id());
        let _ = child.kill();
        let _ = child.wait();
        let null = std::fs::metadata("/dev/null").unwrap().rdev();
        assert_eq!(device, Some(null));
    }

    /// pid, ppid, name, job, stdin.
    type Row = (u32, u32, &'static str, Option<Job>, Option<u64>);

    /// A process table.
    struct Table(Vec<Row>);

    impl Procs for Table {
        fn name(&self, pid: u32) -> Option<String> {
            self.0.iter().find(|p| p.0 == pid).map(|p| p.2.to_owned())
        }
        fn job(&self, pid: u32) -> Option<Job> {
            self.0.iter().find(|p| p.0 == pid)?.3
        }
        fn children(&self, pid: u32) -> Vec<u32> {
            self.0.iter().filter(|p| p.1 == pid).map(|p| p.0).collect()
        }
        fn stdin(&self, pid: u32) -> Option<u64> {
            self.0.iter().find(|p| p.0 == pid)?.4
        }
        fn dev_tty(&self) -> Option<u64> {
            Some(DEV_TTY)
        }
    }

    /// The pane's terminal, one a proxy (or anything else) opened, and /dev/tty.
    const PANE: u64 = 6;
    const INNER: u64 = 7;
    const DEV_TTY: u64 = 2;

    fn on(pgrp: u32, tty: u64, tpgid: u32) -> Option<Job> {
        Some(Job {
            pgrp,
            tty: Some(tty),
            tpgid: Some(tpgid),
        })
    }

    #[test]
    fn atuin_behind_login_relays_its_terminal() {
        let table = |front: u32| {
            Table(vec![
                // login(1) runs as root: nothing to read but its name.
                (100, 1, "login", None, None),
                // `exec atuin pty-proxy … </dev/tty`.
                (101, 100, "atuin", on(100, PANE, 100), Some(DEV_TTY)),
                (102, 101, "fish", on(102, INNER, front), Some(INNER)),
                (103, 102, "claude", on(103, INNER, front), Some(INNER)),
            ])
        };
        assert_eq!(behind_proxy_in(100, &table(103)), 103);
        assert_eq!(behind_proxy_in(100, &table(102)), 102);
    }

    #[test]
    fn a_shell_child_on_its_own_terminal_is_not_in_front() {
        let table = Table(vec![
            (200, 1, "fish", on(200, PANE, 200), Some(PANE)),
            // Started in a session of its own, with a terminal of its own (no proxy).
            (201, 200, "server", on(201, INNER, 201), None),
        ]);
        assert_eq!(behind_proxy_in(200, &table), 200);
    }

    #[test]
    fn a_proxy_without_the_panes_input_is_not_in_front() {
        // A shell without job control, at its prompt, with `script` running in its group.
        let table = |input: Option<u64>| {
            Table(vec![
                (300, 1, "sh", on(300, PANE, 300), Some(PANE)),
                (301, 300, "script", on(300, PANE, 300), input),
                (302, 301, "claude", on(302, INNER, 302), Some(INNER)),
            ])
        };
        // In the background, its input elsewhere (a pipe, /dev/null).
        assert_eq!(behind_proxy_in(300, &table(None)), 300);
        assert_eq!(behind_proxy_in(300, &table(Some(INNER + 1))), 300);
        // Relaying the pane.
        assert_eq!(behind_proxy_in(300, &table(Some(PANE))), 302);
    }

    #[test]
    fn a_program_with_a_terminal_inside_is_in_front() {
        let table = Table(vec![
            (400, 1, "nvim", on(400, PANE, 400), Some(PANE)),
            (401, 400, "fish", on(401, INNER, 401), Some(INNER)),
        ]);
        assert_eq!(behind_proxy_in(400, &table), 400);
    }
}

/// (pid, ppid, command line).
pub type ProcRow = (u32, u32, String);

/// Every process, from `ps`.
pub fn process_table() -> Vec<ProcRow> {
    let Ok(out) = crate::pty::output_locked(
        std::process::Command::new("/bin/ps").args(["-axo", "pid=,ppid=,args="]),
    ) else {
        return Vec::new();
    };
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| {
            let l = l.trim_start();
            let (pid, rest) = l.split_once(char::is_whitespace)?;
            let rest = rest.trim_start();
            let (ppid, args) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
            Some((
                pid.parse().ok()?,
                ppid.parse().ok()?,
                args.trim().to_owned(),
            ))
        })
        .collect()
}

/// `root` and all its descendants, parents before children.
pub fn descendants(table: &[ProcRow], root: u32) -> Vec<ProcRow> {
    let mut out: Vec<ProcRow> = table.iter().filter(|p| p.0 == root).cloned().collect();
    let mut i = 0;
    while i < out.len() {
        let pid = out[i].0;
        out.extend(table.iter().filter(|p| p.1 == pid && p.0 != pid).cloned());
        i += 1;
    }
    out
}

/// Listening TCP ports per pid: the LISTEN sockets in the pid's own `/proc/<pid>/net/tcp{,6}`
/// (its network namespace's, which may not be the daemon's), matched to its open descriptors
/// by inode.
#[cfg(target_os = "linux")]
pub fn listening_ports(pids: &[u32]) -> std::collections::HashMap<u32, Vec<u16>> {
    use std::collections::HashMap;
    let mut map: HashMap<u32, Vec<u16>> = HashMap::new();
    // Tables per network namespace: most pids share one.
    let mut by_netns: HashMap<std::path::PathBuf, HashMap<u64, u16>> = HashMap::new();
    for &pid in pids {
        let netns = std::fs::read_link(format!("/proc/{pid}/ns/net")).ok();
        let cached = netns.as_ref().and_then(|ns| by_netns.get(ns));
        let own;
        let ports_by_inode = match cached {
            Some(t) => t,
            None => {
                // Only a complete read speaks for the namespace; after a failed one, the next
                // pid in it tries again.
                let tables: Option<Vec<String>> = ["tcp", "tcp6"]
                    .iter()
                    .map(|t| std::fs::read_to_string(format!("/proc/{pid}/net/{t}")).ok())
                    .collect();
                let Some(tables) = tables else { continue };
                let parsed: HashMap<u64, u16> =
                    tables.iter().flat_map(|t| listening_sockets(t)).collect();
                match netns {
                    Some(ns) => &*by_netns.entry(ns).or_insert(parsed),
                    None => {
                        own = parsed;
                        &own
                    }
                }
            }
        };
        if ports_by_inode.is_empty() {
            continue;
        }
        let Ok(fds) = std::fs::read_dir(format!("/proc/{pid}/fd")) else {
            continue;
        };
        for fd in fds.flatten() {
            let Ok(target) = std::fs::read_link(fd.path()) else {
                continue;
            };
            let inode = target
                .to_str()
                .and_then(|t| t.strip_prefix("socket:["))
                .and_then(|t| t.strip_suffix(']'))
                .and_then(|t| t.parse::<u64>().ok());
            if let Some(port) = inode.and_then(|i| ports_by_inode.get(&i)) {
                let ports = map.entry(pid).or_default();
                if !ports.contains(port) {
                    ports.push(*port);
                }
            }
        }
    }
    map
}

/// `(inode, port)` of each LISTEN socket in a `/proc/net/tcp`-format table.
#[cfg(any(target_os = "linux", test))]
fn listening_sockets(table: &str) -> Vec<(u64, u16)> {
    const LISTEN: &str = "0A";
    table
        .lines()
        .skip(1)
        .filter_map(|line| {
            let f: Vec<&str> = line.split_whitespace().collect();
            if f.len() < 10 || f[3] != LISTEN {
                return None;
            }
            let port = u16::from_str_radix(f[1].rsplit(':').next()?, 16).ok()?;
            let inode = f[9].parse::<u64>().ok()?;
            (inode != 0).then_some((inode, port))
        })
        .collect()
}

/// Listening TCP ports per pid (one `lsof` call for all of them).
#[cfg(not(target_os = "linux"))]
pub fn listening_ports(pids: &[u32]) -> std::collections::HashMap<u32, Vec<u16>> {
    let mut map: std::collections::HashMap<u32, Vec<u16>> = std::collections::HashMap::new();
    if pids.is_empty() {
        return map;
    }
    let list = pids
        .iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(",");
    let Ok(out) = crate::pty::output_locked(std::process::Command::new("/usr/sbin/lsof").args([
        "-nP",
        "-a",
        "-iTCP",
        "-sTCP:LISTEN",
        "-Fpn",
        "-p",
        &list,
    ])) else {
        return map;
    };
    let mut current = 0u32;
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        if let Some(pid) = line.strip_prefix('p') {
            current = pid.parse().unwrap_or(0);
        } else if let Some(name) = line.strip_prefix('n')
            && let Some(port) = name.rsplit(':').next().and_then(|p| p.parse::<u16>().ok())
        {
            let ports = map.entry(current).or_default();
            if !ports.contains(&port) {
                ports.push(port);
            }
        }
    }
    map
}

#[cfg(test)]
mod proc_tests {
    use super::*;

    #[test]
    fn tree_and_ports_of_a_listener() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let me = std::process::id();
        let table = process_table();
        assert!(table.iter().any(|p| p.0 == me));
        let tree = descendants(&table, me);
        assert_eq!(tree[0].0, me);
        let ports = listening_ports(&[me]);
        assert!(
            ports.get(&me).is_some_and(|p| p.contains(&port)),
            "{ports:?}"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn ports_of_a_listener_in_another_network_namespace() {
        // A new user + network namespace; skipped where that is not allowed.
        let mut child = match std::process::Command::new("unshare")
            .args(["-rn", "python3", "-c"])
            .arg(
                "import socket,sys,time\n\
                 s=socket.socket(); s.bind(('127.0.0.1',0)); s.listen()\n\
                 print(s.getsockname()[1], flush=True); time.sleep(30)",
            )
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
        {
            Ok(c) => c,
            Err(e) => {
                assert!(
                    std::env::var_os("THURM_REQUIRE_NETNS").is_none(),
                    "cannot run unshare: {e}"
                );
                return;
            }
        };
        let mut line = String::new();
        use std::io::BufRead;
        std::io::BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        let Ok(port) = line.trim().parse::<u16>() else {
            let _ = child.kill();
            // CI allows user namespaces (see ci.yml): there, not running this is a failure.
            assert!(
                std::env::var_os("THURM_REQUIRE_NETNS").is_none(),
                "cannot make a network namespace"
            );
            eprintln!("note: cannot make a network namespace here; skipped");
            return;
        };
        let pid = child.id();
        let ports = listening_ports(&[pid]);
        let _ = child.kill();
        let _ = child.wait();
        assert!(
            ports.get(&pid).is_some_and(|p| p.contains(&port)),
            "{ports:?}"
        );
    }

    #[test]
    fn listen_sockets_parsed_from_proc_net_tcp() {
        let table = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode
   0: 0100007F:1F90 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 41234 1 0000000000000000 100 0 0 10 0
   1: 0100007F:A1B2 0100007F:1F90 01 00000000:00000000 00:00000000 00000000  1000        0 41235 1 0000000000000000 20 4 30 10 -1
   2: 00000000:0016 00000000:0000 0A 00000000:00000000 00:00000000 00000000     0        0 0 1 0000000000000000 100 0 0 10 0
";
        assert_eq!(listening_sockets(table), vec![(41234, 8080)]);
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn environment_a_process_started_with() {
        // Our own process: other processes' environments may be hidden by the system.
        let pid = std::process::id();
        let home = std::env::var("HOME").unwrap();
        assert_eq!(env_var(pid, "HOME"), Some(home));
        assert_eq!(env_var(pid, "THURM_SURELY_UNSET_VARIABLE"), None);
    }

    #[test]
    fn finds_variables_by_whole_name() {
        let env: [&[u8]; 3] = [b"PATHS=x", b"PATH=/usr/bin:/bin", b"EMPTY="];
        assert_eq!(
            find_var(env.into_iter(), "PATH").as_deref(),
            Some("/usr/bin:/bin")
        );
        assert_eq!(find_var(env.into_iter(), "EMPTY").as_deref(), Some(""));
        assert_eq!(find_var(env.into_iter(), "PAT"), None);
    }
}

/// The pid of `pid`'s parent.
#[cfg(target_os = "linux")]
pub fn parent(pid: u32) -> Option<u32> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // The first field after the name is field 3; ppid is field 4.
    stat.rsplit_once(')')?.1.split_whitespace().nth(1)?.parse().ok()
}

#[cfg(target_os = "macos")]
pub fn parent(pid: u32) -> Option<u32> {
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
    (n == size).then_some(info.pbi_ppid)
}

/// Process birth, used to distinguish a reused PID from its previous process.
#[cfg(target_os = "linux")]
pub fn process_birth(pid: u32) -> Option<u64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // The first field after the name is field 3; starttime is field 22.
    stat.rsplit_once(')')?
        .1
        .split_whitespace()
        .nth(19)?
        .parse()
        .ok()
}

#[cfg(target_os = "macos")]
pub fn process_birth(pid: u32) -> Option<u64> {
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
    (n == size).then(|| {
        info.pbi_start_tvsec
            .saturating_mul(1_000_000)
            .saturating_add(info.pbi_start_tvusec)
    })
}

/// Programs that run the shell on a terminal of their own and relay it to the pane's
/// (`atuin pty-proxy`, `script`).
const PTY_PROXIES: &[&str] = &["atuin", "script"];

/// The foreground group of the pane's terminal (`pgrp`), seen through PTY proxies. While a
/// proxy runs, its shell and everything that shell starts are on the proxy's terminal, and
/// the pane's shows the proxy's group in front (login(1) and `atuin pty-proxy` under it). So
/// follow the shells and proxies of that group to a proxy that reads the pane's terminal, and
/// take the foreground group of the terminal it runs its child on (again, for a proxy behind
/// a proxy). Anything else in front (Neovim with a `:terminal` open) is what runs.
pub fn behind_proxy(pgrp: u32) -> u32 {
    behind_proxy_in(pgrp, &System)
}

fn behind_proxy_in(pgrp: u32, procs: &impl Procs) -> u32 {
    let mut fg = pgrp;
    for _ in 0..4 {
        match inner_foreground(fg, procs) {
            Some(inner) if inner != fg => fg = inner,
            _ => break,
        }
    }
    fg
}

fn inner_foreground(pgrp: u32, procs: &impl Procs) -> Option<u32> {
    // The pane's terminal. login(1) runs as root and tells us nothing: the next process of its
    // group does.
    let mut tty = procs.job(pgrp).and_then(|j| j.tty);
    let mut queue = vec![pgrp];
    let mut visited = 0;
    while let Some(pid) = queue.pop() {
        visited += 1;
        let Some(name) = procs.name(pid).filter(|_| visited <= 16) else {
            continue;
        };
        let proxy = PTY_PROXIES.contains(&name.as_str());
        if !proxy && !is_shell(&name) {
            continue;
        }
        // The group's own processes first: the shell's jobs are not what it relays, and a
        // process of the group tells the terminal when the leader didn't.
        let (own, others): (Vec<_>, Vec<_>) = procs
            .children(pid)
            .into_iter()
            .filter_map(|c| Some((c, procs.job(c)?)))
            .partition(|(_, j)| j.pgrp == pgrp);
        for (child, j) in own {
            if let Some(t) = j.tty
                && *tty.get_or_insert(t) == t
            {
                queue.push(child);
            }
        }
        // Only a proxy relays another terminal, and only one that reads the pane's: a shell's
        // own children on other terminals, or a proxy left running in the background without
        // its input, are not what is in front. Its input may be /dev/tty, its controlling
        // terminal, which is the pane's (`atuin init` runs it so when its input isn't one).
        let input = procs.stdin(pid);
        if !proxy || tty.is_none() || (input != tty && input != procs.dev_tty()) {
            continue;
        }
        for (_, j) in others {
            if j.tty.is_some()
                && j.tty != tty
                && let Some(inner) = j.tpgid
            {
                return Some(inner);
            }
        }
    }
    None
}

/// What [`behind_proxy`] asks about processes.
trait Procs {
    fn name(&self, pid: u32) -> Option<String>;
    fn job(&self, pid: u32) -> Option<Job>;
    fn children(&self, pid: u32) -> Vec<u32>;
    /// The device `pid`'s standard input is (a terminal's, `/dev/null`'s), if it is one.
    fn stdin(&self, pid: u32) -> Option<u64>;
    /// The device of `/dev/tty`.
    fn dev_tty(&self) -> Option<u64>;
}

/// This machine's processes.
struct System;

impl Procs for System {
    fn name(&self, pid: u32) -> Option<String> {
        name(pid)
    }
    fn job(&self, pid: u32) -> Option<Job> {
        job(pid)
    }
    fn children(&self, pid: u32) -> Vec<u32> {
        children(pid)
    }
    fn stdin(&self, pid: u32) -> Option<u64> {
        stdin_tty(pid)
    }
    fn dev_tty(&self) -> Option<u64> {
        use std::os::unix::fs::MetadataExt;
        static DEV_TTY: std::sync::OnceLock<Option<u64>> = std::sync::OnceLock::new();
        *DEV_TTY.get_or_init(|| std::fs::metadata("/dev/tty").ok().map(|m| m.rdev()))
    }
}

/// A process's group, its controlling terminal and that terminal's foreground group.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Job {
    pgrp: u32,
    tty: Option<u64>,
    tpgid: Option<u32>,
}

#[cfg(target_os = "linux")]
fn job(pid: u32) -> Option<Job> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // After the name: state ppid pgrp session tty_nr tpgid.
    let f: Vec<&str> = stat
        .rsplit_once(')')?
        .1
        .split_whitespace()
        .take(6)
        .collect();
    let num = |i: usize| f.get(i)?.parse::<i64>().ok();
    Some(Job {
        pgrp: num(2)? as u32,
        tty: num(4).filter(|&t| t != 0).map(|t| t as u64),
        tpgid: num(5).filter(|&t| t > 0).map(|t| t as u32),
    })
}

#[cfg(target_os = "linux")]
fn children(pid: u32) -> Vec<u32> {
    // Each thread lists the children it started.
    let Ok(tasks) = std::fs::read_dir(format!("/proc/{pid}/task")) else {
        return Vec::new();
    };
    tasks
        .flatten()
        .filter_map(|t| std::fs::read_to_string(t.path().join("children")).ok())
        .flat_map(|s| {
            s.split_whitespace()
                .filter_map(|p| p.parse().ok())
                .collect::<Vec<u32>>()
        })
        .collect()
}

#[cfg(target_os = "macos")]
fn job(pid: u32) -> Option<Job> {
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
    (n == size).then_some(Job {
        pgrp: info.pbi_pgid,
        tty: (info.e_tdev != u32::MAX).then_some(info.e_tdev as u64),
        tpgid: (info.e_tpgid != 0).then_some(info.e_tpgid),
    })
}

#[cfg(target_os = "macos")]
fn children(pid: u32) -> Vec<u32> {
    let mut pids = [0 as libc::pid_t; 64];
    let n = unsafe {
        libc::proc_listchildpids(
            pid as i32,
            pids.as_mut_ptr().cast(),
            std::mem::size_of_val(&pids) as i32,
        )
    };
    pids[..n.clamp(0, pids.len() as i32) as usize]
        .iter()
        .map(|&p| p as u32)
        .collect()
}

#[cfg(target_os = "linux")]
fn stdin_tty(pid: u32) -> Option<u64> {
    use std::os::unix::fs::{FileTypeExt, MetadataExt};
    let m = std::fs::metadata(format!("/proc/{pid}/fd/0")).ok()?;
    m.file_type().is_char_device().then_some(m.rdev())
}

/// `struct vnode_fdinfowithpath` (`proc_fileinfo`, then the vnode), which libc lacks.
#[cfg(target_os = "macos")]
#[repr(C)]
struct VnodeFdInfo {
    openflags: u32,
    status: u32,
    offset: libc::off_t,
    kind: i32,
    guardflags: u32,
    vnode: libc::vnode_info_path,
}

#[cfg(target_os = "macos")]
const PROC_PIDFDVNODEPATHINFO: libc::c_int = 2;

#[cfg(target_os = "macos")]
fn stdin_tty(pid: u32) -> Option<u64> {
    let mut info: VnodeFdInfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of_val(&info) as i32;
    let n = unsafe {
        libc::proc_pidfdinfo(
            pid as i32,
            0,
            PROC_PIDFDVNODEPATHINFO,
            (&mut info as *mut VnodeFdInfo).cast(),
            size,
        )
    };
    let stat = &info.vnode.vip_vi.vi_stat;
    (n == size && stat.vst_mode & libc::S_IFMT == libc::S_IFCHR).then_some(stat.vst_rdev as u64)
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn job(_pid: u32) -> Option<Job> {
    None
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn stdin_tty(_pid: u32) -> Option<u64> {
    None
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn children(_pid: u32) -> Vec<u32> {
    Vec::new()
}

/// Read the agent's identity and confirm that it belongs to the pane's foreground.
pub fn report_owner(pid: u32, foreground: Option<u32>) -> Result<(u64, u32), String> {
    if pid == 0 || pid > i32::MAX as u32 {
        return Err("invalid owner PID".into());
    }
    let birth = process_birth(pid).ok_or("cannot read the report owner's process")?;
    let pgrp = unsafe { libc::getpgid(pid as i32) };
    if pgrp <= 0 || Some(pgrp as u32) != foreground {
        return Err("report owner is not in the pane's foreground process group".into());
    }
    if process_info(pid).is_none_or(|p| is_shell(&p.name)) {
        return Err("report owner must be an agent process, not a shell".into());
    }
    Ok((birth, pgrp as u32))
}
