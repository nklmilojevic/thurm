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
