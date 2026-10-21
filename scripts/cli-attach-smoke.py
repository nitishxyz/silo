#!/usr/bin/env python3
"""Drive `silo attach` through a real PTY: type, detach (Ctrl-\\), reattach,
check the screen was restored and the shell is the same."""
import json, os, pty, select, signal, sys, time, subprocess, shutil

SILO = os.path.abspath(sys.argv[1] if len(sys.argv) > 1 else "target/debug/silo")
HOME = "/tmp/silo-attach-smoke"
shutil.rmtree(HOME, ignore_errors=True)
env = dict(os.environ, SILO_HOME=HOME)


def run_pty(args, steps):
    pid, fd = pty.fork()
    if pid == 0:
        os.execve(SILO, [SILO] + args, env)
    out = b""

    def pump(seconds):
        nonlocal out
        end = time.time() + seconds
        while time.time() < end:
            r, _, _ = select.select([fd], [], [], 0.05)
            if r:
                try:
                    data = os.read(fd, 65536)
                except OSError:
                    return
                if not data:
                    return
                out += data

    pump(1.0)
    for data, wait in steps:
        os.write(fd, data)
        pump(wait)
    pump(0.5)
    _, status = os.waitpid(pid, 0)
    return out.decode(errors="replace"), status


def stop_server():
    """SIGTERM this home's server (`silo kill-server` is an owner action that
    would ask for approval) and wait for it and its pty-hosts to exit."""
    try:
        with open(os.path.join(HOME, "runtime.json")) as f:
            pid = int(json.load(f)["pid"])
    except (OSError, ValueError, KeyError):
        return
    children = subprocess.run(["pgrep", "-P", str(pid)], capture_output=True, text=True).stdout.split()
    hosts = [int(c) for c in children
             if "pty-host" in subprocess.run(["ps", "-o", "command=", "-p", c], capture_output=True, text=True).stdout]
    try:
        os.kill(pid, signal.SIGTERM)
    except ProcessLookupError:
        return
    for p in [pid] + hosts:
        for _ in range(250):
            try:
                os.kill(p, 0)
            except ProcessLookupError:
                break
            time.sleep(0.02)
        else:
            print(f"cli attach smoke: pid {p} still running", file=sys.stderr)


try:
    out1, st1 = run_pty(["new", "-s", "smoke", "--", "/bin/sh"], [
        (b"echo first-$((6*7)); echo $$ > " + HOME.encode() + b"/pid\r", 1.0),
        (b"\x1c", 0.5),
    ])
    assert "first-42" in out1, out1
    assert "[detached]" in out1, out1
    ls = subprocess.run([SILO, "ls"], env=env, capture_output=True, text=True).stdout
    assert "smoke" in ls, ls
    out2, st2 = run_pty(["attach", "smoke"], [
        (b"echo second-$$\r", 1.0),
        (b"exit 3\r", 1.5),
    ])
    shell_pid = open(HOME + "/pid").read().strip()
    assert "first-42" in out2, "screen restored on reattach:\n" + out2
    assert f"second-{shell_pid}" in out2, out2
    assert "[exited 3]" in out2, out2
    print("cli attach smoke: ok")
finally:
    stop_server()
