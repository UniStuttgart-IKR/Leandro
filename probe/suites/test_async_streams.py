# SPDX-License-Identifier: MIT
# SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
# SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
"""Overlapping async streams with pinned host memory.

Exercises three things at once: pinned host allocations (the OS-descriptor
pinning path and the VA->GPA translation behind it), several CUDA streams
submitting concurrently, and mixed-precision matmul so DMA and compute
actually overlap instead of serialising.

Needs: torch.
"""

import torch
import time


def stress_test_async_streams(matrix_size=8192, num_streams=4, iterations=20):
    print("Starting async stream stress test...")
    print(f"Matrix: {matrix_size}x{matrix_size}, streams: {num_streams}, "
          f"iterations: {iterations}")

    # Pinned host memory. If these pages are not pinned properly for the
    # real driver, the DMA goes wrong rather than failing loudly -- which is
    # why a checksum is printed per iteration.
    host_data = [torch.randn(matrix_size, matrix_size, pin_memory=True)
                 for _ in range(num_streams)]
    host_results = [torch.empty(matrix_size, matrix_size, pin_memory=True)
                    for _ in range(num_streams)]

    # Separate CUDA streams, so submissions overlap.
    streams = [torch.cuda.Stream() for _ in range(num_streams)]

    device_tensors = [torch.empty(matrix_size, matrix_size, device='cuda')
                      for _ in range(num_streams)]
    weights = torch.randn(matrix_size, matrix_size, device='cuda')

    torch.cuda.synchronize()
    start_time = time.perf_counter()

    for i in range(iterations):
        for s_idx, stream in enumerate(streams):
            with torch.cuda.stream(stream):
                # async host -> device
                device_tensors[s_idx].copy_(host_data[s_idx], non_blocking=True)

                with torch.autocast(device_type='cuda', dtype=torch.float16):
                    # Repeated deliberately: stretches the compute so the
                    # DMA has something to overlap with.
                    for _ in range(3):
                        device_tensors[s_idx] = torch.matmul(
                            device_tensors[s_idx], weights)
                        device_tensors[s_idx] = torch.nn.functional.gelu(
                            device_tensors[s_idx])

                # async device -> host
                host_results[s_idx].copy_(device_tensors[s_idx],
                                          non_blocking=True)

        for stream in streams:
            stream.synchronize()

        if i % 5 == 0:
            print(f"iter {i}/{iterations} done, checksum s0: "
                  f"{host_results[0].sum().item():.4f}")

    torch.cuda.synchronize()
    end_time = time.perf_counter()

    print(f"async streams ok, duration: {end_time - start_time:.4f}s")


if __name__ == "__main__":
    stress_test_async_streams()
