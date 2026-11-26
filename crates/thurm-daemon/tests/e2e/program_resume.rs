//! Program Resume Protocol (OSC 7577) end-to-end tests through the real daemon and PTYs.

use super::*;
use std::os::unix::fs::PermissionsExt;

/// Shell snippet that signs and emits an OSC 7577 report for the current shell (`$$`) or `$1`.
/// Usage in POSIX sh: `emit_7577 <pid> <extra_pairs>` (e.g. `emit_7577 "$$" "argv=..."`).
const EMIT_SH: &str = r#"
emit_7577() {
  _pid="$1"
  _rest="$2"
  _t="$(date +%s)"
  _body="pid=${_pid}:time=${_t}:${_rest}"
  _mac="$(printf '%s:%s' "$_body" "$OSC7577_TOKEN" | { sha256sum 2>/dev/null || shasum -a 256; } | cut -c1-64)"
  printf '\033]7577;%s:mac=%s\033\\' "$_body" "$_mac"
}
"#;

fn b64(s: &str) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(s.as_bytes())
}

fn b64_argv(args: &[&str]) -> String {
    args.iter().map(|a| b64(a)).collect::<Vec<_>>().join(",")
}

fn write_exec(path: &Path, body: &str) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, body).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

thread_local! {
    /// The socket of the daemon this test connected to last (`Env::connect`).
    pub(crate) static RECORDS_SOCKET: std::cell::RefCell<Option<PathBuf>> =
        const { std::cell::RefCell::new(None) };
}

