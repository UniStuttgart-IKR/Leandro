# SPDX-License-Identifier: MIT
# SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
# SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
"""CUDA graphs: capture a kernel sequence once, replay it 1000 times.

Compares eager launches against graph replay. The interesting part is not
the speed-up but that replay works at all -- a captured graph is submitted
without walking the per-launch path again.

Needs: torch.
"""

import torch
import time


def cuda_graph_test():
    print("Starting CUDA graphs test...")

    N = 4096
    x = torch.randn(N, N, device='cuda')
    y = torch.randn(N, N, device='cuda')

    # Warm-up on a side stream, as graph capture requires.
    s = torch.cuda.Stream()
    s.wait_stream(torch.cuda.current_stream())
    with torch.cuda.stream(s):
        for _ in range(3):
            z = torch.matmul(x, y)
    torch.cuda.synchronize()

    print("1000 launches WITHOUT CUDA graphs...")
    start_time = time.perf_counter()
    for _ in range(1000):
        z = torch.matmul(x, y)
        z = torch.relu(z)
    torch.cuda.synchronize()
    eager_time = time.perf_counter() - start_time
    print(f"eager execution: {eager_time * 1000:.2f} ms")

    print("Capturing CUDA graph...")
    g = torch.cuda.CUDAGraph()

    # Static input/output tensors, required for capture.
    static_x = x.clone()
    static_y = y.clone()
    static_z = torch.empty_like(static_x)

    with torch.cuda.graph(g):
        static_z = torch.matmul(static_x, static_y)
        static_z = torch.relu(static_z)

    print("1000 launches WITH graph replay...")
    start_time = time.perf_counter()
    for _ in range(1000):
        g.replay()
    torch.cuda.synchronize()
    graph_time = time.perf_counter() - start_time
    print(f"graph execution: {graph_time * 1000:.2f} ms")

    speedup = eager_time / graph_time
    print(f"speedup: {speedup:.2f}x")
    print("CUDA graphs test ok")


if __name__ == "__main__":
    cuda_graph_test()
