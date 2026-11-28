#!/usr/bin/env python3
"""Drive primitives for jOffice cycle-9 drives on jNix.

Like drive.py (cycle 2) but for the packaging battery:
- attaches to the private headless labwc on wayland-1
  with a private session bus (never James's wayland-0
  session, never his lock screen);
- launches a CANDIDATE store path directly (never the
  clone's result link, never the staged runtime);
- scratch XDG tree per drive root, so James's real
  config/state are never touched;
- tracks the exact PID it launched; `kill` touches only
  that PID (the cycle-8 drive agent's windows are not
  ours to close).

State: /tmp/c9drive/state.json (pid, log path).
Logs:  ~/OpenCloud/Development/joffice-runtime/logs/c9-*.log
"""
import glob
import json
import os
import subprocess
import sys
import time

RT = os.path.expanduser("~/OpenCloud/Development/joffice-runtime")
LOGS = os.path.join(RT, "logs")
STATE_DIR = "/tmp/c9drive"
STATE = os.path.join(STATE_DIR, "state.json")
XDG = "/tmp/c9x"


def tool(name):
    hits = sorted(glob.glob(f"/nix/store/*/bin/{name}"))
    return hits[0] if hits else name


RUNTIME_DIR = "/tmp/c9rt"


def env(extra=None):
    e = dict(os.environ)
    # A fully private runtime dir: our own headless labwc
    # and session bus live here, so nothing in this rig
    # can see or touch James's wayland-0 session (or its
    # lock screen). The display NAME is wayland-0 inside
    # this dir — names are per-runtime-dir.
    e["XDG_RUNTIME_DIR"] = RUNTIME_DIR
    e["WAYLAND_DISPLAY"] = "wayland-0"
    e["DBUS_SESSION_BUS_ADDRESS"] = f"unix:path={RUNTIME_DIR}/bus"
    e["XDG_CONFIG_HOME"] = os.path.join(XDG, "config")
    e["XDG_STATE_HOME"] = os.path.join(XDG, "state")
    e["XDG_DATA_HOME"] = os.path.join(XDG, "data")
    e["XDG_CACHE_HOME"] = os.path.join(XDG, "cache")
    if extra:
        e.update(extra)
    return e


def rigup():
    """Start the private bus + headless labwc if absent."""
    os.makedirs(RUNTIME_DIR, exist_ok=True)
    os.chmod(RUNTIME_DIR, 0o700)
    e = env()
    if not os.path.exists(os.path.join(RUNTIME_DIR, "bus")):
        subprocess.Popen(
            ["dbus-daemon", "--session",
             f"--address=unix:path={RUNTIME_DIR}/bus", "--fork"],
            env=e)
        print("bus started")
    if not os.path.exists(os.path.join(RUNTIME_DIR, "wayland-0")):
        le = dict(e)
        le["WLR_BACKENDS"] = "headless"
        le["WLR_HEADLESS_OUTPUTS"] = "1"
        log = open(os.path.join(STATE_DIR, "labwc.log"), "wb") \
            if os.path.isdir(STATE_DIR) else open("/tmp/c9-labwc.log", "wb")
        subprocess.Popen([tool("labwc")], env=le, stdout=log,
                         stderr=subprocess.STDOUT, start_new_session=True)
        for _ in range(40):
            if os.path.exists(os.path.join(RUNTIME_DIR, "wayland-0")):
                break
            time.sleep(0.25)
        print("labwc started")
    print("rig up")


def run(cmd, **kw):
    return subprocess.run(cmd, env=env(), capture_output=True, text=True, **kw)


def save_state(pid, logname, binpath):
    os.makedirs(STATE_DIR, exist_ok=True)
    with open(STATE, "w") as f:
        json.dump({"pid": pid, "log": logname, "bin": binpath}, f)


def load_state():
    with open(STATE) as f:
        return json.load(f)


def launch(binpath, args, logname, extra_env=None):
    os.makedirs(LOGS, exist_ok=True)
    for sub in ("config", "state", "data", "cache"):
        os.makedirs(os.path.join(XDG, sub), exist_ok=True)
    log = open(os.path.join(LOGS, logname), "wb")
    p = subprocess.Popen(
        [binpath] + args,
        env=env(extra_env),
        stdout=log,
        stderr=subprocess.STDOUT,
        start_new_session=True,
    )
    save_state(p.pid, logname, binpath)
    print(f"launched pid={p.pid} {binpath} {args} -> logs/{logname}")
    return p.pid


def waitlog(logname, needle, timeout_s):
    path = os.path.join(LOGS, logname)
    deadline = time.time() + timeout_s
    while time.time() < deadline:
        if os.path.exists(path):
            with open(path, errors="replace") as f:
                if needle in f.read():
                    print(f"FOUND {needle!r} in {logname}")
                    return 0
        time.sleep(0.5)
    print(f"TIMEOUT waiting for {needle!r} in {logname}")
    return 1


def key(combo):
    parts = combo.split("+")
    mods, k = parts[:-1], parts[-1]
    cmd = [tool("wtype")]
    for m in mods:
        cmd += ["-M", m]
    cmd += ["-k", k]
    for m in reversed(mods):
        cmd += ["-m", m]
    run(cmd)
    print(f"key {combo}")


def typetext(text):
    run([tool("wtype"), text])
    print(f"typed {len(text)} chars")


def click(x, y):
    w = tool("wlrctl")
    run([w, "pointer", "move", "-10000", "-10000"])
    time.sleep(0.4)
    run([w, "pointer", "move", str(x), str(y)])
    time.sleep(0.4)
    run([w, "pointer", "click", "left"])
    print(f"click {x},{y}")


def shot(png):
    w = tool("wlrctl")
    run([w, "pointer", "move", "3", "3"])
    time.sleep(0.4)
    r = run([tool("grim"), png])
    print("shot", png, r.returncode)


def toplevels():
    r = run([tool("wlrctl"), "toplevel", "list"])
    print(r.stdout)
    return r.stdout


def kill_ours():
    try:
        st = load_state()
    except Exception:
        print("no state; nothing to kill")
        return 1
    pid = st["pid"]
    r = subprocess.run(["kill", str(pid)], capture_output=True, text=True)
    print(f"killed our pid {pid}: rc={r.returncode}")
    return 0


def main():
    cmd = sys.argv[1]
    if cmd == "rigup":
        os.makedirs(STATE_DIR, exist_ok=True)
        rigup()
    elif cmd == "launch":
        # launch <bin> <logname> [file]
        args = ["--file", sys.argv[4]] if len(sys.argv) > 4 else []
        launch(sys.argv[2], args, sys.argv[3])
    elif cmd == "launchpdf":
        # launchpdf <bin> <logname> <file>: annotate tier on
        launch(sys.argv[2], ["--file", sys.argv[4]], sys.argv[3],
               {"JOFFICE_PDF_ANNOTATE": "experimental"})
    elif cmd == "waitlog":
        sys.exit(waitlog(sys.argv[2], sys.argv[3], float(sys.argv[4])))
    elif cmd == "key":
        key(sys.argv[2])
    elif cmd == "type":
        typetext(sys.argv[2])
    elif cmd == "click":
        click(int(sys.argv[2]), int(sys.argv[3]))
    elif cmd == "shot":
        shot(sys.argv[2])
    elif cmd == "toplevels":
        toplevels()
    elif cmd == "kill":
        sys.exit(kill_ours())
    elif cmd == "env":
        for t in ["grim", "wtype", "wlrctl"]:
            print(t, tool(t))
    else:
        print(f"unknown {cmd}")
        sys.exit(2)


if __name__ == "__main__":
    main()
