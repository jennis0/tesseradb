"""What a build has on the disk, file by file, sampled while it runs.

Walks the bundle root every SAMPLE_S seconds and records, per file, when it first appeared,
when it went, and the largest it ever was — apparent bytes and allocated blocks, which differ
wherever a file is sparse or fallocated past its content. The build's own `--stage-timings-json`
gives the stage boundaries the sample times are read against.

  sample.py <corpus dir> <work dir> <binary> [extra build args...]
"""
import json, os, subprocess, sys, threading, time
from pathlib import Path

SAMPLE_S = 0.5

corpus, work, binary = Path(sys.argv[1]), Path(sys.argv[2]), sys.argv[3]
extra = sys.argv[4:]
work.mkdir(parents=True, exist_ok=True)
out = work / "bundle"
if out.exists():
    import shutil; shutil.rmtree(out)

env = dict(os.environ)
for line in (corpus / ".env").read_text().splitlines():
    if "=" in line and not line.startswith("#"):
        k, v = line.split("=", 1)
        env[k.strip()] = v.strip()

seen = {}          # path -> [first_t, last_t, max_size, max_blocks]
series = []        # (t, tmp_size, tmp_blocks, out_size, out_blocks, nfiles)
peak = {"blocks": -1, "t": 0.0, "listing": []}
stop = threading.Event()
t0 = time.time()

def walk():
    tmp_s = tmp_b = out_s = out_b = 0
    listing = []
    for root, _dirs, files in os.walk(out, onerror=lambda e: None):
        is_tmp = ".build-tmp" in root
        for f in files:
            p = os.path.join(root, f)
            try:
                st = os.lstat(p)
            except OSError:
                continue
            size, blocks = st.st_size, st.st_blocks * 512
            rel = os.path.relpath(p, out)
            listing.append((rel, size, blocks))
            if is_tmp:
                tmp_s += size; tmp_b += blocks
            else:
                out_s += size; out_b += blocks
    return tmp_s, tmp_b, out_s, out_b, listing

def sampler():
    while not stop.is_set():
        t = time.time() - t0
        tmp_s, tmp_b, out_s, out_b, listing = walk()
        for rel, size, blocks in listing:
            e = seen.get(rel)
            if e is None:
                seen[rel] = [t, t, size, blocks]
            else:
                e[1] = t
                if size > e[2]: e[2] = size
                if blocks > e[3]: e[3] = blocks
        series.append((t, tmp_s, tmp_b, out_s, out_b, len(listing)))
        if tmp_b + out_b > peak["blocks"]:
            peak.update(blocks=tmp_b + out_b, t=t, listing=sorted(listing, key=lambda r: -r[2]))
        stop.wait(SAMPLE_S)

cmd = [binary, "build", "--deployment", str(corpus / "tessera.toml"),
       "--config", str(corpus / "corpus.toml"), "--out", str(out),
       "--stage-timings", "--stage-timings-json", str(work / "stages.json")] + extra
log = open(work / "build.log", "w")
proc = subprocess.Popen(cmd, stdout=log, stderr=subprocess.STDOUT, env=env)
if "tessera" not in (subprocess.run(["ps", "-o", "comm=", "-p", str(proc.pid)],
                                    capture_output=True, text=True).stdout):
    proc.kill(); sys.exit("the pid is not the build; refusing to report its disk")
thread = threading.Thread(target=sampler, daemon=True)
thread.start()
code = proc.wait()
stop.set(); thread.join()
log.close()

with open(work / "files.tsv", "w") as f:
    f.write("first_s\tlast_s\tmax_size\tmax_blocks\tpath\n")
    for rel, (a, b, s, bl) in sorted(seen.items(), key=lambda kv: -kv[1][3]):
        f.write(f"{a:.1f}\t{b:.1f}\t{s}\t{bl}\t{rel}\n")
with open(work / "series.tsv", "w") as f:
    f.write("t\ttmp_size\ttmp_blocks\tout_size\tout_blocks\tnfiles\n")
    for row in series:
        f.write("\t".join(str(x) for x in row) + "\n")
with open(work / "peak.tsv", "w") as f:
    f.write(f"# peak at t={peak['t']:.1f}s, {peak['blocks']} allocated bytes\n")
    f.write("size\tblocks\tpath\n")
    for rel, size, blocks in peak["listing"]:
        f.write(f"{size}\t{blocks}\t{rel}\n")
json.dump({"t0": t0, "cmd": cmd, "exit": code, "peak_blocks": peak["blocks"],
           "peak_t": peak["t"]}, open(work / "meta.json", "w"), indent=1)
print(f"exit {code}; {len(series)} samples; peak {peak['blocks'] / 2**30:.2f} GiB at {peak['t']:.0f}s")
sys.exit(code)
