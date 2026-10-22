use std::process::Command;

// Package builds run `thurm completion <shell>` in a sandbox: it must print the script without
// a daemon, and never start one.
#[test]
fn prints_scripts_without_a_daemon() {
    let dir = std::env::temp_dir().join(format!("thcomp-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let socket = dir.join("s");
    for (shell, marker) in [
        ("fish", "complete -c thurm"),
        ("zsh", "#compdef thurm"),
        ("bash", "complete -F _thurm"),
        ("elvish", "arg-completer[thurm]"),
        ("powershell", "-CommandName 'thurm'"),
    ] {
        let out = Command::new(env!("CARGO_BIN_EXE_thurm"))
            .env("THURM_SOCKET", &socket)
            .args(["completion", shell])
            .output()
            .unwrap();
        assert!(out.status.success(), "{shell}: {out:?}");
        let script = String::from_utf8(out.stdout).unwrap();
        assert!(script.contains(marker), "{shell} script lacks {marker:?}");
        assert!(
            script.contains("completion"),
            "{shell} script lacks the subcommand itself"
        );
    }
    assert!(!socket.exists(), "a daemon was started");
    let _ = std::fs::remove_dir_all(&dir);
}
