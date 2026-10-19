#!/usr/bin/env python3
"""Measure Thurm's startup on an isolated daemon and app (macOS).

    scripts/startup-bench.py [--runs 10] [--tabs 6] [--shell minimal|login]

Two numbers, each the median (and min/max) over --runs:

  cold    app launched with no daemon and no saved session, until its window is on
          screen and the shell in its first pane is at a prompt
  reopen  app and daemon stopped (the session saved) with --tabs tabs open, then the app
          launched again, until its window is on screen and every restored pane is at a prompt

plus `reattach`: the app relaunched while the daemon keeps running (window on screen; the
shells never stopped).

"At a prompt" is the daemon's `at_prompt` (OSC 133 from shell integration), polled with
`thurm --json list` (not `thurm wait --prompt`, which settles for 300 ms first): a time is
late by at most one poll, reported as `poll_ms`. "On screen" is the app's first window in
CGWindowList. Neither is the first frame with the prompt drawn: that would need
instrumenting the app.

Build first (release, with /usr/bin first on PATH):
    cargo build --release -p thurm-ffi -p thurm-daemon -p thurm-cli --target aarch64-apple-darwin
    cp crates/thurm-ffi/include/thurm.h macos/Sources/CThurm/include/
    (cd macos && THURM_LIB_DIR=$PWD/../target/aarch64-apple-darwin/release \
        THURM_SDK_VERSION=$(xcrun --sdk macosx --show-sdk-version) swift build -c release --arch arm64)

Everything runs under a fresh /tmp directory (socket, config, state, remotes) with the app's
own daemon binary, so the installed Thurm and its daemon are never touched. `--shell minimal`
(the default) runs zsh with an empty ZDOTDIR; `login` runs your login shell and its config.
Nothing is clicked or typed: tabs are opened with `thurm new-tab`.
"""

import argparse
import glob
import json
import os
import shutil
import signal
import statistics
import subprocess
import sys
import tempfile
import time

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))

WINDOW_PROBE = r"""
import CoreGraphics
import Foundation
// Prints the wall-clock time the first normal window of the pid is on screen, and its id.
let pid = Int32(CommandLine.arguments[1])!
let deadline = Date().addingTimeInterval(Double(CommandLine.arguments[2])!)
while Date() < deadline {
    let list = CGWindowListCopyWindowInfo([.optionOnScreenOnly], kCGNullWindowID) as? [[String: Any]] ?? []
    for w in list where (w[kCGWindowOwnerPID as String] as? Int32) == pid
        && (w[kCGWindowLayer as String] as? Int) == 0 {
        let b = w[kCGWindowBounds as String] as? [String: Double] ?? [:]
        if (b["Height"] ?? 0) > 100 {
            print(Date().timeIntervalSince1970, w[kCGWindowNumber as String] as? Int ?? 0)
            exit(0)
        }
    }
    usleep(500)
}
exit(1)
"""


def die(msg):
    print(f"startup-bench: {msg}", file=sys.stderr)
    sys.exit(1)


