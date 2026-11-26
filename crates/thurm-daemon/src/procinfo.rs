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
    let comm = std::fs::read(format!("/proc/{pid}/comm")).ok()?;
    Some(String::from_utf8_lossy(&comm).trim_end().to_owned())
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
    is_shell_program(name) || name.trim_start_matches('-') == "login"
}

/// Shell programs by executable name (`login` is not one): the one list both foreground
/// detection and program resume use.
pub fn is_shell_program(name: &str) -> bool {
    matches!(
        name.trim_start_matches('-'),
        "zsh"
            | "bash"
            | "fish"
            | "sh"
            | "ash"
            | "dash"
            | "ksh"
            | "mksh"
            | "oksh"
            | "pdksh"
            | "yash"
            | "tcsh"
            | "csh"
            | "nu"
            | "xonsh"
            | "elvish"
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
        use std::os::fd::{FromRawFd, OwnedFd};
        use std::os::unix::fs::MetadataExt;
        let device = |stdin: std::process::Stdio| {
            let mut child = std::process::Command::new("sleep")
                .arg("30")
                .stdin(stdin)
                .spawn()
                .unwrap();
            let device = stdin_tty(child.id());
            let _ = child.kill();
            let _ = child.wait();
            device
        };
        let null = std::fs::metadata("/dev/null").unwrap().rdev();
        assert_eq!(device(std::process::Stdio::null()), Some(null));
        // A terminal's, as its own `stat` gives it.
        let (mut master, mut slave) = (0, 0);
        let opened = unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        assert_eq!(opened, 0);
        let (master, slave) = unsafe {
            (
                OwnedFd::from_raw_fd(master),
                std::fs::File::from_raw_fd(slave),
            )
        };
        let terminal = slave.metadata().unwrap().rdev();
        assert_eq!(device(slave.into()), Some(terminal));
        drop(master);
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

/// Listening TCP ports per pid (the Processes panel's view): what [`listeners`] finds, so the
/// panel and the forwards never disagree about a pane; none when the look failed.
pub fn listening_ports(pids: &[u32]) -> std::collections::HashMap<u32, Vec<u16>> {
    let Some(found) = listeners(pids) else {
        return Default::default();
    };
    found
        .by_pid
        .into_iter()
        .map(|(pid, ls)| {
            let mut ports: Vec<u16> = Vec::new();
            for l in ls {
                if !ports.contains(&l.port) {
                    ports.push(l.port);
                }
            }
            (pid, ports)
        })
        .collect()
}

/// A listening TCP socket: its port and the address it is bound to.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Listener {
    pub port: u16,
    /// `127.0.0.1`, `::1`, `0.0.0.0`, `::`, or another address, spelled alike on Linux and
    /// macOS.
    pub address: String,
}

/// What [`listeners`] found.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Listening {
    pub by_pid: std::collections::HashMap<u32, Vec<Listener>>,
    /// Pids whose sockets could not be read (another user's, a namespace the daemon can't see):
    /// what they listen on is unknown.
    pub unread: std::collections::HashSet<u32>,
}

/// The TCP sockets each of `pids` listens on, with their addresses; `None` when the look
/// failed as a whole (`lsof` could not run).
#[cfg(target_os = "linux")]
pub fn listeners(pids: &[u32]) -> Option<Listening> {
    Some(listeners_in(pids, &RealProc))
}

/// What [`listeners_in`] reads of a process, from `/proc` ([`RealProc`]) or, in tests, made up.
#[cfg(any(target_os = "linux", test))]
trait ProcFs {
    /// `/proc/<pid>/ns/net`: the network namespace, to read its tables once.
    fn netns(&self, pid: u32) -> Option<std::path::PathBuf>;
    /// `/proc/<pid>/net/<name>` (`tcp`, `tcp6`).
    fn table(&self, pid: u32, name: &str) -> std::io::Result<String>;
    /// The targets of `/proc/<pid>/fd/*`, each read on its own.
    fn fd_links(&self, pid: u32) -> std::io::Result<Vec<std::io::Result<std::path::PathBuf>>>;
}

#[cfg(target_os = "linux")]
struct RealProc;

#[cfg(target_os = "linux")]
impl ProcFs for RealProc {
    fn netns(&self, pid: u32) -> Option<std::path::PathBuf> {
        std::fs::read_link(format!("/proc/{pid}/ns/net")).ok()
    }

    fn table(&self, pid: u32, name: &str) -> std::io::Result<String> {
        std::fs::read_to_string(format!("/proc/{pid}/net/{name}"))
    }

    fn fd_links(&self, pid: u32) -> std::io::Result<Vec<std::io::Result<std::path::PathBuf>>> {
        Ok(std::fs::read_dir(format!("/proc/{pid}/fd"))?
            .map(|fd| fd.and_then(|fd| std::fs::read_link(fd.path())))
            .collect())
    }
}

/// Whether a failed read means the process, or the descriptor, has gone: `ENOENT` (a process
/// that exited; a zombie not reaped yet still has `/proc/<pid>`, but no network tables and no
/// descriptors; or a descriptor closed between listing and reading) or `ESRCH`. Its ports are
/// then none. Any other failure (refused, out of descriptors, an I/O error) leaves what it
/// listens on unknown: the process is unread, and its pane's list incomplete, so a failed look
/// never reads as a port gone.
#[cfg(any(target_os = "linux", test))]
fn gone(e: &std::io::Error) -> bool {
    e.kind() == std::io::ErrorKind::NotFound || e.raw_os_error() == Some(libc::ESRCH)
}

#[cfg(any(target_os = "linux", test))]
fn listeners_in(pids: &[u32], fs: &impl ProcFs) -> Listening {
    use std::collections::HashMap;
    let mut out = Listening::default();
    let mut by_netns: HashMap<std::path::PathBuf, HashMap<u64, Listener>> = HashMap::new();
    for &pid in pids {
        let netns = fs.netns(pid);
        let cached = netns.as_ref().and_then(|ns| by_netns.get(ns));
        let own;
        let by_inode = match cached {
            Some(t) => t,
            None => {
                // `tcp` says whether the process is there. A missing `tcp6` beside it is a
                // kernel without IPv6: no sockets there, not a process gone.
                let tables = match fs
                    .table(pid, "tcp")
                    .and_then(|v4| match fs.table(pid, "tcp6") {
                        Ok(v6) => Ok(vec![v4, v6]),
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(vec![v4]),
                        Err(e) => Err(e),
                    }) {
                    Ok(t) => t,
                    Err(e) => {
                        if !gone(&e) {
                            out.unread.insert(pid);
                        }
                        continue;
                    }
                };
                let parsed: HashMap<u64, Listener> = tables
                    .iter()
                    .flat_map(|t| listening_sockets_with_address(t))
                    .collect();
                match netns {
                    Some(ns) => &*by_netns.entry(ns).or_insert(parsed),
                    None => {
                        own = parsed;
                        &own
                    }
                }
            }
        };
        // No listening socket in the process's namespace: it listens on none, whatever its
        // descriptors are.
        if by_inode.is_empty() {
            continue;
        }
        let links = match fs.fd_links(pid) {
            Ok(l) => l,
            Err(e) => {
                if !gone(&e) {
                    out.unread.insert(pid);
                }
                continue;
            }
        };
        let (inodes, partial) = socket_inodes(links.into_iter());
        if partial {
            out.unread.insert(pid);
        }
        for l in inodes.iter().filter_map(|i| by_inode.get(i)) {
            let list = out.by_pid.entry(pid).or_default();
            if !list.contains(l) {
                list.push(l.clone());
            }
        }
    }
    out
}

