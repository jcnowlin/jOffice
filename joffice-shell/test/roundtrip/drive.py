#!/usr/bin/env python3
"""Drive primitives for jOffice cycle-2 test drives on jNix.

Stateless CLI wrapping the labwc rig (wlrctl/wtype/grim) and the
runtime's drive.sh. State lives in the running app and its log.

  drive.py launch <file> <logname>     kill old, launch, return
  drive.py launchbare <logname>        launch with no file (launcher)
  drive.py waitlog <logname> <needle> <timeout_s>
  drive.py key <combo>                 e.g. ctrl+s, ctrl+shift+s, Return
  drive.py type <text>
  drive.py click <x> <y>
  drive.py shot <png>
  drive.py toplevels
  drive.py close                       wlrctl toplevel close
  drive.py kill
  drive.py env                         print resolved tool paths
"""
import glob
import os
import subprocess
import sys
import time

RT = os.path.expanduser("~/OpenCloud/Development/joffice-runtime")
LOGS = os.path.join(RT, "logs")


def tool(name):
    hits = sorted(glob.glob(f"/nix/store/*/bin/{name}"))
    return hits[0] if hits else name


def env():
    e = dict(os.environ)
    uid = os.getuid()
    e["XDG_RUNTIME_DIR"] = f"/run/user/{uid}"
    e["WAYLAND_DISPLAY"] = "wayland-0"
    e["DBUS_SESSION_BUS_ADDRESS"] = f"unix:path=/run/user/{uid}/bus"
    return e


def run(cmd, **kw):
    return subprocess.run(cmd, env=env(), capture_output=True, text=True, **kw)


def logpath(name):
    return os.path.join(LOGS, name)


def launch(args, logname, extra_env=None):
    run(["pkill", "-x", "joffice-shell"])
    time.sleep(1.0)
    os.makedirs(LOGS, exist_ok=True)
    e = env()
    if extra_env:
        e.update(extra_env)
    log = open(logpath(logname), "wb")
    subprocess.Popen(
        [os.path.join(RT, "drive.sh")] + args,
        env=e,
        stdout=log,
        stderr=subprocess.STDOUT,
        start_new_session=True,
    )
    print(f"launched {args} -> logs/{logname}")


def waitlog(logname, needle, timeout_s):
    path = logpath(logname)
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
    run([w, "pointer", "move", str(x), str(y)])
    run([w, "pointer", "click", "left"])
    print(f"click {x},{y}")


def shot(png):
    w = tool("wlrctl")
    run([w, "pointer", "move", "3", "3"])  # DPMS jiggle
    time.sleep(0.4)
    r = run([tool("grim"), png])
    print("shot", png, r.returncode)


def toplevels():
    r = run([tool("wlrctl"), "toplevel", "list"])
    print(r.stdout)
    return r.stdout


def close():
    out = toplevels()
    for line in out.splitlines():
        if "jOffice" in line or "joffice" in line:
            app = line.split()[0].rstrip(":")
            r = run([tool("wlrctl"), "toplevel", "close", app])
            print("closed", app, r.returncode, r.stderr.strip())
            return 0
    print("no joffice toplevel found")
    return 1


def main():
    cmd = sys.argv[1]
    if cmd == "launch":
        extra = {}
        launch(["--file", sys.argv[2]], sys.argv[3], extra)
    elif cmd == "launchbare":
        launch([], sys.argv[2])
    elif cmd == "launchsnap":
        # snapshot mode: env JOFFICE_SNAPSHOT=<png> AFTER=<secs>
        extra = {
            "JOFFICE_SNAPSHOT": sys.argv[3],
            "JOFFICE_SNAPSHOT_AFTER": sys.argv[4] if len(sys.argv) > 4 else "25",
        }
        launch(["--file", sys.argv[2]], sys.argv[5] if len(sys.argv) > 5 else "snap.log", extra)
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
    elif cmd == "close":
        sys.exit(close())
    elif cmd == "kill":
        run(["pkill", "-x", "joffice-shell"])
        print("killed")
    elif cmd == "env":
        for t in ["grim", "wtype", "wlrctl"]:
            print(t, tool(t))
    else:
        print(f"unknown {cmd}")
        sys.exit(2)


if __name__ == "__main__":
    main()