class Bench:
    def __init__(self, args):
        self.args = args
        rust = os.path.join(ROOT, "target/aarch64-apple-darwin/release")
        self.thurmd = os.path.join(rust, "thurmd")
        thurm = os.path.join(rust, "thurm")
        apps = glob.glob(os.path.join(ROOT, "macos/.build/out/Products/Release/Thurm"))
        self.app = args.app or (apps[0] if apps else "")
        for f in (self.thurmd, thurm, self.app):
            if not os.access(f, os.X_OK):
                die(f"missing {f or 'the Swift app'}; build first (see --help)")
        sparkle = glob.glob(
            os.path.join(ROOT, "macos/.build/artifacts/**/Sparkle.xcframework/macos-*"),
            recursive=True,
        )
        # Short: a Unix socket path has to fit in 104 bytes.
        self.dir = tempfile.mkdtemp(prefix="tb", dir="/tmp")
        self.app_proc = None
        self.polls = []
        try:
            self.setup(args, thurm, sparkle)
        except BaseException:
            shutil.rmtree(self.dir, ignore_errors=True)
            raise

    def setup(self, args, thurm, sparkle):
        self.socket = os.path.join(self.dir, "s")
        self.state = os.path.join(self.dir, "state")
        config = os.path.join(self.dir, "config")
        remote = os.path.join(self.dir, "remote")
        bin_dir = os.path.join(self.dir, "bin")
        zdot = os.path.join(self.dir, "zdot")
        for d in (self.state, config, remote, bin_dir, zdot):
            os.makedirs(d)
        # The CLI opens the installed app (`open -b`) when no window takes a new tab:
        # never let it.
        with open(os.path.join(bin_dir, "open"), "w") as f:
            f.write("#!/bin/sh\nexit 1\n")
        os.chmod(os.path.join(bin_dir, "open"), 0o755)
        # Away from thurmd, so that it never starts a daemon of its own: only the app does.
        self.thurm = os.path.join(bin_dir, "thurm")
        shutil.copy(thurm, self.thurm)
        with open(os.path.join(config, "config.toml"), "w") as f:
            if args.shell == "minimal":
                f.write('[terminal]\nshell = ["/bin/zsh", "-l"]\n')
        env = {k: v for k, v in os.environ.items() if not k.startswith("THURM_")}
        env.update(
            THURM_SOCKET=self.socket,
            THURM_CONFIG_DIR=config,
            THURM_STATE_DIR=self.state,
            THURM_REMOTE_DIR=remote,
            THURM_DAEMON=self.thurmd,
            PATH=f"{bin_dir}:/usr/bin:/bin:/usr/sbin:/sbin",
        )
        if args.shell == "minimal":
            # Shell integration hands the shell this ZDOTDIR back: no user zshrc.
            env["ZDOTDIR"] = zdot
        if sparkle:
            env["DYLD_FRAMEWORK_PATH"] = sparkle[0]
        self.env = env
        self.probe = os.path.join(self.dir, "window-probe")
        src = self.probe + ".swift"
        with open(src, "w") as f:
            f.write(WINDOW_PROBE)
        subprocess.run(["/usr/bin/swiftc", "-O", "-o", self.probe, src], check=True)

    def cli(self, *argv, check=True):
        out = subprocess.run(
            [self.thurm, *argv],
            env=self.env,
            capture_output=True,
            text=True,
            check=False,
        )
        if check and out.returncode != 0:
            die(f"thurm {' '.join(argv)}: {out.stderr.strip()}")
        return out

    def daemon_running(self):
        return self.cli("daemon", "status", check=False).returncode == 0

    def stop_app(self):
        if self.app_proc and self.app_proc.poll() is None:
            self.app_proc.send_signal(signal.SIGTERM)
            try:
                self.app_proc.wait(5)
            except subprocess.TimeoutExpired:
                self.app_proc.kill()
                self.app_proc.wait()
        self.app_proc = None

    def stop_daemon(self):
        if self.daemon_running():
            self.cli("daemon", "stop", check=False)
        deadline = time.time() + 5
        while self.daemon_running():
            if time.time() > deadline:
                die("the test daemon did not stop")
            time.sleep(0.05)

    def launch(self, prompts):
        """Launch the app; seconds until its window is on screen, until `prompts` panes are at
        a prompt (0: don't wait for any), and until both."""
        t0 = time.time()
        self.app_proc = subprocess.Popen(
            [self.app],
            env=self.env,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
        probe = subprocess.Popen(
            [self.probe, str(self.app_proc.pid), "20"],
            stdout=subprocess.PIPE,
            text=True,
        )
        prompt_at = t0
        if prompts:
            deadline = t0 + 20
            ready = 0
            while ready < prompts:
                if time.time() > deadline:
                    die(f"only {ready} of {prompts} panes reached a prompt")
                if os.path.exists(self.socket):
                    before = time.time()
                    listed = self.cli("--json", "list", check=False)
                    prompt_at = time.time()
                    self.polls.append(prompt_at - before)
                    if listed.returncode == 0:
                        ready = sum(
                            1
                            for p in json.loads(listed.stdout)
                            if p["alive"] and p["at_prompt"]
                        )
                time.sleep(0.001)
        out, _ = probe.communicate()
        if probe.returncode != 0:
            die("the app's window never appeared")
        window_at, self.window_id = out.split()
        window_at = float(window_at)
        return window_at - t0, prompt_at - t0, max(window_at, prompt_at) - t0

    def open_tabs(self, tabs):
        for _ in range(tabs - len(json.loads(self.cli("--json", "list").stdout))):
            self.cli("new-tab")
        deadline = time.time() + 10
        while True:
            panes = json.loads(self.cli("--json", "list").stdout)
            if len(panes) >= tabs and all(p["at_prompt"] for p in panes):
                break
            if time.time() > deadline:
                die("the new tabs never reached a prompt")
            time.sleep(0.05)
        # The session is saved on stop, but save now so the layout surely has every tab.
        time.sleep(0.5)
        self.cli("save")

    def run(self):
        runs, tabs = self.args.runs, self.args.tabs
        cold, reopen, reattach = [], [], []
        for i in range(runs + 1):
            self.stop_app()
            self.stop_daemon()
            shutil.rmtree(self.state)
            os.makedirs(self.state)
            sample = self.launch(1)
            if i == 0 and self.args.screenshot:
                time.sleep(1)
                subprocess.run(
                    [
                        "screencapture",
                        "-x",
                        "-o",
                        "-l",
                        self.window_id,
                        self.args.screenshot,
                    ],
                    check=False,
                )
            self.open_tabs(tabs)
            self.stop_app()
            reattach_sample = self.launch(0)
            self.stop_app()
            self.stop_daemon()
            reopen_sample = self.launch(tabs)
            # The first round warms the file cache and macOS's launch services.
            if i:
                cold.append(sample)
                reattach.append(reattach_sample)
                reopen.append(reopen_sample)
                print(
                    f"run {i}: cold {sample[2] * 1e3:.0f} ms, reattach "
                    f"{reattach_sample[0] * 1e3:.0f} ms, reopen {reopen_sample[2] * 1e3:.0f} ms",
                    file=sys.stderr,
                )
        report = {
            "shell": self.args.shell,
            "tabs": tabs,
            "runs": runs,
            "cold": summary(cold),
            "reattach": summary(reattach)["window"],
            "reopen": summary(reopen),
            "poll_ms": round(statistics.median(self.polls) * 1e3, 1),
        }
        print(json.dumps(report, indent=2))

    def cleanup(self):
        self.stop_app()
        self.stop_daemon()
        shutil.rmtree(self.dir, ignore_errors=True)


def summary(samples):
    def ms(values):
        return {
            "median_ms": round(statistics.median(values) * 1e3, 1),
            "min_ms": round(min(values) * 1e3, 1),
            "max_ms": round(max(values) * 1e3, 1),
        }

    return {
        "window": ms([s[0] for s in samples]),
        "prompt": ms([s[1] for s in samples]),
        "both": ms([s[2] for s in samples]),
    }


def positive(value):
    n = int(value)
    if n < 1:
        raise argparse.ArgumentTypeError(f"{value} is not a positive number")
    return n


def main():
    parser = argparse.ArgumentParser(
        description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter
    )
    parser.add_argument("--runs", type=positive, default=10)
    parser.add_argument("--tabs", type=positive, default=6)
    parser.add_argument("--shell", choices=["minimal", "login"], default="minimal")
    parser.add_argument(
        "--app", help="the Swift app binary (default: macos/.build release)"
    )
    parser.add_argument("--screenshot", help="save the first cold window here (PNG)")
    args = parser.parse_args()
    if sys.platform != "darwin":
        die("macOS only")
    bench = Bench(args)
    try:
        bench.run()
    finally:
        bench.cleanup()


if __name__ == "__main__":
    main()