/// The socket inodes among a process's fd links (`/proc/<pid>/fd/*`), and whether one could not
/// be read: then what it listens on is only partly known. A link that has gone (its descriptor
/// closed between listing and reading, or the process exited) is not a partial read ([`gone`]).
#[cfg(any(target_os = "linux", test))]
fn socket_inodes(
    links: impl Iterator<Item = std::io::Result<std::path::PathBuf>>,
) -> (Vec<u64>, bool) {
    let mut inodes = Vec::new();
    let mut partial = false;
    for link in links {
        match link {
            Ok(target) => inodes.extend(
                target
                    .to_str()
                    .and_then(|t| t.strip_prefix("socket:["))
                    .and_then(|t| t.strip_suffix(']'))
                    .and_then(|t| t.parse::<u64>().ok()),
            ),
            Err(e) if gone(&e) => {}
            Err(_) => partial = true,
        }
    }
    (inodes, partial)
}

/// `(inode, listener)` of each LISTEN socket in a `/proc/net/tcp` or `tcp6` table.
#[cfg(any(target_os = "linux", test))]
fn listening_sockets_with_address(table: &str) -> Vec<(u64, Listener)> {
    const LISTEN: &str = "0A";
    table
        .lines()
        .skip(1)
        .filter_map(|line| {
            let f: Vec<&str> = line.split_whitespace().collect();
            if f.len() < 10 || f[3] != LISTEN {
                return None;
            }
            let (addr, port) = f[1].rsplit_once(':')?;
            let port = u16::from_str_radix(port, 16).ok()?;
            let inode = f[9].parse::<u64>().ok()?;
            let address = proc_net_address(addr)?;
            (inode != 0).then_some((inode, Listener { port, address }))
        })
        .collect()
}

/// An address as `/proc/net/tcp{,6}` writes it: 32-bit words in host byte order (little-endian
/// on every Linux Thurm runs on), 8 hex digits for IPv4 and 32 for IPv6.
#[cfg(any(target_os = "linux", test))]
fn proc_net_address(hex: &str) -> Option<String> {
    let word = |i: usize| -> Option<[u8; 4]> {
        Some(
            u32::from_str_radix(hex.get(i * 8..i * 8 + 8)?, 16)
                .ok()?
                .to_le_bytes(),
        )
    };
    match hex.len() {
        8 => Some(std::net::Ipv4Addr::from(word(0)?).to_string()),
        32 => {
            let mut b = [0u8; 16];
            for i in 0..4 {
                b[i * 4..i * 4 + 4].copy_from_slice(&word(i)?);
            }
            let v6 = std::net::Ipv6Addr::from(b);
            Some(match v6.to_ipv4_mapped() {
                Some(v4) => v4.to_string(),
                None => v6.to_string(),
            })
        }
        _ => None,
    }
}

/// The TCP sockets each of `pids` listens on, with their addresses (one `lsof` call); `None`
/// when `lsof` could not run.
#[cfg(not(target_os = "linux"))]
pub fn listeners(pids: &[u32]) -> Option<Listening> {
    let mut out = Listening::default();
    if pids.is_empty() {
        return Some(out);
    }
    let list = pids
        .iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(",");
    // Exit status 1 is also "nothing listens": only a failure to run counts.
    let res = crate::pty::output_locked(std::process::Command::new("/usr/sbin/lsof").args([
        "-nP",
        "-a",
        "-iTCP",
        "-sTCP:LISTEN",
        "-Fptn",
        "-p",
        &list,
    ]))
    .ok()?;
    out.by_pid = parse_lsof_listeners(&String::from_utf8_lossy(&res.stdout));
    // `lsof` leaves out a process it may not inspect (another user's) without a word, and exits
    // as it does when nothing listens: ask the system which of them it could not have read.
    // Only those it said nothing of: one it listed was read.
    out.unread = pids
        .iter()
        .copied()
        .filter(|pid| !out.by_pid.contains_key(pid))
        .filter(|&pid| fds_access(pid) == FdAccess::Denied)
        .collect();
    Some(out)
}

/// Whether this process may list `pid`'s descriptors.
#[cfg(not(target_os = "linux"))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FdAccess {
    Readable,
    /// Exited: nothing of it to read.
    Gone,
    Denied,
}

