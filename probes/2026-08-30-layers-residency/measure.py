#!/usr/bin/env python3
"""Run a build, timestamp every line it prints, and sample its RSS at 20 Hz.

Prints, per stage, the max RssAnon/VmRSS observed inside that stage's own window — the stage
timings the build prints give each stage's duration and the line's timestamp gives its end.
"""
import subprocess, sys, threading, time, os, re, json

binary, dep, out, log = sys.argv[1], sys.argv[2], sys.argv[3], sys.argv[4]
extra = sys.argv[5:]
if os.path.exists(out):
    subprocess.run(["rm", "-rf", out], check=True)
env = dict(os.environ)
env_path = os.path.join(os.path.dirname(os.path.abspath(dep)), ".env")
for line in open(env_path):
    if "=" in line:
        k, v = line.strip().split("=", 1)
        env[k] = v

t0 = time.monotonic()
p = subprocess.Popen([binary, "build", "--deployment", dep, "--out", out, "--stage-timings"] + extra,
                     stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True, bufsize=1, env=env)
samples = []       # (t, vmrss_kb, anon_kb, file_kb)
spill_peak = [0]
stop = threading.Event()

def sample():
    status = f"/proc/{p.pid}/status"
    tmp = os.path.join(out, ".build-tmp")
    n = 0
    while not stop.is_set():
        try:
            vals = {}
            with open(status) as f:
                for line in f:
                    if line.startswith(("VmRSS:", "RssAnon:", "RssFile:")):
                        vals[line.split(":")[0]] = int(line.split()[1])
            samples.append((time.monotonic() - t0, vals.get("VmRSS", 0), vals.get("RssAnon", 0), vals.get("RssFile", 0)))
        except OSError:
            pass
        n += 1
        if n % 10 == 0:
            try:
                total = 0
                for name in os.listdir(tmp):
                    if name.startswith("member"):
                        total += os.path.getsize(os.path.join(tmp, name))
                spill_peak[0] = max(spill_peak[0], total)
            except OSError:
                pass
        time.sleep(0.05)

th = threading.Thread(target=sample, daemon=True)
th.start()
lines = []
for line in p.stdout:
    lines.append((time.monotonic() - t0, line.rstrip("\n")))
p.wait()
stop.set()
th.join()
wall = time.monotonic() - t0

stage_re = re.compile(r"^stage\s+(\S+)\s+([\d.]+)s\s+rows=(\d+)\s+peak=\s*(\d+) MiB")
stages = []
for t, line in lines:
    m = stage_re.match(line)
    if m:
        stages.append((m.group(1), float(m.group(2)), t))

def window(lo, hi, idx):
    vals = [s[idx] for s in samples if lo <= s[0] <= hi]
    return max(vals) // 1024 if vals else 0

report = {"wall_s": round(wall, 2), "exit": p.returncode,
          "max_vmrss_mib": max((s[1] for s in samples), default=0) // 1024,
          "max_anon_mib": max((s[2] for s in samples), default=0) // 1024,
          "max_file_mib": max((s[3] for s in samples), default=0) // 1024,
          "spill_peak_mib": spill_peak[0] >> 20,
          "stages": []}
for name, dur, end in stages:
    report["stages"].append({"stage": name, "s": round(dur, 2),
                             "vmrss_mib": window(end - dur, end, 1),
                             "anon_mib": window(end - dur, end, 2)})
with open(log, "w") as f:
    f.write("\n".join(l for _, l in lines) + "\n")
with open(log + ".rss", "w") as f:
    for t, v, a, fi in samples:
        f.write("%.3f %d %d %d\n" % (t, v, a, fi))
with open(log + ".json", "w") as f:
    json.dump(report, f, indent=1)
print(json.dumps({k: v for k, v in report.items() if k != "stages"}))
for s in report["stages"]:
    print("  %-16s %6.2fs  vmrss=%5d MiB  anon=%5d MiB" % (s["stage"], s["s"], s["vmrss_mib"], s["anon_mib"]))
