"""What one build process holds and moves, sampled beside it.

Records, every SAMPLE_S seconds for as long as the pid lives: the wall clock, the process's
anonymous, file-backed and swapped resident sizes, its CPU ticks and fault counts, the bytes it
has read from and written to the block layer, and the allocated bytes under the bundle root
(`st_blocks` × 512, so a sparse or fallocated file is charged what it occupies).

The sampler never starts or stops the build. `run.sh` starts it and passes the pid here.

  sample.py <pid> <bundle root> <out tsv>
"""

import os
import sys
import time
from pathlib import Path

SAMPLE_S = 2.0

COLUMNS = [
    "t",  # seconds since the Unix epoch, so a sample lines up with a stage record
    "wall_s",  # seconds since the first sample
    "rss_anon_kb",
    "rss_file_kb",
    "vm_swap_kb",
    "cpu_ticks",  # utime + stime
    "minflt",
    "majflt",
    "read_bytes",
    "write_bytes",
    "bundle_alloc_b",
    "bundle_files",
]


def status(pid):
    """RssAnon, RssFile and VmSwap in KiB."""
    want = {"RssAnon:": 0, "RssFile:": 0, "VmSwap:": 0}
    with open(f"/proc/{pid}/status") as f:
        for line in f:
            key = line.split(maxsplit=1)[0]
            if key in want:
                want[key] = int(line.split()[1])
    return want["RssAnon:"], want["RssFile:"], want["VmSwap:"]


def stat(pid):
    """utime + stime in clock ticks, and the minor and major fault counts.

    The command name sits in parentheses and may hold spaces, so the numeric fields are read
    from after the last closing parenthesis. Field numbers are `proc(5)`'s, one-based.
    """
    with open(f"/proc/{pid}/stat") as f:
        raw = f.read()
    rest = raw[raw.rindex(")") + 2 :].split()
    minflt, majflt = int(rest[7]), int(rest[9])
    utime, stime = int(rest[11]), int(rest[12])
    return utime + stime, minflt, majflt


def io(pid):
    read = write = 0
    with open(f"/proc/{pid}/io") as f:
        for line in f:
            if line.startswith("read_bytes:"):
                read = int(line.split()[1])
            elif line.startswith("write_bytes:"):
                write = int(line.split()[1])
    return read, write


def allocated(root):
    """Allocated bytes and file count under the bundle root, including its scratch directory."""
    total = files = 0
    for dirpath, _dirs, names in os.walk(root, onerror=lambda e: None):
        for name in names:
            try:
                st = os.lstat(os.path.join(dirpath, name))
            except OSError:
                continue
            total += st.st_blocks * 512
            files += 1
    return total, files


def main():
    pid, root, out = int(sys.argv[1]), Path(sys.argv[2]), Path(sys.argv[3])
    out.parent.mkdir(parents=True, exist_ok=True)
    t0 = time.time()
    with open(out, "w", buffering=1) as f:
        f.write("\t".join(COLUMNS) + "\n")
        while True:
            t = time.time()
            try:
                anon, filek, swap = status(pid)
                ticks, minflt, majflt = stat(pid)
                read, write = io(pid)
            except (OSError, ValueError, IndexError):
                break  # the build has gone
            alloc, nfiles = allocated(root)
            f.write(
                "\t".join(
                    str(x)
                    for x in (
                        f"{t:.3f}",
                        f"{t - t0:.1f}",
                        anon,
                        filek,
                        swap,
                        ticks,
                        minflt,
                        majflt,
                        read,
                        write,
                        alloc,
                        nfiles,
                    )
                )
                + "\n"
            )
            slept = SAMPLE_S - (time.time() - t)
            if slept > 0:
                time.sleep(slept)


if __name__ == "__main__":
    main()
