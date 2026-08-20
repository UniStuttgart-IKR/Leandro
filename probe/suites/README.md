<!-- SPDX-License-Identifier: MIT -->
# Python suites

Breadth tests over real CUDA APIs — PyTorch, CuPy and RAPIDS — run in the
guest against the virtio-nvrm boundary. They exist to find what the narrow
probes do not reach: a suite drags in a whole library stack and uses APIs
nobody wrote a probe for.

## How these relate to the gates and the probes

Three things live in this repository that all "run something on the GPU",
and they are not interchangeable:

| | what it is | verdict | when it runs |
|---|---|---|---|
| **Gate** (`scripts/test.sh gpu`) | the acceptance criterion | binary PASS/FAIL against the **native host run** | after every change to module, host or session logic |
| **Probes** (`probe/c`, `probe/python`) | single-purpose instruments | print measured values | driven by the gate and the benchmarks |
| **Suites** (here) | breadth over real API surface | a table; expectations derived from the rig's knobs | by hand, when the question is "what does not work yet" |

The distinction that matters: **the gate must stay binary.** It compares
against a native reference and anything other than PASS is a defect. The
suites can expect failures — their expectations depend on knobs
(`LEA_MANAGED_COMPAT`, `max_pin_mib`) that the gate deliberately does not
set. Folding them into the gate would mean teaching the gate to accept red,
which is the one thing a gate must not learn.

The runner is therefore its own script, not a gate mode — but it lives in
`probe/run/` beside the other probe entry points rather than becoming a
third top-level thing.

## Running them

```sh
./scripts/showcase.sh up           # a guest with the module loaded
./probe/run/suites.sh              # all eight, one result table
./probe/run/suites.sh --list       # what is expected of each
./probe/run/suites.sh --only nvrtc # just one
```

Exit code: `0` everything as expected (including known failures), `1` a
deviation, `2` the run could not be carried out (guest unreachable, no venv).

## Dependencies

**None of these are in the base image.** The guest needs a venv at
`~/gpu/venv` (`scripts/showcase.sh up --with-torch` creates one,
~2.5 GiB). A suite whose imports are missing reports `SKIP`, not `FAIL` — a
missing library says nothing about the boundary.

Measured against: torch 2.13.0, cupy-cuda13x 14.1.1, cudf/cuml-cu13 26.6.0.

| suite | needs |
|---|---|
| `test_async_streams.py`, `test_cuda_graphs.py`, `test_high_freq_event_polling.py`, `test_multi_process_cuda.py`, `test_vram_churn.py` | torch |
| `test_nvrtc.py`, `test_uvm_migration.py` | cupy |
| `rapids_cuml_cudf.py` | cupy, cudf, cuml |

## What each one checks

| suite | checks |
|---|---|
| `test_async_streams.py` | pinned host memory + 4 concurrent streams + mixed-precision matmul, so DMA and compute overlap |
| `test_cuda_graphs.py` | capture a kernel sequence once, replay it 1000×; compares against eager launches |
| `test_high_freq_event_polling.py` | 5000 timed event pairs back to back — pressure on the submission path |
| `test_multi_process_cuda.py` | CUDA IPC: a device tensor handed to a spawned process, written through, read back |
| `test_nvrtc.py` | runtime compilation (libnvrtc + PTX JIT) and a raw kernel launch |
| `test_uvm_migration.py` | `cudaMallocManaged` 256 MiB, GPU writes, host reads back |
| `test_vram_churn.py` | fill VRAM to OOM, reduce over every block, then free/re-allocate in cycles |
| `rapids_cuml_cudf.py` | cuDF dataframe over 20M rows plus a cuML random forest — the broadest surface |

## Last measured state

Rig: driver 610.57.04, RTX 2070, persistence on, cloud-hypervisor v53.0,
guest module `virtio_nvrm.ko`, host backend `vhost-user-nvrm` **with
`LEA_MANAGED_COMPAT=1`**. Re-measured 2026-08-20; the same eight passed at
610.43.03 before it.

With `LEA_MANAGED_COMPAT=1` on the host **and** `max_pin_mib >= 2048` in the
guest, **all eight pass**. Both of the failures once recorded here turned out
to be knobs rather than boundaries.

WARNING: "all eight" needs three libraries the guest image does not carry.
The image has torch and numpy; `cupy`, `cudf` and `cuml` are not in it,
and without them three suites report SKIP -- which is the runner behaving
correctly (a missing dependency says nothing about the boundary) and NOT
the same statement as a pass. Measured 2026-08-20 on a guest built from
`build.sh bake --with-torch`: 5 PASS / 3 SKIP as delivered, 7/8 after
`cupy-cuda13x`, 8/8 after RAPIDS. To get there:

```sh
scripts/showcase.sh ssh -- '~/gpu/venv/bin/pip install cupy-cuda13x'
scripts/showcase.sh ssh -- '~/gpu/venv/bin/pip install \
    --extra-index-url=https://pypi.nvidia.com cudf-cu13 cuml-cu13'
```

Versions the 8/8 run used: torch 2.13.0, numpy 2.5.2, cupy 14.2.0,
cudf 26.08.00, cuml 26.08.00. RAPIDS is several GB; that is why it is not
baked in.

