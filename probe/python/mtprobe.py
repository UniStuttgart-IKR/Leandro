# SPDX-License-Identifier: MIT
# SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
# SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
# Concurrency INSIDE one guest: several threads, several streams.
#
# "One round trip at a time per connection (a mutex around the round)" is
# on record as deliberately unsupported for multithreaded libcuda. What is
# measured here is (a) whether it stays correct and (b) how expensive the
# serialization gets when many threads do setup work at once -- setup being
# the path that costs ioctls at all.
import sys, threading, time
import torch

N_THREADS = int(sys.argv[1]) if len(sys.argv) > 1 else 8
N_ITERS = int(sys.argv[2]) if len(sys.argv) > 2 else 20

torch.cuda.init()
dev = torch.device("cuda:0")
errors = []
barrier = threading.Barrier(N_THREADS)


def worker(tid):
    try:
        barrier.wait()
        stream = torch.cuda.Stream(device=dev)
        with torch.cuda.stream(stream):
            for i in range(N_ITERS):
                # Every iteration touches fresh memory -> the allocator goes
                # to the driver occasionally, i.e. real ioctls.
                a = torch.full((256, 256), float(tid + 1), device=dev)
                b = torch.full((256, 256), 2.0, device=dev)
                c = a * b
                del a, b
                s = float(c.sum().item())
                want = (tid + 1) * 2.0 * 256 * 256
                if abs(s - want) > 1e-3:
                    errors.append(f"T{tid} Iter{i}: {s} != {want}")
                del c
        stream.synchronize()
    except Exception as e:  # noqa: BLE001
        errors.append(f"T{tid}: {type(e).__name__}: {e}")


t0 = time.perf_counter()
threads = [threading.Thread(target=worker, args=(i,)) for i in range(N_THREADS)]
for t in threads:
    t.start()
for t in threads:
    t.join()
t1 = time.perf_counter()

print(f"mtprobe: {N_THREADS} threads x {N_ITERS} iterations, "
      f"{(t1 - t0) * 1000:.1f} ms, errors: {len(errors)}")
for e in errors[:5]:
    print("  ", e)
sys.exit(1 if errors else 0)
