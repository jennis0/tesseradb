"""Sample a running process's user-space instruction pointers with `perf_event_open`, where no
profiler is installed, and count them by symbol of the executable.

    python3 ipsample.py <pid> <seconds> <executable>

Every thread of the process is sampled on the CPU clock at 997 Hz with the kernel excluded, which
`perf_event_paranoid = 2` allows for one's own processes. An address is resolved against `nm`'s
symbols using the executable's mapping in `/proc/<pid>/maps`; one outside the executable is
counted under the mapping it falls in.
"""

from __future__ import annotations

import bisect
import collections
import ctypes
import mmap
import os
import struct
import subprocess
import sys
import time

PERF_TYPE_SOFTWARE, PERF_COUNT_SW_CPU_CLOCK = 1, 0
PERF_SAMPLE_IP, PERF_SAMPLE_TID = 1, 2
PERF_RECORD_SAMPLE = 9
EXCLUDE_KERNEL, EXCLUDE_HV = 1 << 5, 1 << 6
NR_PERF_EVENT_OPEN = 298
PAGES = 1 + 64

libc = ctypes.CDLL(None, use_errno=True)


def open_event(tid: int) -> int:
    attr = bytearray(128)
    struct.pack_into("<IIQQQQ", attr, 0, PERF_TYPE_SOFTWARE, 128, PERF_COUNT_SW_CPU_CLOCK,
                     997, PERF_SAMPLE_IP | PERF_SAMPLE_TID, 0)
    # flags: freq (bit 10), exclude_kernel, exclude_hv
    struct.pack_into("<Q", attr, 40, (1 << 10) | EXCLUDE_KERNEL | EXCLUDE_HV)
    buf = ctypes.create_string_buffer(bytes(attr))
    fd = libc.syscall(NR_PERF_EVENT_OPEN, buf, tid, -1, -1, 0)
    if fd < 0:
        raise OSError(ctypes.get_errno(), f"perf_event_open on {tid}")
    return fd


def drain(ring: mmap.mmap, counts: collections.Counter) -> None:
    page = mmap.PAGESIZE
    head = struct.unpack_from("<Q", ring, 1024)[0]
    tail = struct.unpack_from("<Q", ring, 1032)[0]
    size = (PAGES - 1) * page
    while tail < head:
        at = page + tail % size
        kind, _misc, length = struct.unpack_from("<IHH", ring, at)
        if kind == PERF_RECORD_SAMPLE:
            record = bytes(ring[page + (tail + 8) % size: page + (tail + 8) % size + 16])
            if len(record) == 16:
                counts[struct.unpack_from("<Q", record, 0)[0]] += 1
        tail += length
    struct.pack_into("<Q", ring, 1032, tail)


def symbols(executable: str) -> tuple[list[int], list[str]]:
    out = subprocess.run(["nm", "-C", "--defined-only", "-n", executable], capture_output=True,
                         text=True).stdout
    addrs, names = [], []
    for line in out.splitlines():
        parts = line.split(" ", 2)
        if len(parts) == 3 and parts[1] in "tTwW":
            addrs.append(int(parts[0], 16))
            names.append(parts[2])
    return addrs, names


def main() -> None:
    pid, seconds, executable = int(sys.argv[1]), float(sys.argv[2]), sys.argv[3]
    maps = []
    for line in open(f"/proc/{pid}/maps"):
        fields = line.split()
        lo, hi = (int(x, 16) for x in fields[0].split("-"))
        maps.append((lo, hi, int(fields[2], 16), fields[5] if len(fields) > 5 else "[anon]"))
    base = min(lo - off for lo, _, off, path in maps if path == os.path.realpath(executable))
    rings = []
    for tid in os.listdir(f"/proc/{pid}/task"):
        fd = open_event(int(tid))
        rings.append((fd, mmap.mmap(fd, PAGES * mmap.PAGESIZE)))
    counts: collections.Counter = collections.Counter()
    end = time.time() + seconds
    while time.time() < end:
        time.sleep(0.05)
        for _, ring in rings:
            drain(ring, counts)
    addrs, names = symbols(executable)
    by_symbol: collections.Counter = collections.Counter()
    for ip, n in counts.items():
        mapped = next((m for m in maps if m[0] <= ip < m[1]), None)
        if mapped and mapped[3] == os.path.realpath(executable):
            i = bisect.bisect_right(addrs, ip - base) - 1
            by_symbol[names[i] if i >= 0 else "?"] += n
        else:
            by_symbol[mapped[3] if mapped else "?"] += n
    total = sum(by_symbol.values())
    print(f"{total} samples over {seconds:.0f} s, {len(rings)} threads")
    for name, n in by_symbol.most_common(40):
        print(f"{100 * n / total:6.2f}%  {n:7d}  {name[:200]}")


if __name__ == "__main__":
    main()