| suite | result | note |
|---|---|---|
| `test_async_streams.py` | PASS | needs `max_pin_mib >= 2048`, see below |
| `test_cuda_graphs.py` | PASS | |
| `test_high_freq_event_polling.py` | PASS | |
| `test_multi_process_cuda.py` | PASS | |
| `test_nvrtc.py` | PASS | |
| `test_uvm_migration.py` | PASS | depends on the host switch, see below |
| `test_vram_churn.py` | PASS | reaches OOM in phase 1, which is expected |
| `rapids_cuml_cudf.py` | PASS | |

### Not a limitation either: pinned host memory

`test_async_streams.py` was recorded as a hard boundary limitation, failing
at the first `pin_memory=True` with `cudaErrorOperatingSystem` (304). It is
not. Host pinning **works**: the guest module pins the user pages
(`pin_user_pages_fast`), reports their GPA runs, and the host assembles them
into one contiguous host VA registered as an OS descriptor. The GPU gate
exercises exactly that path and counts 27 successful pins per run.

What the suite hits is a **cap** — in fact two of them, at opposite ends:

| limit | where | scope | default |
|---|---|---|---|
| `max_pin_mib` | guest module parameter | cumulative, all pins | 1024 MiB |
| `LEA_MAX_PIN_MIB` | host `vhost-user-nvrm` | **one arena, i.e. one allocation** | 256 MiB |

The suite allocates 256 MiB buffers (sitting exactly on the host default) and
about 2 GiB in total (over the guest default), so it can hit either. Both
refuse with CUDA error 304; the backend log tells them apart — the host one
prints `over the pin limit ... (LEA_MAX_PIN_MIB)`. Measured on one rig:

| | result |
|---|---|
| single allocation, 4 / 64 / 256 MiB | OK |
| cumulative at `max_pin_mib=1024` | stops at 960 MiB |
| cumulative at `max_pin_mib=3072` | 2560 MiB, no failure reached |
| the suite at 1024 | FAIL, error 304 |
| the suite at 3072 | **PASS** |

Raise the guest one with `scripts/showcase.sh up --max-pin-mib 3072`
(or `LEA_GUEST_MAX_PIN_MIB`); raise the host one by starting the backend with
`LEA_MAX_PIN_MIB=<MiB>`. Both name themselves in their error messages.

Two caveats, so this is not read as more than it is:

- A **single** allocation failing at 512 MiB while 15 x 64 MiB succeeded is
  the HOST's per-arena limit, not fragmentation. Verified from the backend
  log: `arena length 536870912 over the pin limit 256 MiB
  (LEA_MAX_PIN_MIB)`. An earlier note here blamed the aux buffer
  (`max_runs`); that was wrong.
- The suite prints `nan` checksums. That is the test's own fp16 arithmetic
  overflowing, not the boundary: the **native** host run produces exactly
  the same `nan`, in the same time. The suite therefore proves that the path
  survives, not that the numbers are right.

`probe/c/hostregprobe.c` measures the `cudaHostRegister` path directly.

### Not a limitation: managed memory

`test_uvm_migration.py` was previously recorded as a second hard failure.
It is not. Measured both ways on one rig, everything else identical:

| host backend | result |
|---|---|
| `LEA_MANAGED_COMPAT=1` | **PASS** — reads 52.0 back |
| without it | `cudaErrorInvalidValue: invalid argument` |

`LEA_MANAGED_COMPAT` is a **host** switch, read in `session.rs`; setting it
inside the guest does nothing. Without it the managed-only UVM commands are
forwarded and refused with `0x1e`, and `cudaMallocManaged` fails loudly —
which is the intended default, not an accident.

`probe/run/suites.sh` therefore *derives* this expectation from the running
backend's environment instead of hard-coding it. Hard-coding is exactly what
turned a missing environment variable into a recorded architecture limit.

### Where managed memory actually lives

Not in VRAM. The guest pins its own anonymous pages, reports their GPA runs,
and the host assembles them into one contiguous host VA registered as
`NV01_MEMORY_SYSTEM_OS_DESCRIPTOR` and attached to a GPU VA. The flags say
it plainly (`host_pool.rs`, `OSDESC_FLAGS`): `LOCATION 11:8 = PCI`,
`COHERENCY = CACHED`. So the GPU reaches those pages **over the bus, in
system RAM**, on every access.

That is exactly why "nothing to migrate" is the truthful answer: real
managed memory would move pages into VRAM on GPU access and back on CPU
access; here they are permanently sysmem-resident and stay put.

Three consequences worth knowing:

- **Bandwidth is the PCIe link, not VRAM.** Orders of magnitude apart. A
  kernel hammering a managed buffer is not doing what it would do natively.
  (Reasoned from the wiring, not measured here -- the RIG line carries
  `pcie=` so a measurement can at least be attributed.)
- **It costs guest RAM, pinned.** Not card memory. So the two pin limits
  above are what bounds it, and a VRAM cap would not count it at all.
- **Oversubscription cannot work**, not merely because migration is faked
  but because the backing is guest RAM: there cannot be more managed memory
  than the guest has.

"Managed light" answers the migration and hint commands as semantic no-ops
for pages that are already sysmem-resident.
