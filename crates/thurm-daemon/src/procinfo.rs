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
    let args = rest
        .split(|&b| b == 0)
        .take(argc)
        .map(|s| String::from_utf8_lossy(s).into_owned())
        .collect();
    Some(args)
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
    }
}

/// (pid, ppid, command line).
pub type ProcRow = (u32, u32, String);

/// Every process, from `ps`.
pub fn process_table() -> Vec<ProcRow> {
    let Ok(out) = std::process::Command::new("/bin/ps")
        .args(["-axo", "pid=,ppid=,args="])
        .output()
    else {
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
        let netns = std::fs::read_link(format!("/proc/{pid}/ns/net"))
            .unwrap_or_else(|_| format!("pid:{pid}").into());
        let ports_by_inode = by_netns.entry(netns).or_insert_with(|| {
            ["tcp", "tcp6"]
                .iter()
                .filter_map(|t| std::fs::read_to_string(format!("/proc/{pid}/net/{t}")).ok())
                .flat_map(|text| listening_sockets(&text))
                .collect()
        });
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
    let Ok(out) = std::process::Command::new("/usr/sbin/lsof")
        .args(["-nP", "-a", "-iTCP", "-sTCP:LISTEN", "-Fpn", "-p", &list])
        .output()
    else {
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
            Err(_) => return,
        };
        let mut line = String::new();
        use std::io::BufRead;
        std::io::BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        let Ok(port) = line.trim().parse::<u16>() else {
            let _ = child.kill();
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
}
