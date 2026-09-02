"""Time to /readyz, the open's own warm-up line, and peak RSS — one fresh process."""
import os, subprocess, sys, time, signal, requests, re

W = "/tmp/claude-1000/-home-joe-code-tessera/e2f19e9a-6b70-45bd-ad7f-ff1233184a80/scratchpad/work"
BIN = sys.argv[1]
TAG = sys.argv[2]
env = dict(os.environ)
for line in open(os.path.join(W, ".env")):
    line = line.strip()
    if line and not line.startswith("#") and "=" in line:
        k, v = line.split("=", 1)
        env[k.strip()] = v.strip().strip('"').strip("'")

log = os.path.join(W, "serve-%s.log" % TAG)
lf = open(log, "w")
p = subprocess.Popen([BIN, "serve", "--deployment", os.path.join(W, "tessera.toml")],
                     stdout=lf, stderr=lf, cwd=W, start_new_session=True, env=env)
t0 = time.monotonic()
ready = None
peak = 0
try:
    while True:
        if p.poll() is not None:
            raise SystemExit("server exited:\n" + open(log).read()[-3000:])
        try:
            with open("/proc/%d/status" % p.pid) as f:
                for line in f:
                    if line.startswith("VmHWM"):
                        peak = max(peak, int(line.split()[1]))
        except Exception:
            pass
        try:
            if requests.get("http://127.0.0.1:8211/readyz", timeout=2).status_code == 200:
                ready = time.monotonic() - t0
                break
        except Exception:
            pass
        time.sleep(0.05)
    try:
        with open("/proc/%d/status" % p.pid) as f:
            for line in f:
                if line.startswith("VmHWM"):
                    peak = max(peak, int(line.split()[1]))
    except Exception:
        pass
finally:
    try:
        os.killpg(os.getpgid(p.pid), signal.SIGTERM); p.wait(timeout=60)
    except Exception:
        os.killpg(os.getpgid(p.pid), signal.SIGKILL); p.wait()

print("%s: readyz %.1f s, peak RSS %.2f GB" % (TAG, ready, peak / 1024 / 1024))
for line in open(log):
    if "row form" in line or "warm" in line or "elapsed_ms" in line or "adopt" in line.lower():
        print("   ", line.rstrip()[:400])
