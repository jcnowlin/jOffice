#!/usr/bin/env python3
"""Measurement drives for cycle 2 (Kagi round 1 §5), on jNix.

For each measurement document: launch with the jank probes on,
sample PSS (smaps_rollup) of the shell + WebKit processes every
100 ms during open, wait for the handshake, record the log's
MEASURE lines, then close. Output: one JSON object per document
on stdout + measure-results.json in the results dir.

Usage: measure.py <docs...> -- <results_dir>
Process matching is by /proc cmdline scan (no pkill -f games).
"""
import glob
import json
import os
import subprocess
import sys
import time

RT = os.path.expanduser("~/OpenCloud/Development/joffice-runtime")
LOGS = os.path.join(RT, "logs")


def procs():
    out = {}
    for pid in filter(str.isdigit, os.listdir("/proc")):
        try:
            with open(f"/proc/{pid}/cmdline", "rb") as f:
                cmd = f.read().replace(b"\0", b" ").decode(errors="replace")
            if "joffice-shell" in cmd or "WebKitWebProcess" in cmd or "WebKitNetworkProcess" in cmd:
                out[int(pid)] = cmd.strip()[:60]
        except OSError:
            pass
    return out


def pss_kb(pid):
    try:
        with open(f"/proc/{pid}/smaps_rollup") as f:
            for line in f:
                if line.startswith("Pss:"):
                    return int(line.split()[1])
    except OSError:
        pass
    return 0


def main():
    docs = []
    results_dir = "."
    args = sys.argv[1:]
    if "--" in args:
        i = args.index("--")
        docs, results_dir = args[:i], args[i + 1]
    else:
        docs = args
    os.makedirs(results_dir, exist_ok=True)
    sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
    import drive

    results = {}
    for doc in docs:
        name = os.path.basename(doc)
        logname = f"measure-{name}.log"
        extra = {"JOFFICE_JANK": "1"}
        drive.launch(["--file", doc], logname, extra)
        samples = []
        t0 = time.time()
        onready_at = None
        logp = drive.logpath(logname)
        while time.time() - t0 < 240:
            ps = procs()
            total = sum(pss_kb(p) for p in ps)
            if ps:
                samples.append({"t": round(time.time() - t0, 2), "pss_kb": total, "procs": ps})
            if os.path.exists(logp):
                with open(logp, errors="replace") as f:
                    content = f.read()
                if "modified_ready" in content:
                    onready_at = time.time() - t0
                    break
            time.sleep(0.1)
        time.sleep(3)  # settle
        ps = procs()
        settle_pss = sum(pss_kb(p) for p in ps)
        log_text = ""
        if os.path.exists(logp):
            with open(logp, errors="replace") as f:
                log_text = f.read()
        measure_lines = [l for l in log_text.splitlines() if "MEASURE" in l or "RAF_GAP" in l or "JANK" in l]
        peak = max((s["pss_kb"] for s in samples), default=0)
        results[name] = {
            "open_to_handshake_s": onready_at,
            "peak_pss_kb": peak,
            "settle_pss_kb": settle_pss,
            "bin_bytes": None,
            "measure_lines": measure_lines[-40:],
            "samples_tail": samples[-5:],
        }
        print(json.dumps({name: results[name]}, indent=1), flush=True)
        drive.run(["pkill", "-x", "joffice-shell"])
        time.sleep(1.5)
    with open(os.path.join(results_dir, "measure-results.json"), "w") as f:
        json.dump(results, f, indent=1)
    print("MEASURE DONE")


if __name__ == "__main__":
    main()