#[cfg(target_os = "macos")]
pub fn fds_access(pid: u32) -> FdAccess {
    // With no buffer, the size the descriptor list needs: what fails `lsof` fails here too.
    let n = unsafe {
        *libc::__error() = 0;
        libc::proc_pidinfo(
            pid as i32,
            libc::PROC_PIDLISTFDS,
            0,
            std::ptr::null_mut(),
            0,
        )
    };
    if n > 0 {
        return FdAccess::Readable;
    }
    match std::io::Error::last_os_error().raw_os_error() {
        Some(libc::ESRCH) => FdAccess::Gone,
        // A process with no descriptors at all answers 0 without an error.
        Some(0) | None => FdAccess::Readable,
        _ => FdAccess::Denied,
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub fn fds_access(_pid: u32) -> FdAccess {
    FdAccess::Readable
}

/// `lsof -Fpn` output: `p<pid>` lines, each followed by `n<address>:<port>` lines.
#[cfg(any(not(target_os = "linux"), test))]
fn parse_lsof_listeners(text: &str) -> std::collections::HashMap<u32, Vec<Listener>> {
    let mut map: std::collections::HashMap<u32, Vec<Listener>> = Default::default();
    let mut current = 0u32;
    let mut ipv6 = false;
    for line in text.lines() {
        if let Some(pid) = line.strip_prefix('p') {
            current = pid.parse().unwrap_or(0);
        } else if line.starts_with('f') {
            ipv6 = false;
        } else if let Some(kind) = line.strip_prefix('t') {
            ipv6 = kind == "IPv6";
        } else if let Some(name) = line.strip_prefix('n')
            && let Some((addr, port)) = name.rsplit_once(':')
            && let Ok(port) = port.parse::<u16>()
        {
            let addr = addr.trim_start_matches('[').trim_end_matches(']');
            // Spelled as the Linux tables have it: `*` is the family's any-address, and an
            // IPv4 address mapped into IPv6 is the IPv4 one.
            let address = match addr {
                "*" if ipv6 => "::".to_owned(),
                "*" => "0.0.0.0".to_owned(),
                a => a.strip_prefix("::ffff:").unwrap_or(a).to_owned(),
            };
            let l = Listener { port, address };
            let list = map.entry(current).or_default();
            if !list.contains(&l) {
                list.push(l);
            }
        }
    }
    map
}

#[cfg(test)]
mod proc_tests {
    use super::*;

    #[test]
    fn shell_programs_are_one_list() {
        for name in ["mksh", "oksh", "pdksh", "yash", "ash", "-zsh", "fish"] {
            assert!(is_shell_program(name), "{name}");
            assert!(is_shell(name), "{name}");
        }
        // login is a shell for foreground detection only.
        assert!(is_shell("login"));
        assert!(!is_shell_program("login"));
        assert!(!is_shell_program("python3"));
    }

    #[test]
    fn zombie_and_dead_states_are_not_live() {
        assert!(stat_state_is_live(Some("S")));
        assert!(stat_state_is_live(Some("R")));
        assert!(!stat_state_is_live(Some("Z")));
        assert!(!stat_state_is_live(Some("X")));
        assert!(!stat_state_is_live(None));
    }

    #[test]
    fn procfs_records_parse_whatever_the_process_name() {
        // A name with non-UTF-8 bytes, spaces and a `)` of its own.
        let stat = b"4242 (we\xffird) n) S 1 4242 4242 34816 4242 4194560 0 0 0 0 0 0 0 0 20 0 1 0 98765 0";
        let f = stat_fields(stat).unwrap();
        assert_eq!(f[0], "S");
        assert_eq!(f[1], "1");
        assert_eq!(f[19], "98765");
        assert_eq!(stat_fields(b"no name"), None);
        let status = b"Name:\tst\xfe\xffrange\nUmask:\t0022\nState:\tS (sleeping)\nUid:\t501\t502\t502\t502\nGid:\t20\n";
        assert_eq!(status_uids(status), Some((501, 502)));
        assert_eq!(status_uids(b"Name:\tx\n"), None);
    }

    /// A program whose executable name is not UTF-8 is still alive, born and owned.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_non_utf8_process_name_is_still_inspected() {
        use std::os::unix::ffi::OsStrExt;
        let dir = PrivateDir::new();
        let exe = dir.path().join(std::ffi::OsStr::from_bytes(b"sl\xffeep"));
        let Ok(sleep) = std::fs::canonicalize("/bin/sleep") else {
            return;
        };
        std::fs::copy(sleep, &exe).unwrap();
        // Another test forking while the copy was open can leave it busy for a moment.
        let mut tries = 0;
        let mut child = loop {
            match std::process::Command::new(&exe).arg("30").spawn() {
                Err(e) if e.raw_os_error() == Some(libc::ETXTBSY) && tries < 50 => {
                    tries += 1;
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
                spawned => break spawned.unwrap(),
            }
        };
        let pid = child.id();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        // Until the exec, the child still has the test's name.
        while std::fs::read(format!("/proc/{pid}/comm")).is_ok_and(|c| !c.contains(&0xff)) {
            assert!(std::time::Instant::now() < deadline, "child never ran");
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(live_process_birth(pid).is_some());
        assert!(process_birth(pid).is_some());
        assert_eq!(parent(pid), Some(std::process::id()));
        let me = unsafe { (libc::getuid(), libc::geteuid()) };
        assert_eq!(process_uids(pid), Some(me));
        assert!(job(pid).is_some());
        child.kill().unwrap();
        child.wait().unwrap();
    }

    /// A program whose main thread ended while a worker runs on is alive.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_leader_zombie_with_a_live_worker_is_alive() {
        extern "C" fn worker(_: *mut libc::c_void) -> *mut libc::c_void {
            loop {
                unsafe { libc::pause() };
            }
        }
        let pid = unsafe { libc::fork() };
        assert!(pid >= 0);
        if pid == 0 {
            // The child's only thread is its leader: start a worker, then end the leader
            // alone (what `pthread_exit` comes to, without unwinding through Rust frames).
            unsafe {
                let mut t: libc::pthread_t = std::mem::zeroed();
                if libc::pthread_create(&mut t, std::ptr::null(), worker, std::ptr::null_mut()) != 0
                {
                    libc::_exit(1);
                }
                libc::syscall(libc::SYS_exit, 0);
                libc::_exit(1);
            }
        }
        let pid = pid as u32;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while proc_stat(format!("/proc/{pid}/stat"))
            .is_some_and(|f| f.first().map(String::as_str) != Some("Z"))
        {
            assert!(std::time::Instant::now() < deadline, "leader never ended");
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let alive = live_process_birth(pid).is_some();
        unsafe { libc::kill(pid as i32, libc::SIGKILL) };
        // With every thread gone, the zombie left is not alive.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut exited = false;
        while std::time::Instant::now() < deadline {
            if live_process_birth(pid).is_none() {
                exited = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        unsafe { libc::waitpid(pid as i32, std::ptr::null_mut(), 0) };
        assert!(alive, "a running worker keeps the process alive");
        assert!(exited, "an exited thread group is not alive");
    }

    #[test]
    fn live_birth_of_self_and_of_a_zombie() {
        assert!(live_process_birth(std::process::id()).is_some());
        // An exited child not reaped yet keeps its birth but is not live.
        let mut child = std::process::Command::new("/bin/sh")
            .args(["-c", "exit 0"])
            .spawn()
            .unwrap();
        let pid = child.id();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while live_process_birth(pid).is_some() {
            assert!(std::time::Instant::now() < deadline, "child never exited");
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        // Linux still shows a zombie's start time (macOS answers nothing for one).
        if cfg!(target_os = "linux") {
            assert!(process_birth(pid).is_some(), "zombie keeps its birth");
        }
        child.wait().unwrap();
    }

    #[test]
    fn ancestry_walks_past_64_hops_and_stops_on_cycles() {
        // 1000 <- 999 <- ... <- 900 (pane) ; reporter 1000 is 100 hops below.
        let parent_of = |p: u32| (p > 2).then(|| p - 1);
        let (chain, reached) = ancestors_with(1000, 900, parent_of);
        assert!(reached);
        assert_eq!(chain.len(), 100);
        assert_eq!(chain.last(), Some(&900));
        // A cycle that never reaches the pane is not descent.
        let cyclic = |p: u32| Some(if p == 10 { 11 } else { 10 });
        assert!(!ancestors_with(10, 5, cyclic).1);
        // Reaching pid 1 stops the walk, even where a parent lookup would go on to the pane.
        let parent_through_init = |p: u32| Some(if p == 1 { 900 } else { p - 1 });
        let (chain, reached) = ancestors_with(50, 900, parent_through_init);
        assert_eq!(chain.last(), Some(&1));
        assert!(!reached);
    }

    #[test]
    fn only_a_group_of_the_user_alone_is_private() {
        let me = b"alice".to_vec();
        let only_me = std::slice::from_ref(&me);
        assert!(is_private_group(&me, &me, &[], only_me));
        assert!(is_private_group(&me, &me, only_me, only_me));
        // Another member, another name (`staff`, `users`), or no user name: shared.
        let me_and_eve = [me.clone(), b"eve".to_vec()];
        assert!(!is_private_group(&me, &me, &me_and_eve, only_me));
        assert!(!is_private_group(&me, b"staff", &[], only_me));
        assert!(!is_private_group(b"", b"", &[], &[]));
        // Another account with it as its primary group is in it, though `gr_mem` doesn't say.
        assert!(!is_private_group(&me, &me, &[], &me_and_eve));
        // A listing of accounts that doesn't show the user is incomplete: not private.
        assert!(!is_private_group(&me, &me, &[], &[]));
        // macOS gives every account `staff` as its primary group.
        if cfg!(target_os = "macos") {
            assert_eq!(private_group(unsafe { libc::geteuid() }, 20), None);
        }
    }

    #[test]
    fn trusted_install_requires_a_chain_nobody_else_can_change() {
        use std::path::Path;
        const ME: u32 = 501;
        const MY_GID: u32 = 20;
        let entry = |uid, gid, mode, is_dir| Ownership {
            uid,
            gid,
            mode,
            is_dir,
            is_file: !is_dir,
        };
        let root_dir = entry(0, 0, 0o40755, true);
        let root_exe = entry(0, 0, 0o100755, false);
        // `exe` is `file`, its parent `dir`, every directory above root's.
        let check = |exe: &str, dir: Ownership, file: Ownership| {
            let exe = Path::new(exe);
            let parent = exe.parent().unwrap().to_path_buf();
            is_trusted_install_with(exe, ME, Some(MY_GID), |p| {
                Some(if p == exe {
                    file
                } else if p == parent {
                    dir
                } else {
                    root_dir
                })
            })
        };
        // Root's all the way: trusted anywhere.
        assert!(check("/bin/zsh", root_dir, root_exe));
        assert!(check("/nix/store/abc-bash/bin/bash", root_dir, root_exe));
        // The user's own file outside a package prefix: not trusted.
        let mine_dir = entry(ME, MY_GID, 0o40755, true);
        let mine_exe = entry(ME, MY_GID, 0o100755, false);
        assert!(!check("/home/me/bin/zsh", mine_dir, mine_exe));
        // A Homebrew tree the user administers (group-writable by `admin` on macOS).
        let admin = if cfg!(target_os = "macos") {
            80
        } else {
            MY_GID
        };
        let brew_dir = entry(ME, admin, 0o40775, true);
        assert!(check("/opt/homebrew/bin/fish", brew_dir, mine_exe));
        // Nix's store: root's, group-writable by its build users, sticky.
        let store = entry(0, 30000, 0o41775, true);
        assert!(check("/nix/store/fish", store, root_exe));
        // Root's but writable by another group: not without the sticky bit, never a file.
        let unsticky = entry(0, 30000, 0o40775, true);
        assert!(!check("/nix/store/fish", unsticky, root_exe));
        assert!(!check("/usr/local/share/fish", store, root_exe));
        let group_exe = entry(0, 100, 0o100775, false);
        assert!(!check("/nix/store/fish", root_dir, group_exe));
        assert!(!check("/opt/homebrew/bin/fish", root_dir, group_exe));
        // The user's primary group when it is shared (macOS's `staff`): not trusted to write.
        let staff_exe = entry(ME, MY_GID, 0o100775, false);
        let staff_dir = entry(ME, MY_GID, 0o40775, true);
        let shared_primary = |exe: &str, dir: Ownership, file: Ownership| {
            let exe = Path::new(exe);
            let parent = exe.parent().unwrap().to_path_buf();
            is_trusted_install_with(exe, ME, None, |p| {
                Some(if p == exe {
                    file
                } else if p == parent {
                    dir
                } else {
                    root_dir
                })
            })
        };
        assert!(check("/usr/local/bin/fish", mine_dir, staff_exe));
        assert!(!shared_primary("/usr/local/bin/fish", mine_dir, staff_exe));
        assert!(!shared_primary("/usr/local/bin/fish", staff_dir, mine_exe));
        assert!(shared_primary("/usr/local/bin/fish", mine_dir, mine_exe));
        // Under a package prefix, another account's tree or file is not trusted...
        let theirs_dir = entry(1001, 1001, 0o40755, true);
        let theirs_exe = entry(1001, 1001, 0o100755, false);
        assert!(!check(
            "/home/linuxbrew/.linuxbrew/bin/bash",
            theirs_dir,
            root_exe
        ));
        assert!(!check("/opt/homebrew/bin/atuin", mine_dir, theirs_exe));
        // ...nor one anyone can write, nor one a group of others can.
        let open_dir = entry(ME, MY_GID, 0o40777, true);
        assert!(!check("/opt/homebrew/bin/zsh", open_dir, mine_exe));
        let shared_dir = entry(ME, 100, 0o40775, true);
        assert!(!check("/usr/local/bin/zsh", shared_dir, mine_exe));
        let shared_exe = entry(ME, 100, 0o100775, false);
        assert!(!check("/usr/local/bin/zsh", mine_dir, shared_exe));
        // A higher directory counts too.
        let exe = Path::new("/opt/homebrew/Cellar/fish/4.0/bin/fish");
        assert!(!is_trusted_install_with(exe, ME, Some(MY_GID), |p| {
            Some(if p == exe {
                mine_exe
            } else if p == Path::new("/opt/homebrew/Cellar") {
                open_dir
            } else {
                mine_dir
            })
        }));
        // A relative path and an unreadable file are not installs.
        assert!(!check("bin/sh", root_dir, root_exe));
        assert!(!is_trusted_install_with(
            Path::new("/bin/sh"),
            ME,
            Some(MY_GID),
            |_| None
        ));
        // On disk: a file in a private directory of the user's, and the system shell.
        let dir = PrivateDir::new();
        let fake = dir.path().join("zsh");
        std::fs::write(&fake, "#!/bin/sh\n").unwrap();
        let canon = std::fs::canonicalize(&fake).unwrap();
        if !PACKAGE_PREFIXES.iter().any(|p| canon.starts_with(p)) && unsafe { libc::geteuid() } != 0
        {
            assert!(!is_trusted_install(&canon));
        }
        // The system shell is root's (skipped where it is not, e.g. a Nix-only system).
        if let Ok(sh) = std::fs::canonicalize("/bin/sh")
            && sh.starts_with("/bin")
        {
            assert!(is_trusted_install(&sh), "{sh:?}");
        }
    }

    /// A directory made atomically, private to the test (mode 0700), removed when dropped.
    struct PrivateDir(std::path::PathBuf);

    impl PrivateDir {
        fn new() -> Self {
            use std::os::unix::ffi::OsStrExt;
            let template = std::env::temp_dir().join("thurm-test-XXXXXX");
            let mut buf = std::ffi::CString::new(template.as_os_str().as_bytes())
                .unwrap()
                .into_bytes_with_nul();
            let made = unsafe { libc::mkdtemp(buf.as_mut_ptr().cast()) };
            assert!(!made.is_null(), "{}", std::io::Error::last_os_error());
            buf.pop();
            Self(std::path::PathBuf::from(std::ffi::OsString::from(
                std::ffi::OsStr::from_bytes(&buf),
            )))
        }

        fn path(&self) -> &std::path::Path {
            &self.0
        }
    }

    impl Drop for PrivateDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn same_root_and_namespace_for_self() {
        let me = std::process::id();
        assert!(same_root(me, me));
        // The pane leader's own report, from a root that is not the daemon's: refused even
        // though the reporter is its own reference.
        let (daemon, other) = ((1u64, 2u64), (1u64, 99u64));
        assert!(!roots_agree(Some(other), Some(daemon), Some(Some(other))));
        assert!(roots_agree(Some(daemon), Some(daemon), Some(Some(daemon))));
        // An uninspectable reference (macOS login) leaves the daemon's root to compare with.
        assert!(roots_agree(Some(daemon), Some(daemon), None));
        assert!(!roots_agree(Some(other), Some(daemon), None));
        // A reference read but elsewhere, or not resolvable, is not a match.
        assert!(!roots_agree(Some(daemon), Some(daemon), Some(Some(other))));
        assert!(!roots_agree(Some(daemon), Some(daemon), Some(None)));
        // Nothing read proves nothing.
        assert!(!roots_agree(None, Some(daemon), None));
        assert!(!roots_agree(Some(daemon), None, None));
        // The same directory reached through another mount (a bind mount of `/` without the
        // mounts under it) is not the same root.
        let (root, bind) = ((64769u64, 2u64, 1u64), (64769u64, 2u64, 731u64));
        assert!(!roots_agree(Some(bind), Some(root), None));
        assert!(!roots_agree(Some(root), Some(root), Some(Some(bind))));
        assert_eq!(
            fdinfo_mnt_id("pos:\t0\nflags:\t012100000\nmnt_id:\t731\nino:\t2\n"),
            Some(731)
        );
        assert_eq!(fdinfo_mnt_id("pos:\t0\nflags:\t012100000\n"), None);
        assert_eq!(fdinfo_mnt_id("mnt_id:\tx\n"), None);
        #[cfg(target_os = "linux")]
        {
            let own = root_identity("/proc/self/root").expect("own root identity");
            assert_eq!(root_identity(&format!("/proc/{me}/root")), Some(own));
        }
        assert!(same_mount_ns(me));
        assert!(
            !same_root(u32::MAX - 7, me),
            "an unreadable process is not proven"
        );
    }

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
    fn listeners_with_their_addresses() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let me = std::process::id();
        let found = listeners(&[me]).expect("a look");
        let mine = found.by_pid.get(&me).cloned().unwrap_or_default();
        assert!(
            mine.contains(&Listener {
                port,
                address: "127.0.0.1".into()
            }),
            "{mine:?}"
        );
        assert!(found.unread.is_empty(), "{:?}", found.unread);
    }

    #[test]
    fn listener_addresses_from_proc_net_tables() {
        let v4 = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode
   0: 0100007F:1F90 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 41234 1 0000000000000000 100 0 0 10 0
   1: 00000000:1435 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 41236 1 0000000000000000 100 0 0 10 0
   2: 0100007F:A1B2 0100007F:1F90 01 00000000:00000000 00:00000000 00000000  1000        0 41235 1 0000000000000000 20 4 30 10 -1
";
        let l = |port: u16, address: &str| Listener {
            port,
            address: address.into(),
        };
        assert_eq!(
            listening_sockets_with_address(v4),
            vec![(41234, l(8080, "127.0.0.1")), (41236, l(5173, "0.0.0.0"))]
        );
        let v6 = "  sl  local_address                         remote_address                        st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode
   0: 00000000000000000000000001000000:1435 00000000000000000000000000000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 51234 1 0000000000000000 100 0 0 10 0
   1: 00000000000000000000000000000000:0BB8 00000000000000000000000000000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 51235 1 0000000000000000 100 0 0 10 0
   2: 0000000000000000FFFF00000100007F:0BB9 00000000000000000000000000000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 51236 1 0000000000000000 100 0 0 10 0
";
        assert_eq!(
            listening_sockets_with_address(v6),
            vec![
                (51234, l(5173, "::1")),
                (51235, l(3000, "::")),
                (51236, l(3001, "127.0.0.1"))
            ]
        );
    }

    #[test]
    fn an_unreadable_fd_link_makes_a_partial_read() {
        use std::io::{Error, ErrorKind};
        let link = |t: &str| Ok(std::path::PathBuf::from(t));
        // All read: the sockets, complete.
        let (inodes, partial) = socket_inodes(
            [
                link("socket:[41234]"),
                link("/dev/null"),
                link("socket:[7]"),
            ]
            .into_iter(),
        );
        assert_eq!(inodes, [41234, 7]);
        assert!(!partial);
        // An fd closed between listing and reading: not a failure.
        let (inodes, partial) = socket_inodes(
            [
                link("socket:[41234]"),
                Err(Error::from(ErrorKind::NotFound)),
            ]
            .into_iter(),
        );
        assert_eq!(inodes, [41234]);
        assert!(!partial);
        // A link that may not be read: partial, the sockets found kept.
        let (inodes, partial) = socket_inodes(
            [
                Err(Error::from(ErrorKind::PermissionDenied)),
                link("socket:[41234]"),
            ]
            .into_iter(),
        );
        assert_eq!(inodes, [41234]);
        assert!(partial);
        // Gone with its process (ESRCH): not partial either.
        let (_, partial) = socket_inodes(
            [
                Err(Error::from_raw_os_error(libc::ESRCH)),
                link("socket:[1]"),
            ]
            .into_iter(),
        );
        assert!(!partial);
        // A passing failure (out of descriptors, an I/O error): unknown, so partial.
        for errno in [libc::EMFILE, libc::EIO] {
            let (inodes, partial) = socket_inodes(
                [link("socket:[1]"), Err(Error::from_raw_os_error(errno))].into_iter(),
            );
            assert_eq!(inodes, [1]);
            assert!(partial, "errno {errno}");
        }
    }

    /// A pid's fd links (each a target or its read's error), or the error listing them gives.
    type FakeFds = Result<Vec<Result<&'static str, std::io::Error>>, std::io::ErrorKind>;

    /// A made-up `/proc`: each pid's tables and fd links, or the error reading them gives.
    #[derive(Default)]
    struct FakeProc {
        tables: std::collections::HashMap<u32, Result<String, std::io::ErrorKind>>,
        fds: std::collections::HashMap<u32, FakeFds>,
        /// An OS error (errno) reading the pid's tables, or listing its descriptors.
        raw_tables: std::collections::HashMap<u32, i32>,
        raw_fds: std::collections::HashMap<u32, i32>,
        /// Pids whose kernel has no IPv6: no `tcp6` table.
        no_tcp6: std::collections::HashSet<u32>,
    }

    /// `e` again, its OS error code kept.
    fn again(e: &std::io::Error) -> std::io::Error {
        e.raw_os_error()
            .map(std::io::Error::from_raw_os_error)
            .unwrap_or_else(|| e.kind().into())
    }

    impl ProcFs for FakeProc {
        fn netns(&self, pid: u32) -> Option<std::path::PathBuf> {
            // Each pid in a namespace of its own: every table is read.
            Some(std::path::PathBuf::from(format!("net:[{pid}]")))
        }

        fn table(&self, pid: u32, name: &str) -> std::io::Result<String> {
            if let Some(&errno) = self.raw_tables.get(&pid) {
                return Err(std::io::Error::from_raw_os_error(errno));
            }
            if name == "tcp6" && self.no_tcp6.contains(&pid) {
                return Err(std::io::ErrorKind::NotFound.into());
            }
            match self.tables.get(&pid) {
                Some(Ok(t)) if name == "tcp" => Ok(t.clone()),
                Some(Ok(_)) => Ok(HEADER.into()),
                Some(Err(k)) => Err((*k).into()),
                None => Err(std::io::ErrorKind::NotFound.into()),
            }
        }

        fn fd_links(&self, pid: u32) -> std::io::Result<Vec<std::io::Result<std::path::PathBuf>>> {
            if let Some(&errno) = self.raw_fds.get(&pid) {
                return Err(std::io::Error::from_raw_os_error(errno));
            }
            match self.fds.get(&pid) {
                Some(Ok(links)) => Ok(links
                    .iter()
                    .map(|l| match l {
                        Ok(t) => Ok(std::path::PathBuf::from(t)),
                        Err(e) => Err(again(e)),
                    })
                    .collect()),
                Some(Err(k)) => Err((*k).into()),
                None => Err(std::io::ErrorKind::NotFound.into()),
            }
        }
    }

    const HEADER: &str = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n";

    /// A tcp table with one LISTEN socket on 127.0.0.1:`port`, inode `inode`.
    fn listening(port: u16, inode: u64) -> String {
        format!(
            "{HEADER}   0: 0100007F:{port:04X} 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 {inode} 1 0000000000000000 100 0 0 10 0\n"
        )
    }

    #[test]
    fn only_a_process_gone_is_not_unread() {
        use std::io::ErrorKind::{NotFound, PermissionDenied};
        let esrch = || std::io::Error::from_raw_os_error(3);
        let mut fs = FakeProc::default();
        // 10: listening, all read.
        fs.tables.insert(10, Ok(listening(5173, 100)));
        fs.fds
            .insert(10, Ok(vec![Ok("socket:[100]"), Ok("/dev/null")]));
        // 11: exited and not reaped yet (a zombie): `/proc/11` is there, its tables are not.
        fs.tables.insert(11, Err(NotFound));
        // 12: exits between reading its table and listing its descriptors.
        fs.tables.insert(12, Ok(listening(3000, 200)));
        fs.fds.insert(12, Err(NotFound));
        // 13: a descriptor closed between listing and reading, another gone with the process.
        fs.tables.insert(13, Ok(listening(8080, 300)));
        fs.fds.insert(
            13,
            Ok(vec![
                Ok("socket:[300]"),
                Err(std::io::Error::from(NotFound)),
                Err(esrch()),
            ]),
        );
        // 14, 15, 16: another user's process, refused at the table, the fd directory, a link.
        fs.tables.insert(14, Err(PermissionDenied));
        fs.tables.insert(15, Ok(listening(9000, 500)));
        fs.fds.insert(15, Err(PermissionDenied));
        fs.tables.insert(16, Ok(listening(9100, 600)));
        fs.fds.insert(
            16,
            Ok(vec![
                Ok("socket:[600]"),
                Err(std::io::Error::from(PermissionDenied)),
            ]),
        );
        // 17: no listening socket in its namespace, descriptors refused: it listens on none.
        fs.tables.insert(17, Ok(HEADER.into()));
        fs.fds.insert(17, Err(PermissionDenied));
        // 18, 19, 20: a passing failure (out of descriptors, an I/O error) at the table, the fd
        // directory, a link: unknown, not gone.
        let emfile = || std::io::Error::from_raw_os_error(libc::EMFILE);
        let eio = || std::io::Error::from_raw_os_error(libc::EIO);
        fs.raw_tables.insert(18, libc::EMFILE);
        fs.tables.insert(19, Ok(listening(9200, 700)));
        fs.raw_fds.insert(19, libc::EIO);
        fs.tables.insert(20, Ok(listening(9300, 800)));
        fs.fds
            .insert(20, Ok(vec![Ok("socket:[800]"), Err(emfile()), Err(eio())]));
        // 21: gone between its table and its descriptors (ESRCH).
        fs.tables.insert(21, Ok(listening(9400, 900)));
        fs.raw_fds.insert(21, libc::ESRCH);
        // 22: a kernel without IPv6 (no tcp6 table): its IPv4 listener, complete.
        fs.tables.insert(22, Ok(listening(9500, 1000)));
        fs.no_tcp6.insert(22);
        fs.fds.insert(22, Ok(vec![Ok("socket:[1000]")]));

        let found = listeners_in(&[10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22], &fs);
        let mut unread: Vec<u32> = found.unread.iter().copied().collect();
        unread.sort();
        assert_eq!(unread, [14, 15, 16, 18, 19, 20]);
        let port = |pid| {
            found
                .by_pid
                .get(&pid)
                .map(|l| l.iter().map(|l| l.port).collect::<Vec<_>>())
        };
        assert_eq!(port(10), Some(vec![5173]));
        assert_eq!(
            port(13),
            Some(vec![8080]),
            "the socket read before the others went"
        );
        assert_eq!(
            port(16),
            Some(vec![9100]),
            "a partial read keeps what it found"
        );
        assert_eq!(port(11), None);
        assert_eq!(port(12), None);
        assert_eq!(port(17), None);
        assert_eq!(
            port(22),
            Some(vec![9500]),
            "IPv4 listeners on a kernel without IPv6"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn another_users_process_is_unread_not_empty() {
        let me = std::process::id();
        assert_eq!(fds_access(me), FdAccess::Readable);
        assert_eq!(fds_access(999_999), FdAccess::Gone);
        // launchd is root's: lsof says nothing of it and exits as for "no listeners".
        if unsafe { libc::geteuid() } == 0 {
            return;
        }
        assert_eq!(fds_access(1), FdAccess::Denied);
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let found = listeners(&[1, me]).expect("a look");
        assert!(found.unread.contains(&1), "{found:?}");
        assert!(!found.unread.contains(&me));
        assert!(found.by_pid[&me].iter().any(|l| l.port == port));
    }

    #[test]
    fn listener_addresses_from_lsof() {
        let out = "p100\nf3\ntIPv4\nn127.0.0.1:5173\nf4\ntIPv6\nn[::1]:5173\n\
                   p200\nf3\ntIPv4\nn*:8080\nf4\ntIPv6\nn*:8080\nf5\ntIPv6\nn[::ffff:127.0.0.1]:9000\n";
        let map = parse_lsof_listeners(out);
        let l = |port: u16, address: &str| Listener {
            port,
            address: address.into(),
        };
        assert_eq!(map[&100], vec![l(5173, "127.0.0.1"), l(5173, "::1")]);
        // As /proc/net/tcp{,6} spell them on Linux.
        assert_eq!(
            map[&200],
            vec![l(8080, "0.0.0.0"), l(8080, "::"), l(9000, "127.0.0.1")]
        );
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
    // The first field after the name is field 3; ppid is field 4.
    proc_stat(format!("/proc/{pid}/stat"))?.get(1)?.parse().ok()
}

/// The fields of a `/proc/<pid>/stat` record after the process name, the state first. The
/// name is whatever bytes the process chose (not necessarily UTF-8, and may hold `)` or
/// spaces), so the record is split after its last `)` before anything is decoded.
#[cfg(target_os = "linux")]
fn proc_stat(path: impl AsRef<std::path::Path>) -> Option<Vec<String>> {
    stat_fields(&std::fs::read(path).ok()?)
}

#[cfg(any(target_os = "linux", test))]
fn stat_fields(stat: &[u8]) -> Option<Vec<String>> {
    let close = stat.iter().rposition(|&b| b == b')')?;
    let rest = std::str::from_utf8(&stat[close + 1..]).ok()?;
    Some(rest.split_ascii_whitespace().map(str::to_owned).collect())
}

/// Real and effective uid from the `Uid:` line of a `/proc/<pid>/status` record, read without
/// decoding the rest (its `Name:` line holds the process name's bytes).
#[cfg(any(target_os = "linux", test))]
fn status_uids(status: &[u8]) -> Option<(u32, u32)> {
    let line = status
        .split(|&b| b == b'\n')
        .find_map(|l| l.strip_prefix(b"Uid:"))?;
    let mut parts = std::str::from_utf8(line).ok()?.split_ascii_whitespace();
    let ruid = parts.next()?.parse().ok()?;
    let euid = parts.next()?.parse().ok()?;
    Some((ruid, euid))
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
    // The first field after the name is field 3; starttime is field 22.
    proc_stat(format!("/proc/{pid}/stat"))?
        .get(19)?
        .parse()
        .ok()
}

/// Like [`process_birth`], but `None` for a process that has exited and not been reaped yet
/// (a zombie keeps its pid and birth until its parent collects it). A thread-group leader that
/// ended with `pthread_exit` shows as a zombie too while its other threads run on: the process
/// is alive as long as one of them is.
#[cfg(target_os = "linux")]
pub fn live_process_birth(pid: u32) -> Option<u64> {
    let fields = proc_stat(format!("/proc/{pid}/stat"))?;
    if !stat_state_is_live(fields.first().map(String::as_str)) && !other_thread_live(pid) {
        return None;
    }
    fields.get(19)?.parse().ok()
}

/// Whether a thread of `pid` other than its leader is still running. Once the whole thread
/// group has exited, only the (zombie) leader is listed.
#[cfg(target_os = "linux")]
fn other_thread_live(pid: u32) -> bool {
    let Ok(tasks) = std::fs::read_dir(format!("/proc/{pid}/task")) else {
        return false;
    };
    let leader = pid.to_string();
    tasks.flatten().any(|t| {
        t.file_name() != leader.as_str()
            && proc_stat(t.path().join("stat"))
                .is_some_and(|f| stat_state_is_live(f.first().map(String::as_str)))
    })
}

/// Whether the state field (the first after the name) of `/proc/<pid>/stat` is a live one:
/// not a zombie (`Z`) or dead (`X`, `x`).
#[cfg(any(target_os = "linux", test))]
fn stat_state_is_live(state: Option<&str>) -> bool {
    !matches!(state, Some("Z" | "X" | "x") | None)
}

#[cfg(target_os = "macos")]
pub fn live_process_birth(pid: u32) -> Option<u64> {
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
    (n == size && info.pbi_status != libc::SZOMB).then(|| {
        info.pbi_start_tvsec
            .saturating_mul(1_000_000)
            .saturating_add(info.pbi_start_tvusec)
    })
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub fn live_process_birth(pid: u32) -> Option<u64> {
    process_birth(pid)
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

/// Whether `name` is one of the known PTY proxy executables.
pub fn is_pty_proxy(name: &str) -> bool {
    PTY_PROXIES.contains(&name)
}

/// Real and effective user IDs of `pid`.
#[cfg(target_os = "linux")]
pub fn process_uids(pid: u32) -> Option<(u32, u32)> {
    status_uids(&std::fs::read(format!("/proc/{pid}/status")).ok()?)
}

#[cfg(target_os = "macos")]
pub fn process_uids(pid: u32) -> Option<(u32, u32)> {
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
    (n == size).then_some((info.pbi_ruid, info.pbi_uid))
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub fn process_uids(_pid: u32) -> Option<(u32, u32)> {
    None
}

/// Full resolved path to `pid`'s executable.
#[cfg(target_os = "linux")]
pub fn exe_path(pid: u32) -> Option<std::path::PathBuf> {
    std::fs::read_link(format!("/proc/{pid}/exe")).ok()
}

#[cfg(target_os = "macos")]
pub fn exe_path(pid: u32) -> Option<std::path::PathBuf> {
    let mut buf = vec![0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
    let n = unsafe { libc::proc_pidpath(pid as i32, buf.as_mut_ptr().cast(), buf.len() as u32) };
    if n <= 0 {
        return None;
    }
    buf.truncate(n as usize);
    use std::os::unix::ffi::OsStringExt;
    Some(std::path::PathBuf::from(std::ffi::OsString::from_vec(buf)))
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub fn exe_path(_pid: u32) -> Option<std::path::PathBuf> {
    None
}

/// Controlling terminal device number (`rdev`) of `pid`, if it has one.
pub fn controlling_tty(pid: u32) -> Option<u64> {
    job(pid).and_then(|j| j.tty)
}

/// Whether `pid` has the daemon's root directory (what it reported is resolved and run in the
/// daemon's view of the filesystem), and `reference_pid`'s too when that one can be inspected
/// (setuid `login(1)` on macOS cannot).
#[cfg(target_os = "linux")]
pub fn same_root(pid: u32, reference_pid: u32) -> bool {
    roots_agree(
        root_identity(&format!("/proc/{pid}/root")),
        root_identity("/proc/self/root"),
        root_identity(&format!("/proc/{reference_pid}/root")).map(Some),
    )
}

/// The identity of the directory a procfs `root` link leads to: its device and inode, and the
/// mount it is reached through (a bind mount of `/` that leaves out the mounts under it has
/// the same device and inode, but not the same view). Read from one handle on it; `None` when
/// any of it cannot be read.
#[cfg(target_os = "linux")]
fn root_identity(link: &str) -> Option<(u64, u64, u64)> {
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    let c = std::ffi::CString::new(link).ok()?;
    let fd = unsafe {
        libc::open(
            c.as_ptr(),
            libc::O_PATH | libc::O_DIRECTORY | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return None;
    }
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstat(fd.as_raw_fd(), &mut st) } != 0 {
        return None;
    }
    let info = std::fs::read_to_string(format!("/proc/self/fdinfo/{}", fd.as_raw_fd())).ok()?;
    let mnt_id = fdinfo_mnt_id(&info)?;
    Some((st.st_dev as u64, st.st_ino as u64, mnt_id))
}

/// The `mnt_id:` field of a `/proc/<pid>/fdinfo/<fd>` file.
#[cfg(any(target_os = "linux", test))]
fn fdinfo_mnt_id(info: &str) -> Option<u64> {
    info.lines()
        .find_map(|l| l.strip_prefix("mnt_id:"))
        .and_then(|v| v.trim().parse().ok())
}

/// The rule [`same_root`] applies: the reporter's root must be read and be the daemon's (an
/// unreadable one proves nothing), and a readable reference root (`Some(None)`: read, but not
/// one the daemon can resolve) must be that same root too.
#[cfg(any(target_os = "linux", target_os = "macos", test))]
fn roots_agree<T: PartialEq>(
    reporter: Option<T>,
    daemon: Option<T>,
    reference: Option<Option<T>>,
) -> bool {
    match (reporter, daemon) {
        (Some(r), Some(d)) if r == d => reference.is_none_or(|x| x.as_ref() == Some(&r)),
        _ => false,
    }
}

/// Whether `pid` sees the filesystem through the daemon's mount namespace (a private one can
/// replace a subtree while `/` stays the same). False when either cannot be read.
#[cfg(target_os = "linux")]
pub fn same_mount_ns(pid: u32) -> bool {
    match (
        std::fs::read_link(format!("/proc/{pid}/ns/mnt")),
        std::fs::read_link("/proc/self/ns/mnt"),
    ) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

#[cfg(not(target_os = "linux"))]
pub fn same_mount_ns(_pid: u32) -> bool {
    true
}

/// Session id of `pid`.
pub fn session_id(pid: u32) -> Option<u32> {
    let sid = unsafe { libc::getsid(pid as libc::pid_t) };
    (sid > 0).then_some(sid as u32)
}

/// Session of the terminal behind `fd` (on Linux a PTY master answers for its slave).
pub fn terminal_session(fd: std::os::fd::RawFd) -> Option<u32> {
    let sid = unsafe { libc::tcgetsid(fd) };
    (sid > 0).then_some(sid as u32)
}

#[cfg(target_os = "macos")]
pub fn same_root(pid: u32, reference_pid: u32) -> bool {
    use std::os::unix::fs::MetadataExt;
    /// The root directory's vnode identity: `Some(None)` for the implicit system root (XNU
    /// leaves `pvi_rdir` zeroed when the process never called `chroot`), `None` when it
    /// cannot be read. The stat fields are authoritative; the path XNU rebuilds is not.
    fn root_id(p: u32) -> Option<Option<(u64, u64)>> {
        let mut info: libc::proc_vnodepathinfo = unsafe { std::mem::zeroed() };
        let size = std::mem::size_of::<libc::proc_vnodepathinfo>() as i32;
        let n = unsafe {
            libc::proc_pidinfo(
                p as i32,
                libc::PROC_PIDVNODEPATHINFO,
                0,
                (&mut info as *mut libc::proc_vnodepathinfo).cast(),
                size,
            )
        };
        if n != size {
            return None;
        }
        let st = &info.pvi_rdir.vip_vi.vi_stat;
        Some((st.vst_ino != 0).then_some((st.vst_dev as u64, st.vst_ino)))
    }
    // The implicit root is `/` as the daemon sees it, provided the daemon's own root is the
    // implicit one (or that same `/`).
    let system_root = || -> Option<(u64, u64)> {
        let m = std::fs::metadata("/").ok()?;
        let slash = (m.dev(), m.ino());
        match root_id(std::process::id())? {
            None => Some(slash),
            Some(own) if own == slash => Some(slash),
            Some(_) => None,
        }
    };
    let resolve = |id: Option<(u64, u64)>| id.or_else(system_root);
    roots_agree(
        root_id(pid).and_then(resolve),
        root_id(std::process::id()).and_then(resolve),
        root_id(reference_pid).map(resolve),
    )
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub fn same_root(_pid: u32, _reference_pid: u32) -> bool {
    true
}

/// Walks parent pointers from `pid` up toward `stop_at`, until it is reached or the walk ends
/// (pid 1, no readable parent, a cycle). Returns `(ancestors, reached_stop_at)` where
/// `ancestors` starts with `pid`'s parent and includes `stop_at` when reached (`ancestors` is
/// empty and `reached_stop_at` is `true` when `pid == stop_at`). [`MAX_ANCESTRY`] only guards
/// against a parent table changing under the walk: running out of it means "unknown", which
/// callers treat as not a descendant.
pub fn ancestors(pid: u32, stop_at: u32) -> (Vec<u32>, bool) {
    ancestors_with(pid, stop_at, parent)
}

/// Far more hops than any real process tree has.
pub const MAX_ANCESTRY: usize = 4096;

fn ancestors_with(
    pid: u32,
    stop_at: u32,
    parent_of: impl Fn(u32) -> Option<u32>,
) -> (Vec<u32>, bool) {
    if pid == stop_at {
        return (Vec::new(), true);
    }
    let mut chain = Vec::new();
    let mut seen = std::collections::HashSet::new();
    seen.insert(pid);
    let mut cur = pid;
    while chain.len() < MAX_ANCESTRY {
        let Some(ppid) = parent_of(cur) else {
            return (chain, false);
        };
        if ppid == 0 || !seen.insert(ppid) {
            return (chain, false);
        }
        chain.push(ppid);
        if ppid == stop_at {
            return (chain, true);
        }
        if ppid == 1 {
            return (chain, false);
        }
        cur = ppid;
    }
    (chain, false)
}

/// Directories a package manager installs shells and tools into. Their trees belong to the
/// user on some systems (Homebrew's prefix is its admin's, group-writable by `admin`; a
/// single-user Nix store is the user's), so they are trusted when only root, the daemon's user
/// or a group standing for those can change them, rather than only when root owns them.
/// Those groups are root's, macOS's `admin` (whose members can become root anyway) and the
/// user's private group; any other group may only write to the Nix store, a sticky directory
/// of root's where its build users cannot replace what they do not own.
const PACKAGE_PREFIXES: &[&str] = &[
    "/opt/homebrew",
    "/usr/local",
    "/home/linuxbrew/.linuxbrew",
    "/nix/store",
    "/run/current-system",
];

/// What [`is_trusted_install`] looks at in a file or directory.
#[derive(Clone, Copy, Debug)]
struct Ownership {
    uid: u32,
    gid: u32,
    mode: u32,
    is_dir: bool,
    is_file: bool,
}

/// Whether the canonical executable `exe` is a trusted installation: owned by root with every
/// directory up to `/` owned by root and writable by nobody else, or, under a package-manager
/// prefix, with the executable and every directory up to `/` changeable by nobody but root and
/// the daemon's user. A program another account (or anyone) can replace is not, wherever it is;
/// one the user put somewhere outside a package prefix is not either.
pub fn is_trusted_install(exe: &std::path::Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    static PRIVATE_GID: std::sync::OnceLock<Option<u32>> = std::sync::OnceLock::new();
    let (uid, gid) = unsafe { (libc::geteuid(), libc::getegid()) };
    let private_gid = *PRIVATE_GID.get_or_init(|| private_group(uid, gid));
    is_trusted_install_with(exe, uid, private_gid, |p| {
        std::fs::metadata(p).ok().map(|m| Ownership {
            uid: m.uid(),
            gid: m.gid(),
            mode: m.mode(),
            is_dir: m.is_dir(),
            is_file: m.is_file(),
        })
    })
}

/// `gid` when it is `uid`'s own group, which nobody else is in: named after the user, listing
/// no other member, and no other account's primary group (a user-private group, which Linux
/// distributions give each account and Linuxbrew's installer makes its prefix writable by).
/// `gr_mem` lists only supplementary members, so the accounts are read too; if that listing
/// does not even show the user (a directory service that doesn't enumerate), the group is
/// not taken as private. A shared primary group (macOS's `staff`, a `users` group) is not,
/// nor one that cannot be read.
fn private_group(uid: u32, gid: u32) -> Option<u32> {
    use std::ffi::CStr;
    let mut buf = vec![0 as libc::c_char; 16384];
    let user = unsafe {
        let mut pw: libc::passwd = std::mem::zeroed();
        let mut out = std::ptr::null_mut();
        let rc = libc::getpwuid_r(uid, &mut pw, buf.as_mut_ptr(), buf.len(), &mut out);
        if rc != 0 || out.is_null() || pw.pw_name.is_null() {
            return None;
        }
        CStr::from_ptr(pw.pw_name).to_bytes().to_vec()
    };
    let mut gbuf = vec![0 as libc::c_char; 65536];
    let (group, members) = unsafe {
        let mut gr: libc::group = std::mem::zeroed();
        let mut out = std::ptr::null_mut();
        let rc = libc::getgrgid_r(gid, &mut gr, gbuf.as_mut_ptr(), gbuf.len(), &mut out);
        if rc != 0 || out.is_null() || gr.gr_name.is_null() {
            return None;
        }
        let mut members = Vec::new();
        let mut m = gr.gr_mem;
        while !m.is_null() && !(*m).is_null() {
            members.push(CStr::from_ptr(*m).to_bytes().to_vec());
            m = m.add(1);
        }
        (CStr::from_ptr(gr.gr_name).to_bytes().to_vec(), members)
    };
    // Every account whose primary group this is. `getpwent` is not reentrant: this runs once
    // (cached by the caller), and nothing else in the daemon walks the accounts.
    let mut primary = Vec::new();
    unsafe {
        libc::setpwent();
        loop {
            let pw = libc::getpwent();
            if pw.is_null() {
                break;
            }
            if (*pw).pw_gid == gid && !(*pw).pw_name.is_null() {
                primary.push(CStr::from_ptr((*pw).pw_name).to_bytes().to_vec());
            }
        }
        libc::endpwent();
    }
    is_private_group(&user, &group, &members, &primary).then_some(gid)
}

/// `group` is `user`'s alone: named after them, with no other supplementary member, and the
/// primary group of no other account (`primary`, which must show the user: a listing without
/// them is incomplete).
fn is_private_group(user: &[u8], group: &[u8], members: &[Vec<u8>], primary: &[Vec<u8>]) -> bool {
    !user.is_empty()
        && group == user
        && members.iter().all(|m| m == user)
        && primary.iter().any(|m| m == user)
        && primary.iter().all(|m| m == user)
}

fn is_trusted_install_with(
    exe: &std::path::Path,
    my_uid: u32,
    private_gid: Option<u32>,
    meta: impl Fn(&std::path::Path) -> Option<Ownership>,
) -> bool {
    if !exe.is_absolute() {
        return false;
    }
    let packaged = PACKAGE_PREFIXES
        .iter()
        .any(|p| exe.starts_with(p) && exe != std::path::Path::new(p));
    let root_only = |m: &Ownership| m.uid == 0 && m.mode & 0o022 == 0;
    // The group may write when it is root's, macOS's `admin` (who can become root anyway) or
    // the user's private one; any other only to the Nix store, root's and sticky: its build
    // users can add entries there, but not replace root's (each checked in turn).
    let admin_gid = if cfg!(target_os = "macos") { 80 } else { 0 };
    let group_may_write = |path: &std::path::Path, m: &Ownership| {
        m.gid == 0
            || m.gid == admin_gid
            || private_gid == Some(m.gid)
            || (m.uid == 0
                && m.is_dir
                && m.mode & 0o1000 != 0
                && path == std::path::Path::new("/nix/store"))
    };
    let package_owned = |path: &std::path::Path, m: &Ownership| {
        (m.uid == 0 || m.uid == my_uid)
            && m.mode & 0o002 == 0
            && (m.mode & 0o020 == 0 || group_may_write(path, m))
    };
    let ok = |path: &std::path::Path, m: &Ownership| {
        if packaged {
            package_owned(path, m)
        } else {
            root_only(m)
        }
    };
    let Some(file) = meta(exe) else {
        return false;
    };
    if !file.is_file || !ok(exe, &file) {
        return false;
    }
    exe.ancestors()
        .skip(1)
        .all(|dir| meta(dir).is_some_and(|m| m.is_dir && ok(dir, &m)))
}

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
    // After the name: state ppid pgrp session tty_nr tpgid.
    let f = proc_stat(format!("/proc/{pid}/stat"))?;
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

// As <sys/proc_info.h> lays it out (`sizeof`, then `offsetof` `pvip` and the fields read).
#[cfg(target_os = "macos")]
const _: () = {
    assert!(std::mem::size_of::<VnodeFdInfo>() == 1200);
    assert!(std::mem::offset_of!(VnodeFdInfo, vnode) == 24);
    assert!(std::mem::offset_of!(VnodeFdInfo, vnode.vip_vi.vi_stat.vst_mode) == 28);
    assert!(std::mem::offset_of!(VnodeFdInfo, vnode.vip_vi.vi_stat.vst_rdev) == 140);
};

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