/// The daemon answers `ResumeRecords` only to the `thurm` CLI (by its executable's name), so
/// the tests ask through this test binary copied under that name into the test's directory
/// (removed with it), running [`records_cli_helper`]: the check the daemon makes is the real
/// one.
fn records_cli(socket: &Path) -> PathBuf {
    let cli = socket.parent().unwrap().join("cli").join("thurm");
    if !cli.exists() {
        std::fs::create_dir_all(cli.parent().unwrap()).unwrap();
        let part = cli.with_extension("part");
        std::fs::copy(std::env::current_exe().unwrap(), &part).unwrap();
        std::fs::set_permissions(&part, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::rename(&part, &cli).unwrap();
    }
    cli
}

fn resume_records(_c: &Client, pane: Option<PaneId>) -> Vec<PaneResumeRecords> {
    let socket = RECORDS_SOCKET
        .with(|s| s.borrow().clone())
        .expect("connect to a daemon first");
    let out = Command::new(records_cli(&socket))
        .args([
            "--exact",
            "program_resume::records_cli_helper",
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .env("THURM_E2E_RECORDS_SOCKET", &socket)
        .env(
            "THURM_E2E_RECORDS_PANE",
            pane.map(|p| p.to_string()).unwrap_or_default(),
        )
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    // On a line of its own, though libtest may print the test's name before it.
    let Some(json) = stdout
        .lines()
        .find_map(|l| l.split_once("RECORDS_JSON:").map(|(_, j)| j))
    else {
        panic!(
            "no records from the CLI helper: {stdout}{}",
            String::from_utf8_lossy(&out.stderr)
        );
    };
    serde_json::from_str(json).unwrap()
}

/// Run by [`resume_records`] from the copy named `thurm`: asks the daemon for the records and
/// prints them. Does nothing when run as a test.
#[test]
#[ignore = "run by resume_records as the thurm CLI"]
fn records_cli_helper() {
    let Some(socket) = std::env::var_os("THURM_E2E_RECORDS_SOCKET") else {
        return;
    };
    let pane = std::env::var("THURM_E2E_RECORDS_PANE")
        .ok()
        .filter(|p| !p.is_empty())
        .map(|p| p.parse::<PaneId>().unwrap());
    let c = Client::connect(
        ConnectOptions {
            socket: PathBuf::from(socket),
            spawn_daemon: None,
            client_name: "thurm",
            ui: false,
        },
        |_| {},
        || {},
    )
    .expect("connect");
    match c.request(Request::ResumeRecords { pane }).unwrap() {
        Response::ResumeRecords(list) => {
            println!("\nRECORDS_JSON:{}", serde_json::to_string(&list).unwrap());
        }
        other => panic!("unexpected response: {other:?}"),
    }
}

fn wait_records(
    c: &Client,
    pane: PaneId,
    what: &str,
    pred: impl Fn(&[ResumeRecordInfo]) -> bool,
) -> Vec<ResumeRecordInfo> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let recs = resume_records(c, Some(pane))
            .into_iter()
            .next()
            .map(|p| p.records)
            .unwrap_or_default();
        if pred(&recs) {
            return recs;
        }
        assert!(Instant::now() < deadline, "{what}: {recs:?}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn run_line(c: &Client, pane: PaneId, line: &str) {
    c.request(Request::Input {
        pane,
        data: format!("{line}\r").into_bytes(),
    })
    .unwrap();
}

fn write_config(env: &Env, text: &str) {
    std::fs::write(env.dir.join("config/config.toml"), text).unwrap();
}

/// Waits until `session.json` satisfies `pred` (the daemon writes it after updating the
/// records the client sees, so it may lag them).
fn wait_snapshot(env: &Env, what: &str, pred: impl Fn(&serde_json::Value) -> bool) {
    let path = env.dir.join("state/session.json");
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let snap = std::fs::read(&path)
            .ok()
            .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok());
        if snap.as_ref().is_some_and(&pred) {
            return;
        }
        assert!(Instant::now() < deadline, "{what}: {snap:?}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Pane `i`'s saved record in `snap`, opened with the daemon's key (beside the state
/// directory), or `null` when it has none. The record is sealed on disk.
fn opened_record(env: &Env, snap: &serde_json::Value, i: usize) -> serde_json::Value {
    use aes_gcm::aead::{Aead, KeyInit, Payload};
    use base64::Engine;
    use hmac::{Hmac, Mac};
    let pane = &snap["panes"][i];
    let Some(sealed) = pane["program_resume"].as_str() else {
        return serde_json::Value::Null;
    };
    let evidence = std::fs::read(env.dir.join("state-keys/resume-evidence.key")).unwrap();
    let mut mac = <Hmac<sha2::Sha256> as hmac::KeyInit>::new_from_slice(&evidence).unwrap();
    mac.update(b"thurm program resume record seal v1");
    let key: [u8; 32] = mac.finalize().into_bytes().into();
    let data = base64::engine::general_purpose::STANDARD
        .decode(sealed.strip_prefix("sealed-v1:").unwrap())
        .unwrap();
    let (nonce, msg) = data.split_at(12);
    let nonce: [u8; 12] = nonce.try_into().unwrap();
    let aad = format!("thurm program resume record of pane {}", pane["id"]);
    let json = aes_gcm::Aes256Gcm::new(&key.into())
        .decrypt(
            &nonce.into(),
            Payload {
                msg,
                aad: aad.as_bytes(),
            },
        )
        .unwrap();
    serde_json::from_slice(&json).unwrap()
}

fn sha256_hex(s: &str) -> String {
    use sha2::Digest;
    sha2::Sha256::digest(s.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn find_shell(name: &str) -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
        .chain([PathBuf::from("/bin"), PathBuf::from("/usr/bin")])
        .map(|d| d.join(name))
        .find(|p| p.is_file())
}

fn running_as_root() -> bool {
    unsafe { libc::geteuid() == 0 || libc::getuid() == 0 }
}

#[test]
fn token_is_unique_per_pane_valid_and_never_inherited() {
    let env = Env::new("resume-token");
    write_config(
        &env,
        r#"
        [terminal]
        shell = ["/bin/sh"]
        [terminal.env]
        OSC7577_TOKEN = "config-must-not-override-token"
        [session]
        snapshot_interval_secs = 1
        "#,
    );
    let _d = env.start_with(&[("OSC7577_TOKEN", Path::new("parent-leaked-token-0123456789"))]);
    let (c, _) = env.connect();

    let mut tokens = Vec::new();
    for i in 0..2 {
        let resp = c
            .request(Request::CreatePane(CreatePane {
                cwd: Some(env.dir.display().to_string()),
                env: vec![(
                    "OSC7577_TOKEN".into(),
                    "client-must-not-override-token".into(),
                )],
                size: PaneSize {
                    cols: 120,
                    rows: 24,
                    ..Default::default()
                },
                ..Default::default()
            }))
            .unwrap();
        let Response::PaneCreated { pane } = resp else {
            panic!("{resp:?}");
        };
        // The pane checks its token's shape and writes only that verdict and a digest: a live
        // token never reaches a file or the test output.
        let out_file = env.dir.join(format!("tok-{i}"));
        run_line(
            &c,
            pane,
            &format!(
                "t=\"$OSC7577_TOKEN\"; \
                 if [ -z \"$t\" ]; then v=empty; \
                 elif printf '%s' \"$t\" | LC_ALL=C grep -Eq '^[A-Za-z0-9_-]{{22,64}}$'; then v=valid; \
                 else v=invalid; fi; \
                 d=$(printf '%s' \"$t\" | {{ sha256sum 2>/dev/null || shasum -a 256; }} | cut -c1-64); \
                 printf '%s %s' \"$v\" \"$d\" > '{}'; unset t d v; printf 'TOK_%s_%d\\n' DONE {i}",
                out_file.display()
            ),
        );
        wait_match(&c, pane, &format!("TOK_DONE_{i}"));
        let report = std::fs::read_to_string(&out_file).unwrap();
        let (verdict, digest) = report.split_once(' ').unwrap();
        // A failed hash would leave nothing to compare: the uniqueness check below would
        // prove nothing.
        assert!(
            running_as_root()
                || (digest.len() == 64 && digest.bytes().all(|b| b.is_ascii_hexdigit())),
            "no SHA-256 digest of the token: {digest:?}"
        );
        if running_as_root() {
            assert_eq!(
                verdict, "empty",
                "OSC7577_TOKEN must be unset when running as root"
            );
            continue;
        }
        assert_eq!(verdict, "valid", "token length or charset out of spec");
        for foreign in [
            "parent-leaked-token-0123456789",
            "config-must-not-override-token",
            "client-must-not-override-token",
        ] {
            assert!(digest != sha256_hex(foreign), "pane token is {foreign}");
        }
        tokens.push(digest.to_owned());
    }
    if !running_as_root() {
        assert!(tokens[0] != tokens[1], "panes must never share a token");
    }
}

#[test]
fn report_persists_resumes_verbatim_without_shell_parsing_and_never_retries_twice() {
    if running_as_root() {
        return;
    }
    let env = Env::new("resume-verbatim");
    write_config(
        &env,
        r#"
        [terminal]
        shell = ["/bin/sh"]
        [session]
        snapshot_interval_secs = 60
        resume_programs = "all"
        "#,
    );
    let marker = env.dir.join("never-created");
    let count_file = env.dir.join("run-count");
    let args_file = env.dir.join("got-args");
    let prog = env.dir.join("bin/demo-prog");
    write_exec(
        &prog,
        &format!(
            "#!/bin/sh\n\
             echo x >> '{}'\n\
             : > '{}'\n\
             for a in \"$@\"; do printf '[%s]\\n' \"$a\" >> '{}'; done\n\
             echo RESUMED_OK\n",
            count_file.display(),
            args_file.display(),
            args_file.display(),
        ),
    );
    let reporter = env.dir.join("bin/reporter");
    let adversarial = format!("$(touch '{}'); a; 'q' \"d\" * ?", marker.display());
    let encoded_argv = b64_argv(&[
        &prog.display().to_string(),
        "--flag",
        "",
        &adversarial,
        "plain",
    ]);
    write_exec(
        &reporter,
        &format!(
            "#!/bin/sh\n{EMIT_SH}\n\
             emit_7577 \"$$\" \"argv={encoded_argv}\"\n\
             # Bad MAC and stale time must be ignored and leave the record intact.\n\
             printf '\\033]7577;pid=%s:time=%s:clear=1:mac=0000000000000000000000000000000000000000000000000000000000000000\\033\\\\' \"$$\" \"$(date +%s)\"\n\
             _old=\"pid=$$:time=1000000000:clear=1\"\n\
             _old_mac=\"$(printf '%s:%s' \"$_old\" \"$OSC7577_TOKEN\" | {{ sha256sum 2>/dev/null || shasum -a 256; }} | cut -c1-64)\"\n\
             printf '\\033]7577;%s:mac=%s\\033\\\\' \"$_old\" \"$_old_mac\"\n\
             # RIS, DECSTR, alt screen, and OSC 133;A must NOT remove a resume record.\n\
             printf '\\033c\\033[!p\\033[?1049h\\033[?1049l\\033]133;A\\033\\\\'\n\
             echo REPORTED\n\
             sleep 60\n"
        ),
    );

    let mut daemon = env.start();
    let (c, _) = env.connect();
    let pane = create(&c, &env.dir);
    // The new pane made the session dirty: saved now, so only the report can save it again.
    c.request(Request::SaveSnapshot).unwrap();
    run_line(&c, pane, &format!("'{}'", reporter.display()));
    wait_match(&c, pane, "REPORTED");

    let recs = wait_records(&c, pane, "accepted record", |r| r.len() == 1);
    assert!(recs[0].chosen);
    assert!(recs[0].trusted);
    assert_eq!(recs[0].argv[3], adversarial);

    // Persisted on acceptance (not at the 60 s snapshot interval).
    let want = prog.display().to_string();
    wait_snapshot(&env, "record saved", |snap| {
        opened_record(&env, snap, 0)["argv"][0] == want.as_str()
    });
    // Sealed: the reported arguments are not in the file.
    let bytes = std::fs::read_to_string(env.dir.join("state/session.json")).unwrap();
    assert!(
        !bytes.contains("--flag") && !bytes.contains("$(touch"),
        "{bytes}"
    );

    // Cold restart.
    c.request(Request::Shutdown { kill_panes: true }).unwrap();
    daemon.wait_exit();
    drop(c);

    let mut daemon2 = env.start();
    let (c2, _) = env.connect();
    wait_match(&c2, pane, "RESUMED_OK");
    assert!(!marker.exists(), "shell evaluated $(touch ...) in argv!");
    let got = std::fs::read_to_string(&args_file).unwrap();
    assert_eq!(got, format!("[--flag]\n[]\n[{adversarial}]\n[plain]\n"));
    assert_eq!(
        std::fs::read_to_string(&count_file)
            .unwrap()
            .lines()
            .count(),
        1
    );

    // After the resumed command exited, the shell is still interactive: only a shell running
    // the line prints the joined word (the terminal's echo of the typed line would not).
    run_line(&c2, pane, "printf 'SHELL_%s\\n' ALIVE");
    wait_match(&c2, pane, "SHELL_ALIVE");

    // Second restart without reporting again must NOT re-run the program.
    c2.request(Request::Shutdown { kill_panes: true }).unwrap();
    daemon2.wait_exit();
    drop(c2);

    let _daemon3 = env.start();
    let (c3, _) = env.connect();
    // Untouched past the typed resume's due time (2.5 s) and a monitor tick: input would
    // cancel a repeat this checks for.
    std::thread::sleep(Duration::from_millis(4500));
    assert_eq!(
        std::fs::read_to_string(&count_file)
            .unwrap()
            .lines()
            .count(),
        1,
        "resume ran a second time"
    );
    run_line(&c3, pane, "printf 'THIRD_%s\\n' START");
    wait_match(&c3, pane, "THIRD_START");
}

#[test]
fn zsh_and_bash_integration_resume_uses_startup_path_bypasses_aliases_and_suspends() {
    if running_as_root() {
        return;
    }
    for shell_name in ["zsh", "bash"] {
        let Some(shell_path) = find_shell(shell_name) else {
            continue;
        };
        let env = Env::new(&format!("resume-hook-{shell_name}"));
        let home = env.dir.join("home");
        let custom_bin = env.dir.join("custom-bin");
        let alias_marker = env.dir.join("alias-hijacked");
        let env_dump = env.dir.join("env-dump");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&custom_bin).unwrap();

        let prog = custom_bin.join("myresumeprog");
        write_exec(
            &prog,
            &format!(
                "#!/bin/sh\n\
                 printf 'RTICKET=%s\\nREXEC=%s\\nRC_VAR=%s\\n' \"${{THURM_RESUME_TICKET-}}\" \"${{THURM_RESUME_EXEC-}}\" \"${{FROM_RC-}}\" > '{}'\n\
                 echo HOOK_RUNNING\n\
                 sleep 30\n",
                env_dump.display()
            ),
        );
        // Traced from the rc file on (as when debugging startup): the trace goes to the pane,
        // and must not show the single-use ticket or the helper.
        let (xtrace, untrace) = if shell_name == "zsh" {
            ("setopt xtrace", "unsetopt xtrace")
        } else {
            ("set -x", "set +x")
        };
        let rc_body = format!(
            "{xtrace}\n\
             export PATH=\"{}:$PATH\"\n\
             export FROM_RC=yes\n\
             myresumeprog() {{ touch '{}'; }}\n\
             alias myresumeprog='touch {}'\n",
            custom_bin.display(),
            alias_marker.display(),
            alias_marker.display(),
        );
        if shell_name == "zsh" {
            // The program is on PATH only once a precmd hook (not one Thurm knows by name)
            // ran: the resume must come after it.
            let zshrc = rc_body.replacen(
                &format!("export PATH=\"{}:$PATH\"\n", custom_bin.display()),
                &format!(
                    "_my_env_hook() {{ [[ :$PATH: == *:{0}:* ]] || export PATH=\"{0}:$PATH\"; }}\n\
                     precmd_functions+=(_my_env_hook)\n",
                    custom_bin.display()
                ),
                1,
            );
            assert_ne!(zshrc, rc_body);
            std::fs::write(home.join(".zshrc"), &zshrc).unwrap();
        } else {
            std::fs::write(home.join(".bashrc"), &rc_body).unwrap();
            std::fs::write(home.join(".bash_profile"), "source ~/.bashrc\n").unwrap();
        }
        write_config(
            &env,
            &format!(
                "[terminal]\nshell = [\"{}\"]\nshell_integration = true\n\
                 [session]\nsnapshot_interval_secs = 60\nresume_programs = \"all\"\n",
                shell_path.display()
            ),
        );

        let reporter = env.dir.join("report.sh");
        let enc = b64_argv(&["myresumeprog"]);
        write_exec(
            &reporter,
            &format!(
                "#!/bin/sh\n{EMIT_SH}\nemit_7577 \"$$\" \"argv={enc}\"\necho READY\nsleep 60\n"
            ),
        );

        let mut daemon = env.start_clean(&[("HOME", &home)]);
        let (c, _) = env.connect();
        let pane = create(&c, &env.dir);
        run_line(&c, pane, &format!("'{}'", reporter.display()));
        wait_match(&c, pane, "READY");
        wait_records(&c, pane, "hook record", |r| r.len() == 1 && r[0].trusted);

        c.request(Request::Shutdown { kill_panes: true }).unwrap();
        daemon.wait_exit();
        drop(c);

        let _daemon2 = env.start_clean(&[("HOME", &home)]);
        let (c2, _) = env.connect();
        wait_match(&c2, pane, "HOOK_RUNNING");
        assert!(
            !alias_marker.exists(),
            "{shell_name}: shell function/alias ran instead of PATH lookup"
        );
        let dumped = std::fs::read_to_string(&env_dump).unwrap();
        assert_eq!(dumped, "RTICKET=\nREXEC=\nRC_VAR=yes\n", "{shell_name}");
        // Neither the single-use ticket nor the helper shows in the pane: zsh redraws the line
        // it runs, which holds only a function's name.
        let shown = capture(&c2, pane);
        assert!(
            shown.contains("FROM_RC=yes"),
            "{shell_name}: no trace: {shown}"
        );
        assert!(
            !shown.contains("--exec-resume") && !shown.contains("thurmd"),
            "{shell_name}: {shown}"
        );

        // Ctrl-Z suspends the resumed foreground job and returns to the interactive shell.
        std::thread::sleep(Duration::from_millis(150));
        c2.request(Request::Input {
            pane,
            data: vec![0x1a],
        })
        .unwrap();
        std::thread::sleep(Duration::from_millis(250));
        // Untraced from here: zsh's prompt hooks trace hundreds of lines, which would scroll
        // the output below off the screen.
        run_line(&c2, pane, untrace);
        // Only a shell running it prints the joined word (an echo of the typed line would not).
        run_line(&c2, pane, "printf 'SUSPENDED_%s\\n' BACK");
        wait_match(&c2, pane, "SUSPENDED_BACK");

        // The command went from the daemon to the helper over the socket, never to disk.
        assert!(
            !env.dir.join("state/resume").exists(),
            "{shell_name}: a resume file was written"
        );
    }
}

/// A shell behind a PTY proxy (`script`, started from the shell's startup file) reports from
/// the proxy's terminal and runs the typed resume line there, not on the pane's: the daemon
/// follows the proxy for both, as it reads the pane.
#[test]
fn a_shell_behind_script_still_resumes() {
    if running_as_root() || !Path::new("/usr/bin/script").exists() {
        return;
    }
    let env = Env::new("resume-script-proxy");
    let custom_bin = env.dir.join("custom-bin");
    write_exec(
        &custom_bin.join("proxiedprog"),
        "#!/bin/sh\necho PROXIED_RESUMED\nsleep 30\n",
    );
    // `sh` starts the proxy from its startup file (`ENV`), once: its shell is the one behind
    // it. The proxy's own syntax: BSD `script [-q] file command...`, util-linux `-q -c cmd file`.
    let proxy = if cfg!(target_os = "macos") {
        "/usr/bin/script -q /dev/null /bin/sh"
    } else {
        "/usr/bin/script -q -c /bin/sh /dev/null"
    };
    let envrc = env.dir.join("envrc");
    std::fs::write(
        &envrc,
        format!("[ -z \"$BEHIND\" ] && {{ export BEHIND=1; exec {proxy}; }}\n"),
    )
    .unwrap();
    write_config(
        &env,
        &format!(
            "[terminal]\nshell = [\"/bin/sh\"]\nshell_integration = true\n\
             [terminal.env]\nENV = {:?}\n\
             [session]\nsnapshot_interval_secs = 60\nresume_programs = \"all\"\n",
            envrc.display().to_string()
        ),
    );
    let reporter = env.dir.join("report.sh");
    let enc = b64_argv(&[&custom_bin.join("proxiedprog").display().to_string()]);
    write_exec(
        &reporter,
        &format!("#!/bin/sh\n{EMIT_SH}\nemit_7577 \"$$\" \"argv={enc}\"\necho READY\nsleep 60\n"),
    );

    let mut daemon = env.start_clean(&[]);
    let (c, _) = env.connect();
    let pane = create(&c, &env.dir);
    // Typed again until the shell behind the proxy reads it (what comes before is lost).
    send_until(
        &c,
        pane,
        format!("'{}'\r", reporter.display()).as_bytes(),
        "(?m)^READY",
    );
    wait_records(&c, pane, "proxied record", |r| r.len() == 1 && r[0].trusted);
    c.request(Request::Shutdown { kill_panes: true }).unwrap();
    daemon.wait_exit();
    drop(c);

    let _daemon2 = env.start_clean(&[]);
    let (c2, _) = env.connect();
    wait_match(&c2, pane, "PROXIED_RESUMED");
}

/// Ctrl-C on a resumed bash job unwinds the prompt hook that started it: bash's own xtrace (on
/// from `.bashrc`) must survive that, so the ticket's untraced run may not turn it off in the
/// interactive shell.
#[test]
fn bash_resume_interrupted_keeps_xtrace() {
    if running_as_root() {
        return;
    }
    let Some(bash) = find_shell("bash") else {
        return;
    };
    let env = Env::new("resume-bash-intr");
    let home = env.dir.join("home");
    let custom_bin = env.dir.join("custom-bin");
    std::fs::create_dir_all(&home).unwrap();
    write_exec(
        &custom_bin.join("intrprog"),
        "#!/bin/sh\necho INTR_RESUMED\nsleep 30\n",
    );
    std::fs::write(
        home.join(".bashrc"),
        format!("set -x\nexport PATH=\"{}:$PATH\"\n", custom_bin.display()),
    )
    .unwrap();
    std::fs::write(home.join(".bash_profile"), "source ~/.bashrc\n").unwrap();
    write_config(
        &env,
        &format!(
            "[terminal]\nshell = [\"{}\"]\nshell_integration = true\n\
             [session]\nsnapshot_interval_secs = 60\nresume_programs = \"all\"\n",
            bash.display()
        ),
    );
    let reporter = env.dir.join("report.sh");
    let enc = b64_argv(&["intrprog"]);
    write_exec(
        &reporter,
        &format!("#!/bin/sh\n{EMIT_SH}\nemit_7577 \"$$\" \"argv={enc}\"\necho READY\nsleep 60\n"),
    );

    let mut daemon = env.start_clean(&[("HOME", &home)]);
    let (c, _) = env.connect();
    let pane = create(&c, &env.dir);
    run_line(&c, pane, &format!("'{}'", reporter.display()));
    wait_match(&c, pane, "READY");
    wait_records(&c, pane, "bash record", |r| r.len() == 1 && r[0].trusted);
    c.request(Request::Shutdown { kill_panes: true }).unwrap();
    daemon.wait_exit();
    drop(c);

    let _daemon2 = env.start_clean(&[("HOME", &home)]);
    let (c2, _) = env.connect();
    wait_match(&c2, pane, "INTR_RESUMED");
    std::thread::sleep(Duration::from_millis(150));
    c2.request(Request::Input {
        pane,
        data: vec![0x03],
    })
    .unwrap();
    std::thread::sleep(Duration::from_millis(250));
    // Traced, the command shows as `+ printf ...` before its output. Searched in the
    // scrollback (the traced prompt hook after it scrolls it off the screen), and typed again
    // if it came before bash read input again after the interrupt.
    let deadline = Instant::now() + Duration::from_secs(20);
    'typed: loop {
        run_line(&c2, pane, "printf 'TRACED_%s\\n' STILL");
        let retype = Instant::now() + Duration::from_secs(2);
        while Instant::now() < retype {
            if capture(&c2, pane).contains("\nTRACED_STILL") {
                break 'typed;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        assert!(
            Instant::now() < deadline,
            "command never ran: {}",
            capture(&c2, pane)
        );
    }
    let shown = capture(&c2, pane);
    assert!(
        shown.contains("+ printf 'TRACED_%s\\n' STILL"),
        "xtrace lost after Ctrl-C: {shown}"
    );
    assert!(
        !shown.contains("--exec-resume") && shown.matches("INTR_RESUMED").count() == 1,
        "{shown}"
    );
}

/// zsh without its line editor has no first-prompt widget: started with `--no-zle` the daemon
/// types the resume line, and with ZLE turned off by `.zshrc` the integration runs it from its
/// hook. Either way the program is looked up on the PATH `.zshrc`'s precmd hooks make, and runs
/// in the foreground reading the terminal.
#[test]
fn zsh_without_zle_still_resumes() {
    if running_as_root() {
        return;
    }
    let Some(zsh) = find_shell("zsh") else {
        return;
    };
    for (name, args, zshrc) in [
        ("flag", "\"--no-zle\"", ""),
        ("zshrc", "", "unsetopt zle\n"),
    ] {
        let env = Env::new(&format!("resume-nozle-{name}"));
        let home = env.dir.join("home");
        let custom_bin = env.dir.join("custom-bin");
        std::fs::create_dir_all(&home).unwrap();
        write_exec(
            &custom_bin.join("nozleprog"),
            "#!/bin/sh\necho NOZLE_RESUMED\nread line\necho \"GOT_${line}_END\"\nsleep 30\n",
        );
        std::fs::write(
            home.join(".zshrc"),
            format!(
                "_my_env_hook() {{ [[ :$PATH: == *:{0}:* ]] || export PATH=\"{0}:$PATH\"; }}\n\
                 precmd_functions+=(_my_env_hook)\n{zshrc}",
                custom_bin.display()
            ),
        )
        .unwrap();
        let sep = if args.is_empty() { "" } else { ", " };
        write_config(
            &env,
            &format!(
                "[terminal]\nshell = [\"{}\"{sep}{args}]\nshell_integration = true\n\
                 [session]\nsnapshot_interval_secs = 60\nresume_programs = \"all\"\n",
                zsh.display()
            ),
        );
        let reporter = env.dir.join("report.sh");
        let enc = b64_argv(&["nozleprog"]);
        write_exec(
            &reporter,
            &format!(
                "#!/bin/sh\n{EMIT_SH}\nemit_7577 \"$$\" \"argv={enc}\"\necho NOZLE_READY\nsleep 60\n"
            ),
        );

        let mut daemon = env.start_clean(&[("HOME", &home)]);
        let (c, _) = env.connect();
        let pane = create(&c, &env.dir);
        run_line(&c, pane, &format!("'{}'", reporter.display()));
        wait_match(&c, pane, "NOZLE_READY");
        wait_records(&c, pane, "no-zle record", |r| r.len() == 1 && r[0].trusted);
        c.request(Request::Shutdown { kill_panes: true }).unwrap();
        daemon.wait_exit();
        drop(c);

        let _daemon2 = env.start_clean(&[("HOME", &home)]);
        let (c2, _) = env.connect();
        wait_match(&c2, pane, "NOZLE_RESUMED");
        run_line(&c2, pane, "typed");
        wait_match(&c2, pane, "GOT_typed_END");
    }
}

#[test]
fn first_process_resumes_as_first_process() {
    if running_as_root() {
        return;
    }
    let env = Env::new("resume-first-proc");
    write_config(
        &env,
        r#"
        [terminal]
        shell = ["/bin/sh"]
        [session]
        snapshot_interval_secs = 60
        resume_programs = "all"
        "#,
    );
    let pid_file = env.dir.join("resumed-pid");
    let runs = env.dir.join("resumed-runs");
    let snap_at_start = env.dir.join("snapshot-at-start.json");
    let resumed_bin = env.dir.join("bin/resumed");
    // What the snapshot said when the command started: it must already be marked tried.
    write_exec(
        &resumed_bin,
        &format!(
            "#!/bin/sh\ncp '{}' '{}'\necho x >> '{}'\nprintf '%s' \"$$\" > '{}'\necho FIRST_RESUMED\nsleep 30\n",
            env.dir.join("state/session.json").display(),
            snap_at_start.display(),
            runs.display(),
            pid_file.display()
        ),
    );
    let first_bin = env.dir.join("bin/first");
    let enc = b64_argv(&[&resumed_bin.display().to_string()]);
    write_exec(
        &first_bin,
        &format!(
            "#!/bin/sh\n{EMIT_SH}\nemit_7577 \"$$\" \"argv={enc}\"\necho FIRST_REPORTED\nsleep 60\n"
        ),
    );

    let mut daemon = env.start();
    let (c, _) = env.connect();
    let Response::PaneCreated { pane } = c
        .request(Request::CreatePane(CreatePane {
            command: Some(vec![first_bin.display().to_string()]),
            cwd: Some(env.dir.display().to_string()),
            size: PaneSize {
                cols: 80,
                rows: 24,
                ..Default::default()
            },
            ..Default::default()
        }))
        .unwrap()
    else {
        panic!("expected PaneCreated");
    };
    wait_match(&c, pane, "FIRST_REPORTED");
    let recs = wait_records(&c, pane, "first_process record", |r| r.len() == 1);
    assert!(
        recs[0].first_process,
        "expected first_process=true: {recs:?}"
    );

    c.request(Request::Shutdown { kill_panes: true }).unwrap();
    daemon.wait_exit();
    drop(c);

    let daemon2 = env.start();
    let (c2, _) = env.connect();
    wait_match(&c2, pane, "FIRST_RESUMED");
    let Response::PaneInfo(info) = c2.request(Request::PaneInfo { pane }).unwrap() else {
        panic!("expected PaneInfo");
    };
    let resumed_pid: u32 = std::fs::read_to_string(&pid_file)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert_eq!(
        info.pid,
        Some(resumed_pid),
        "resumed program must be the first process on the PTY"
    );
    let at_start: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&snap_at_start).unwrap()).unwrap();
    assert_eq!(
        opened_record(&env, &at_start, 0)["attempted"],
        true,
        "the attempt was not on disk before the command ran"
    );

    // The daemon dies without saving again: the next start does not run the command again.
    drop(c2);
    drop(daemon2);
    let _daemon3 = env.start();
    std::thread::sleep(Duration::from_millis(1500));
    assert_eq!(
        std::fs::read_to_string(&runs).unwrap().lines().count(),
        1,
        "resume ran again after a crash"
    );
}

/// A first-process resume that fails leaves the configured shell in the pane, started with
/// its configured arguments.
#[test]
fn failed_first_process_resume_starts_the_configured_shell_with_its_arguments() {
    if running_as_root() {
        return;
    }
    let env = Env::new("resume-first-fallback");
    // A shell launcher that refuses to start without its argument.
    let launcher = env.dir.join("bin/launcher");
    write_exec(
        &launcher,
        "#!/bin/sh\n\
         [ \"$1\" = --required ] || { echo LAUNCHER_NEEDS_ARG; exit 3; }\n\
         exec /bin/sh\n",
    );
    write_config(
        &env,
        &format!(
            "[terminal]\nshell = [\"{}\", \"--required\"]\n\
             [session]\nsnapshot_interval_secs = 60\nresume_programs = \"all\"\n",
            launcher.display()
        ),
    );
    // The program it reports is gone by the restart.
    let gone = env.dir.join("bin/gone");
    write_exec(&gone, "#!/bin/sh\necho GONE_RAN\n");
    let first_bin = env.dir.join("bin/first");
    let enc = b64_argv(&[&gone.display().to_string()]);
    write_exec(
        &first_bin,
        &format!(
            "#!/bin/sh\n{EMIT_SH}\nemit_7577 \"$$\" \"argv={enc}\"\necho FIRST_REPORTED\nsleep 60\n"
        ),
    );

    let mut daemon = env.start();
    let (c, _) = env.connect();
    let Response::PaneCreated { pane } = c
        .request(Request::CreatePane(CreatePane {
            command: Some(vec![first_bin.display().to_string()]),
            cwd: Some(env.dir.display().to_string()),
            size: PaneSize {
                cols: 80,
                rows: 24,
                ..Default::default()
            },
            ..Default::default()
        }))
        .unwrap()
    else {
        panic!("expected PaneCreated");
    };
    wait_match(&c, pane, "FIRST_REPORTED");
    wait_records(&c, pane, "first_process record", |r| {
        r.len() == 1 && r[0].first_process && r[0].trusted
    });
    c.request(Request::Shutdown { kill_panes: true }).unwrap();
    daemon.wait_exit();
    drop(c);
    std::fs::remove_file(&gone).unwrap();

    let _daemon2 = env.start();
    let (c2, _) = env.connect();
    wait_match(&c2, pane, "cannot resume");
    // Only a shell running the line prints the joined word.
    run_line(&c2, pane, "printf 'FALLBACK_%s\\n' SHELL");
    wait_match(&c2, pane, "FALLBACK_SHELL");
    assert!(
        !capture(&c2, pane).contains("LAUNCHER_NEEDS_ARG"),
        "the fallback shell started without its arguments"
    );
}

#[test]
fn cwd_from_report_requires_directory_while_os_cwd_falls_back() {
    if running_as_root() {
        return;
    }
    let env = Env::new("resume-cwd");
    write_config(
        &env,
        r#"
        [terminal]
        shell = ["/bin/sh"]
        [session]
        snapshot_interval_secs = 60
        resume_programs = "all"
        "#,
    );
    let dir_gone = env.dir.join("dir-gone");
    let dir_keep = env.dir.join("dir-keep");
    let dir_os_gone = env.dir.join("dir-os-gone");
    for d in [&dir_gone, &dir_keep, &dir_os_gone] {
        std::fs::create_dir_all(d).unwrap();
    }
    let hit_gone = env.dir.join("hit-gone");
    let hit_keep = env.dir.join("hit-keep");
    let hit_os = env.dir.join("hit-os");
    let marker_prog = env.dir.join("bin/mark-cwd");
    write_exec(
        &marker_prog,
        "#!/bin/sh\npwd > \"$1\"\necho MARKED \"$1\"\n",
    );

    let make_reporter = |name: &str, out: &Path, explicit_cwd: Option<&Path>| {
        let p = env.dir.join(format!("bin/rep-{name}"));
        let argv = b64_argv(&[
            &marker_prog.display().to_string(),
            &out.display().to_string(),
        ]);
        let cwd_pair = match explicit_cwd {
            Some(d) => format!(":cwd={}", b64(&d.display().to_string())),
            None => String::new(),
        };
        write_exec(
            &p,
            &format!(
                "#!/bin/sh\n{EMIT_SH}\nemit_7577 \"$$\" \"argv={argv}{cwd_pair}\"\necho REP_{name}\nsleep 60\n"
            ),
        );
        p
    };
    let rep_gone = make_reporter("gone", &hit_gone, Some(&dir_gone));
    let rep_keep = make_reporter("keep", &hit_keep, Some(&dir_keep));
    let rep_os = make_reporter("os", &hit_os, None);
    // With both its directories gone, the OS-cwd record starts where a pane without a
    // directory does: the daemon's home.
    let home = env.dir.join("home");
    std::fs::create_dir_all(&home).unwrap();

    let mut daemon = env.start_with(&[("HOME", &home)]);
    let (c, _) = env.connect();
    let p_gone = create(&c, &env.dir);
    let p_keep = create(&c, &env.dir);
    let p_os = create(&c, &dir_os_gone);

    run_line(&c, p_gone, &format!("'{}'", rep_gone.display()));
    run_line(&c, p_keep, &format!("'{}'", rep_keep.display()));
    run_line(&c, p_os, &format!("'{}'", rep_os.display()));
    wait_match(&c, p_gone, "REP_gone");
    wait_match(&c, p_keep, "REP_keep");
    wait_match(&c, p_os, "REP_os");
    wait_records(&c, p_gone, "gone rec", |r| {
        r.len() == 1 && r[0].cwd_from_report
    });
    wait_records(&c, p_keep, "keep rec", |r| {
        r.len() == 1 && r[0].cwd_from_report
    });
    wait_records(&c, p_os, "os rec", |r| {
        r.len() == 1 && !r[0].cwd_from_report
    });

    c.request(Request::Shutdown { kill_panes: true }).unwrap();
    daemon.wait_exit();
    drop(c);

    // Delete both `dir_gone` (from `cwd` key) and `dir_os_gone` (from OS process cwd).
    std::fs::remove_dir_all(&dir_gone).unwrap();
    std::fs::remove_dir_all(&dir_os_gone).unwrap();

    let _daemon2 = env.start_with(&[("HOME", &home)]);
    let (c2, _) = env.connect();
    wait_match(&c2, p_keep, "MARKED");
    wait_match(&c2, p_os, "MARKED");
    run_line(&c2, p_gone, "echo PLAIN_SHELL");
    wait_match(&c2, p_gone, "PLAIN_SHELL");

    assert!(
        !hit_gone.exists(),
        "record with deleted explicit `cwd` key must NOT resume"
    );
    let keep_pwd =
        std::fs::canonicalize(std::fs::read_to_string(&hit_keep).unwrap().trim()).unwrap();
    assert_eq!(keep_pwd, std::fs::canonicalize(&dir_keep).unwrap());
    let os_pwd = std::fs::read_to_string(&hit_os)
        .expect("record whose cwd came from OS (not `cwd` key) should resume in fallback dir");
    assert_eq!(
        std::fs::canonicalize(os_pwd.trim()).unwrap(),
        std::fs::canonicalize(&home).unwrap()
    );
}

#[test]
fn clear_reject_exit_grace_and_close_pane_remove_records() {
    if running_as_root() {
        return;
    }
    let env = Env::new("resume-lifecycle");
    write_config(
        &env,
        r#"
        [terminal]
        shell = ["/bin/sh"]
        [session]
        snapshot_interval_secs = 60
        resume_programs = "all"
        "#,
    );
    let _daemon = env.start();
    let (c, _) = env.connect();
    let pane = create(&c, &env.dir);

    // 1. Report then send a rejected report (valid MAC/time, relative argv[0]) -> removes record.
    let reject_script = env.dir.join("reject.sh");
    let valid_argv = b64_argv(&["/bin/true"]);
    let bad_argv = b64_argv(&["./relative.sh"]);
    write_exec(
        &reject_script,
        &format!(
            "#!/bin/sh\n{EMIT_SH}\n\
             emit_7577 \"$$\" \"argv={valid_argv}\"\n\
             echo STEP1\n\
             sleep 1\n\
             emit_7577 \"$$\" \"argv={bad_argv}\"\n\
             echo STEP2\n\
             sleep 30\n"
        ),
    );
    run_line(&c, pane, &format!("'{}'", reject_script.display()));
    wait_match(&c, pane, "STEP1");
    wait_records(&c, pane, "step1 set", |r| r.len() == 1);
    wait_match(&c, pane, "STEP2");
    wait_records(&c, pane, "step2 rejected removes record", |r| r.is_empty());
    c.request(Request::Input {
        pane,
        data: vec![3],
    })
    .unwrap();

    // 2. Clear removes record immediately.
    let clear_script = env.dir.join("clear.sh");
    write_exec(
        &clear_script,
        &format!(
            "#!/bin/sh\n{EMIT_SH}\n\
             emit_7577 \"$$\" \"argv={valid_argv}\"\n\
             echo CLEAR1\n\
             sleep 1\n\
             emit_7577 \"$$\" \"clear=1\"\n\
             echo CLEAR2\n\
             sleep 30\n"
        ),
    );
    run_line(&c, pane, &format!("'{}'", clear_script.display()));
    wait_match(&c, pane, "CLEAR1");
    wait_records(&c, pane, "clear1 set", |r| r.len() == 1);
    wait_match(&c, pane, "CLEAR2");
    wait_records(&c, pane, "clear2 removed", |r| r.is_empty());
    c.request(Request::Input {
        pane,
        data: vec![3],
    })
    .unwrap();

    // 3. Process exit without `clear` keeps record briefly (<2s grace), then expires it. The
    // record is on disk first (its own argv, unlike the earlier steps'), and the program exits
    // only once the test saw it there.
    let exit_script = env.dir.join("exit.sh");
    let exit_gate = env.dir.join("exit-now");
    let exit_argv = b64_argv(&["/bin/echo", "exit-step"]);
    write_exec(
        &exit_script,
        &format!(
            "#!/bin/sh\n{EMIT_SH}\nemit_7577 \"$$\" \"argv={exit_argv}\"\necho EXIT_SOON\n\
             while [ ! -f '{}' ]; do sleep 0.05; done\n",
            exit_gate.display()
        ),
    );
    run_line(&c, pane, &format!("'{}'", exit_script.display()));
    wait_match(&c, pane, "EXIT_SOON");
    wait_records(&c, pane, "short-lived set", |r| r.len() == 1);
    wait_snapshot(&env, "short-lived record saved", |snap| {
        snap["panes"][0]["id"] == pane && opened_record(&env, snap, 0)["argv"][1] == "exit-step"
    });
    std::fs::write(&exit_gate, "").unwrap();
    wait_records(&c, pane, "expired after exit grace", |r| r.is_empty());
    wait_snapshot(&env, "expired record removed from snapshot", |snap| {
        snap["panes"][0]["id"] == pane && snap["panes"][0]["program_resume"].is_null()
    });

    // 4. Closing a pane discards its record immediately.
    let stay = env.dir.join("stay.sh");
    write_exec(
        &stay,
        &format!(
            "#!/bin/sh\n{EMIT_SH}\nemit_7577 \"$$\" \"argv={valid_argv}\"\necho STAY\nsleep 60\n"
        ),
    );
    run_line(&c, pane, &format!("'{}'", stay.display()));
    wait_match(&c, pane, "STAY");
    wait_records(&c, pane, "stay set", |r| r.len() == 1);
    c.request(Request::ClosePane { pane }).unwrap();
    wait_snapshot(&env, "closed pane's record discarded", |snap| {
        snap["panes"].as_array().is_some_and(|p| p.is_empty())
    });
}

#[test]
fn trust_rules_enforce_allowlist_reload_and_wrapped_check() {
    if running_as_root() {
        return;
    }
    let env = Env::new("resume-trust");
    let sessions = env.dir.join("sessions");
    let work_dir = env.dir.join("work");
    std::fs::create_dir_all(&sessions).unwrap();
    std::fs::create_dir_all(&work_dir).unwrap();
    let session_file = sessions.join("s.txt");
    std::fs::write(&session_file, "ok").unwrap();

    // Default mode is "rules": custom binary `fakeeditor` is NOT trusted yet.
    write_config(
        &env,
        r#"
        [terminal]
        shell = ["/bin/sh"]
        [session]
        snapshot_interval_secs = 60
        "#,
    );
    let fake_editor = env.dir.join("bin/fakeeditor");
    let enc = b64_argv(&[
        &fake_editor.display().to_string(),
        "-S",
        &session_file.display().to_string(),
    ]);
    write_exec(
        &fake_editor,
        &format!(
            "#!/bin/sh\n\
             if [ \"$1\" = \"-S\" ]; then echo EDITOR_RESUMED \"$2\"; exit 0; fi\n\
             {EMIT_SH}\n\
             emit_7577 \"$$\" \"argv={enc}\"\n\
             echo EDITOR_UP\n\
             sleep 60\n"
        ),
    );

    let mut daemon = env.start();
    let (c, _) = env.connect();
    let pane = create(&c, &work_dir);
    run_line(&c, pane, &format!("'{}'", fake_editor.display()));
    wait_match(&c, pane, "EDITOR_UP");

    // In memory as untrusted (`no_rule`), NOT written to session.json.
    let recs = wait_records(&c, pane, "untrusted record", |r| r.len() == 1);
    assert!(!recs[0].trusted);
    assert_eq!(recs[0].untrusted_reason.as_deref(), Some("no_rule"));
    // A save that saw the live record leaves it out.
    c.request(Request::SaveSnapshot).unwrap();
    wait_snapshot(&env, "untrusted record not saved", |snap| {
        snap["panes"][0].is_object() && snap["panes"][0]["program_resume"].is_null()
    });

    // Add a trust rule and reload config -> running pane's record immediately becomes trusted
    // and persisted to session.json!
    write_config(
        &env,
        &format!(
            "[terminal]\nshell = [\"/bin/sh\"]\n\
             [session]\nsnapshot_interval_secs = 60\nresume_programs = \"rules\"\n\
             [[session.resume_rules]]\n\
             name = \"fakeeditor\"\n\
             command = [\"{}\", \"-S\", \"<under:{}>\"]\n\
             dirs = [\"{}\"]\n\
             match_process = false\n",
            fake_editor.display(),
            sessions.display(),
            work_dir.display(),
        ),
    );
    c.request(Request::ReloadConfig).unwrap();
    let recs = wait_records(&c, pane, "trusted after reload", |r| {
        r.len() == 1 && r[0].trusted
    });
    assert_eq!(recs[0].rule.as_deref(), Some("fakeeditor"));
    wait_snapshot(&env, "trusted record saved", |snap| {
        !snap["panes"][0]["program_resume"].is_null()
    });

    // Cold restart resumes it!
    c.request(Request::Shutdown { kill_panes: true }).unwrap();
    daemon.wait_exit();
    drop(c);

    let _daemon2 = env.start();
    let (c2, _) = env.connect();
    wait_match(&c2, pane, "EDITOR_RESUMED");
}

#[test]
fn wrapped_child_under_non_shell_is_not_trusted_even_in_all_mode() {
    if running_as_root() {
        return;
    }
    let Some(python) = find_shell("python3") else {
        return;
    };
    let env = Env::new("resume-wrapped");
    write_config(
        &env,
        r#"
        [terminal]
        shell = ["/bin/sh"]
        [session]
        snapshot_interval_secs = 60
        resume_programs = "all"
        "#,
    );
    let inner = env.dir.join("bin/inner");
    let enc = b64_argv(&["/bin/true"]);
    write_exec(
        &inner,
        &format!(
            "#!/bin/sh\n{EMIT_SH}\nemit_7577 \"$$\" \"argv={enc}\"\necho WRAPPED_SENT\nsleep 60\n"
        ),
    );

    let _daemon = env.start();
    let (c, _) = env.connect();
    let pane = create(&c, &env.dir);
    // Run `inner` under python3 (a non-shell ancestor simulating a sandbox/agent tool).
    run_line(
        &c,
        pane,
        &format!(
            "'{}' -c 'import subprocess; subprocess.run([\"{}\"])'",
            python.display(),
            inner.display()
        ),
    );
    wait_match(&c, pane, "WRAPPED_SENT");
    let recs = wait_records(&c, pane, "wrapped record", |r| r.len() == 1);
    assert!(
        !recs[0].trusted,
        "wrapped child must not be trusted: {recs:?}"
    );
    assert_eq!(recs[0].untrusted_reason.as_deref(), Some("wrapped"));
    // A save that saw the live record leaves it out.
    c.request(Request::SaveSnapshot).unwrap();
    wait_snapshot(&env, "untrusted record not saved", |snap| {
        snap["panes"][0].is_object() && snap["panes"][0]["program_resume"].is_null()
    });
}

#[test]
fn fish_integration_resume_uses_startup_path_and_bypasses_functions() {
    if running_as_root() {
        return;
    }
    let Some(fish_path) = find_shell("fish") else {
        return;
    };
    let env = Env::new("resume-fish");
    let home = env.dir.join("home");
    let custom_bin = env.dir.join("custom-bin");
    let fn_marker = env.dir.join("fn-hijacked");
    std::fs::create_dir_all(home.join(".config/fish")).unwrap();
    std::fs::create_dir_all(&custom_bin).unwrap();

    let prog = custom_bin.join("fishresumeprog");
    // It reads the terminal: fish must hand the resumed job the terminal, in full job control
    // too (where a job an event handler starts gets its own process group, not the terminal).
    write_exec(
        &prog,
        "#!/bin/sh\necho FISH_RESUMED_OK\nread line\necho \"FISH_GOT_${line}_END\"\nsleep 30\n",
    );
    // The program is on PATH only once a prompt handler config.fish registered ran (as
    // direnv's does): the resume must come after it.
    std::fs::write(
        home.join(".config/fish/config.fish"),
        format!(
            "status job-control full\n\
             function __my_env --on-event fish_prompt; fish_add_path -gp '{}'; end\n\
             function fishresumeprog; touch '{}'; end\n",
            custom_bin.display(),
            fn_marker.display(),
        ),
    )
    .unwrap();
    write_config(
        &env,
        &format!(
            "[terminal]\nshell = [\"{}\"]\nshell_integration = true\n\
             [session]\nsnapshot_interval_secs = 60\nresume_programs = \"all\"\n",
            fish_path.display()
        ),
    );

    let reporter = env.dir.join("report.sh");
    let enc = b64_argv(&["fishresumeprog"]);
    write_exec(
        &reporter,
        &format!(
            "#!/bin/sh\n{EMIT_SH}\nemit_7577 \"$$\" \"argv={enc}\"\necho FISH_READY\nsleep 60\n"
        ),
    );

    let mut daemon = env.start_clean(&[("HOME", &home)]);
    let (c, _) = env.connect();
    let pane = create(&c, &env.dir);
    run_line(&c, pane, &format!("'{}'", reporter.display()));
    wait_match(&c, pane, "FISH_READY");
    wait_records(&c, pane, "fish record", |r| r.len() == 1 && r[0].trusted);

    c.request(Request::Shutdown { kill_panes: true }).unwrap();
    daemon.wait_exit();
    drop(c);

    let _daemon2 = env.start_clean(&[("HOME", &home)]);
    let (c2, _) = env.connect();
    wait_match(&c2, pane, "FISH_RESUMED_OK");
    assert!(
        !fn_marker.exists(),
        "fish function ran instead of PATH lookup"
    );
    // The program's output is not a prompt fish redraws: resizing keeps it.
    for cols in [60, 100] {
        c2.request(Request::Resize {
            pane,
            size: PaneSize {
                cols,
                rows: 24,
                cell_width: 8,
                cell_height: 16,
            },
        })
        .unwrap();
        std::thread::sleep(Duration::from_millis(300));
        let shown = capture(&c2, pane);
        assert!(
            shown.contains("FISH_RESUMED_OK"),
            "after resize to {cols}: {shown}"
        );
    }
    run_line(&c2, pane, "typed");
    wait_match(&c2, pane, "FISH_GOT_typed_END");
}

/// A resumed job ended by Ctrl-C (which cancels the rest of fish's evaluation it runs in)
/// leaves fish with the job-control mode config.fish chose, not the one it runs the job in.
#[test]
fn fish_resume_interrupted_keeps_the_configured_job_control() {
    if running_as_root() {
        return;
    }
    let Some(fish_path) = find_shell("fish") else {
        return;
    };
    let env = Env::new("resume-fish-int");
    let home = env.dir.join("home");
    let custom_bin = env.dir.join("custom-bin");
    std::fs::create_dir_all(home.join(".config/fish")).unwrap();
    write_exec(
        &custom_bin.join("fishintprog"),
        "#!/bin/sh\necho FISH_INT_RUNNING\nsleep 30\n",
    );
    std::fs::write(
        home.join(".config/fish/config.fish"),
        format!(
            "status job-control none\nfish_add_path -gp '{}'\n",
            custom_bin.display()
        ),
    )
    .unwrap();
    write_config(
        &env,
        &format!(
            "[terminal]\nshell = [\"{}\"]\nshell_integration = true\n\
             [session]\nsnapshot_interval_secs = 60\nresume_programs = \"all\"\n",
            fish_path.display()
        ),
    );
    let reporter = env.dir.join("report.sh");
    let enc = b64_argv(&["fishintprog"]);
    write_exec(
        &reporter,
        &format!(
            "#!/bin/sh\n{EMIT_SH}\nemit_7577 \"$$\" \"argv={enc}\"\necho FISH_INT_READY\nsleep 60\n"
        ),
    );

    let mut daemon = env.start_clean(&[("HOME", &home)]);
    let (c, _) = env.connect();
    let pane = create(&c, &env.dir);
    run_line(&c, pane, &format!("'{}'", reporter.display()));
    wait_match(&c, pane, "FISH_INT_READY");
    wait_records(&c, pane, "fish record", |r| r.len() == 1 && r[0].trusted);
    c.request(Request::Shutdown { kill_panes: true }).unwrap();
    daemon.wait_exit();
    drop(c);

    let _daemon2 = env.start_clean(&[("HOME", &home)]);
    let (c2, _) = env.connect();
    wait_match(&c2, pane, "FISH_INT_RUNNING");
    std::thread::sleep(Duration::from_millis(150));
    c2.request(Request::Input {
        pane,
        data: vec![0x03],
    })
    .unwrap();
    std::thread::sleep(Duration::from_millis(250));
    // Only a shell running it prints the joined word (an echo of the typed line would not).
    run_line(
        &c2,
        pane,
        "if status is-no-job-control; printf 'JC_%s\\n' KEPT; else; printf 'JC_%s\\n' CHANGED; end",
    );
    wait_match(&c2, pane, "JC_(KEPT|CHANGED)");
    let shown = capture(&c2, pane);
    assert!(shown.contains("JC_KEPT"), "{shown}");
}

#[test]
fn records_and_token_survive_in_place_upgrade() {
    if running_as_root() {
        return;
    }
    let env = Env::new("resume-upgrade");
    write_config(
        &env,
        r#"
        [terminal]
        shell = ["/bin/sh"]
        [session]
        snapshot_interval_secs = 60
        resume_programs = "all"
        "#,
    );
    let daemon = env.start();
    let (c, _) = env.connect();
    let pane = create(&c, &env.dir);

    let trigger = env.dir.join("send-clear");
    let prog = env.dir.join("bin/upgrade-prog");
    let argv1 = b64_argv(&["/bin/true"]);
    write_exec(
        &prog,
        &format!(
            "#!/bin/sh\n{EMIT_SH}\n\
             emit_7577 \"$$\" \"argv={argv1}\"\n\
             echo UPGRADE_SET\n\
             while [ ! -f '{}' ]; do sleep 0.05; done\n\
             emit_7577 \"$$\" \"clear=1\"\n\
             echo UPGRADE_CLEARED\n\
             sleep 30\n",
            trigger.display()
        ),
    );
    run_line(&c, pane, &format!("'{}'", prog.display()));
    wait_match(&c, pane, "UPGRADE_SET");
    wait_records(&c, pane, "set before upgrade", |r| r.len() == 1);

    let c = super::workflows_upgrade::upgrade(&env, &daemon, &c);
    // Record survived the in-place upgrade.
    let recs = wait_records(&c, pane, "kept after upgrade", |r| r.len() == 1);
    assert_eq!(recs[0].argv, vec!["/bin/true"]);

    // Token survived too: the running process can send `clear=1` signed with the same token!
    std::fs::write(&trigger, "go").unwrap();
    wait_match(&c, pane, "UPGRADE_CLEARED");
    wait_records(&c, pane, "cleared after upgrade", |r| r.is_empty());
}
