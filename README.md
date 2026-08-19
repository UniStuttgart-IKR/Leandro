<!-- SPDX-License-Identifier: MIT -->
# Project Leandro

**Cooperative GPU paravirtualization without SR-IOV:** unmodified CUDA
applications inside a VM, computing on a consumer NVIDIA GPU that the host
keeps. No passthrough, no vGPU licence, no host kernel patch.

> [!WARNING]
> **Written by AI, and not line-by-line reviewed.** Roughly 99 % of the code
> and the documentation in this repository was written by large language
> models — Claude Opus, Claude Fable, Claude Sonnet and Gemini 3 Pro — under
> human direction, with every claim measured against a real card. It has
> **not** had a full human code review. Treat it as a research prototype for
> rapid experimentation, not as something to put in front of users or into a
> production path.
>
> The review is happening in phases: understanding, reviewing, then rewriting
> the documentation. Honestly, a complete line-by-line review of this much
> code can take a very long time, and this disclaimer stays until it is done.

> [!IMPORTANT]
> **This is not the thesis. It is what fell out of it.** The Master's thesis
> behind this is [MeisterStack](https://github.com/UniStuttgart-IKR/MeisterStack),
> a lightweight VM orchestrator written in Rust. Leandro started as an
> experiment inside that work — can a VM the orchestrator places get a real
> GPU, on consumer hardware, without SR-IOV? — and then went wrong in a
> productive direction: it worked, kept working, and grew into a repository
> of its own.
>
> So the thesis has priority, and until it is finished — end of 2026 — this
> project is only **semi-active**: issues and pull requests may sit for a
> while, and the direction is set by what the orchestrator needs from it.
> That is not disinterest, it is a deadline. The disclaimer goes when the
> thesis is done.

> [!CAUTION]
> **Only ever run on one GPU architecture: Turing.** The class tables cover
> Fermi through Blackwell, derived mechanically from the driver's own
> `resource_list.h`, but only a Turing card (RTX 2070) has been exercised on
> real silicon. Everything else is marked unverified in the descriptor table
> and the host logs a line the first time a guest touches such a class. A run
> on any other architecture is an experiment — please report how it goes.

## What this is, without assuming anything

A virtual machine normally cannot use the graphics card in the machine it
runs on. The usual answers are to hand the whole card to one VM and lose it
for everything else (**passthrough**), or to buy a card and a licence that
can slice itself up (**SR-IOV**, NVIDIA vGPU). On an ordinary consumer card
in an ordinary desktop, neither is available.

This project takes a third route. Inside the guest, programs load NVIDIA's
**real** proprietary software — the same `libcuda.so`, the same PTX
compiler, the same `nvidia-smi`. Nothing is reimplemented and nothing is
emulated. One layer further down, where that software would normally talk
to the graphics driver in the kernel, a small guest driver takes the
request, hands it across the VM boundary, and a program on the host runs it
against the real card. The answer travels back the same way.

Because the cut is made **below** CUDA and **above** the hardware, nothing
in the chain has to know what CUDA *is*. It only has to know that request
number `0x2b` carries a 40-byte parameter block, and where inside that
block a file descriptor sits. That is where the completeness comes from:
unmodified programs work, including ones nobody tested, because the layer
being carried is small and finite while the API above it is neither.

The price is stated up front: NVIDIA promises nothing about the stability
of that layer, so the driver in the guest and the driver on the host must
be the **same version**. That is a design assumption here, not an oversight.

## What this is not

| | Why it is different |
|---|---|
| **GPU passthrough** (VFIO) | gives one VM the whole card and takes it away from the host and from every other VM. Here the host keeps its card and several guests share it. |
| **SR-IOV / NVIDIA vGPU** | needs a datacentre card and a licence. This runs on a consumer RTX. |
| **API remoting** (rCUDA, and friends) | intercepts the CUDA API, so it has to implement each function and goes stale as the API grows. This intercepts the driver interface *below* it, so it never sees a CUDA function at all. |
| **virtio-gpu / Venus / virgl** | paravirtualise *graphics* APIs (Vulkan, OpenGL) for rendering. They do not carry CUDA, and NVIDIA's proprietary stack does not sit behind them. |
| **A container** | shares the host kernel. This is a real VM with its own kernel and no GPU of its own. |
| **Production software** | see the warning above. It is a research prototype with gates, not a product with support. |

---

**Building it, running it, testing it: [`DEVELOPMENT.md`](DEVELOPMENT.md).**

Architecture and what each file does:
[`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md).
Measuring and gates: [`docs/TESTING.md`](docs/TESTING.md).
Display and rendering, and why they are a second project:
[`docs/DISPLAY.md`](docs/DISPLAY.md).
Every question, settled or not: [`docs/OPEN-QUESTIONS.md`](docs/OPEN-QUESTIONS.md).
Notes for working on the code, and the measurements behind the numbers:
[`docs/llm.md`](docs/llm.md).
Naming scheme: [`docs/NAMING.md`](docs/NAMING.md) — the former name
`nvshim` is retired ([`docs/OPEN-QUESTIONS.md`](docs/OPEN-QUESTIONS.md)
number 3 records why).

**Where this came from:**
[MeisterStack](https://github.com/UniStuttgart-IKR/MeisterStack), a
lightweight VM orchestrator in Rust and the actual subject of the thesis
(IKR, University of Stuttgart). The two fit together the obvious way round,
which is why the experiment happened at all: MeisterStack places and manages
the VMs, and this project is what gives one of them a real GPU. That
relationship also explains the roadmap below — packaging, scheduling and
deployment are what an orchestrator needs from a component, in that
order, and in that order they have been landing.

## What this is for

Two goals, and both of them are about the code leaving this repository
rather than staying in it.

**A showcase.** As much of this as possible should be readable and reusable
by people who work on kernels and virtualization. That is why it is MIT
licensed, why the reasoning sits in the code next to what it explains, and
why negative results and retracted hypotheses are written down instead of
quietly deleted — a dead end that is recorded is worth more than one that
has to be walked twice.

**Upstream, where a piece is genuinely general.** Some of this is not
NVIDIA-specific at all and should not live here forever:

- The SHMEM support for cloud-hypervisor's *generic* vhost-user device
  (`patches/0001-generic-vhost-user-shmem.patch`) is about 300 lines with no
  NVIDIA in it. Any vhost-user device that needs a shared window has the same
  gap, and QEMU's `vhost-user-device` has it too.
- The device itself is a **table-described UAPI carrier**, not a GPU device.
  Eight of its ten message kinds are already generic. What that would mean
  as a virtio device, and what stands in the way, is worked through in
  [`docs/VIRTIO-UAPI.md`](docs/VIRTIO-UAPI.md).
- The measurement tooling (`nvrm-trace`, the probes) is useful for anyone
  reverse-engineering an ioctl surface, whoever's it is.

Related prior art is credited under [Acknowledgements](#acknowledgements)
rather than competed with.

## Status

> Every number in this section was measured on one rig, by one person.
> They are measurements, not benchmarks — see the rule under
> [Roadmap](#built-while-the-thesis-runs) for what would change that.

**Unmodified programs run without LD_PRELOAD, without wrappers, as a
normal user** -- inside a real VM that has no GPU of its own. A dedicated
virtio device carries the driver interface across the VM boundary:
**virtio-nvrm** in the guest (`virtio_nvrm.ko`) owns the NVIDIA device
nodes and forwards open/ioctl/mmap; **vhost-user-nvrm** on the host is
itself the vhost-user device attached to cloud-hypervisor and executes
the calls against the real driver — NVIDIA's Resource Manager ("RM"),
the kernel driver behind `/dev/nvidiactl` and `/dev/nvidiaN`.

A second guest kernel module (`guest-module/nvrm_nodes/`) supplies what
CUDA as a normal user needs beyond forwarding: real device nodes,
`/proc/driver/nvidia/params`, VA->GPA (guest-virtual to guest-physical) translation in the kernel.

The road here was gated stage by stage: the translation was first proven
across a process boundary (LD_PRELOAD shim -> Unix socket -> daemon:
`nvidia-smi`, `nvprobe` stages 0-3 and PyTorch all ran that way), then
across the VM boundary on an interim virtio-gpu carrier -- all of those
gates passed before they were retired. Shim and interim carrier were
removed on 2026-08-04, and their gates went with them. The gate since is
`test.sh gpu`:

| Stage | Content | Gate |
|---|---|---|
| `tables` | device + driver, hello, descriptor tables | **PASS** |
| `smi` | open/ioctl/close -- `nvidia-smi` without LD_PRELOAD | **PASS** |
| `memory` | RM mmap through the host-visible window (the shared-memory region the host places guest mappings into) | **PASS** |
| `kernel` | UVM pool with the guest's own pages (UVM is NVIDIA's unified-memory driver) + OS-descriptor pinning (memory RM pins for the GPU: the `cudaHostRegister` path) | **PASS** |
| `torch` | PyTorch, bit-identical to the reference run | **PASS** |
| `robustness` | concurrency, SIGKILL without a leak, `rmmod` refused | **PASS** |
| `own` | the VM's own process list, under guest PIDs | **PASS** |
| `encode` | NVENC/NVDEC: 1080p h264 encoded and hardware-decoded in the guest | **PASS** |

All of it in one command: `scripts/test.sh gpu`. The full sequence from a
fresh checkout to `nvidia-smi` in a guest is in
[`DEVELOPMENT.md`](DEVELOPMENT.md).

Several VMs share the one GPU (`scripts/showcase.sh up --count 4`).
Measured: four `convburn` runs at once, 95.4 ms/it each against
24.4 ms/it alone -- **3.9x at 4 VMs, i.e. fair time-sharing** -- and all
four results bit-identical (`acc=1.065710664e+00`).

They do not have to be the same KIND of guest. Measured 2026-08-15: one
guest played CS2 on the virtual display while a second ran PyTorch on the
same RTX 2070. Both stayed correct -- `convburn` bit-identical to its
solo run, the display path unmoved at 60.1 FPS -- and the price of
sharing was 15-20 % of throughput, not correctness. `nvidia-smi` on the
host shows the two backends side by side.

### Display

With the display path on, a guest runs a full desktop on the GPU:
`scripts/showcase.sh up --name desktop --index 5 --session gnome --with-steam`
brings up GNOME on the RTX 2070 and Sunshine beside it, and CS2 plays over
Moonlight at **55-60 FPS**, at the virtual display's 60 Hz cap rather than
the GPU's. `scripts/showcase.sh pair` does the Moonlight pairing without a
browser -- once per host per guest. The gate is `scripts/test.sh display` (12 stages, roughly ten
minutes, needs X and Sunshine in the guest and Moonlight on the host).
Beside it sits `scripts/test.sh vdisplay`, the fast one: six stages from
module load to a frame written and read back, without an X server, Vulkan
or streaming -- the gate to run after a refactor rather than before a
release. Both run from `scripts/test.sh gates`. This half is newer and
thinner than the compute
path -- [`docs/DISPLAY.md`](docs/DISPLAY.md).

WARNING: the guest currently runs **both** modules: `nvrm_nodes.ko
create_nodes=0` provides `/proc/driver/nvidia/params`, `virtio_nvrm.ko`
owns the nodes and the forwarding (rationale: `docs/OPEN-QUESTIONS.md` no. 2).

## Limitations

Read this before trying it. None of the following is a bug to be reported;
each is a known boundary of the current design.

- **The driver version must match exactly.** Guest `libcuda` and the host's
  `nvidia.ko` have to be the same version (this tree targets the one in
  `DRIVER_VERSION`). NVIDIA gives no ABI stability guarantee for the `ioctl`
  structs, so a mismatch does not announce itself as an error -- it shows up
  as misinterpreted struct offsets. `scripts/build.sh check-driver` and
  `nvrm_sys::assert_driver_version()` exist to make that failure loud.
  NVIDIA's userspace is not redistributable, so the guest must fetch the
  matching version itself; see [`LICENSES.md`](LICENSES.md).
- **Turing is the only architecture ever run** — see the disclaimer at the
  top. Treat a run on anything else as an experiment.
- **One GPU.** Multi-GPU guests are not implemented.
- **Rendering works, and it is younger than the compute path.** A guest
  with the display path on runs GNOME on the GPU and plays CS2 over
  Moonlight at 55-60 FPS, capped by the virtual display's 60 Hz rather
  than by the GPU. That is new (2026-08-15) and it is measured, not
  designed-for: the compute path has gates behind it, the display path has
  one long session. Treat it as the newer half.
  [`docs/DISPLAY.md`](docs/DISPLAY.md) has the stages and the numbers.
- **Managed memory is partial and opt-in, and it does not live in VRAM.**
  `LEA_MANAGED_COMPAT=1` on the *host* enables it. The pages stay pinned in
  the guest's RAM, registered with the GPU as system memory (`LOCATION=PCI`)
  and reached **over the bus on every access** -- they never migrate to the
  card. That is why the compat path can honestly answer "nothing to migrate".
  Consequences: bandwidth is the PCIe link rather than VRAM, the allocation
  costs guest RAM rather than card memory, and oversubscription -- more
  managed memory than there is VRAM -- does not work and fails visibly
  rather than computing the wrong answer.
- **Pinned host memory is capped by TWO limits, and the smaller one is on
  the host.** `cudaHostRegister` past either returns 304
  (`cudaErrorOperatingSystem`), which reads like a refusal.

  | limit | where | default | shape |
  |---|---|---|---|
  | `max_pin_mib` | guest module | 1024 MiB | total across all pins |
  | `LEA_MAX_PIN_MIB` | host backend | **256 MiB** | **one pin** |

  Measured 2026-08-16: a single 512 MiB pin fails with the guest limit
  at its 1024 MiB default and only 10 MiB pinned, because the HOST refuses
  one arena over 256 MiB -- `0x71 arena: arena length 536870912 over the
  pin limit 256 MiB` in the backend log, which is the only place the two
  are told apart. Raising the guest's `--max-pin-mib` alone therefore
  changes nothing for a large single pin; raise `LEA_MAX_PIN_MIB` on the
  backend as well -- verified the other way round, with
  `LEA_MAX_PIN_MIB=1024` the same 512 MiB pin succeeds with a correct
  roundtrip and no refusal in the log. A single very large pinned allocation can additionally
  fail on fragmentation, because its GPA runs must fit the negotiated aux
  buffer (the per-request side buffer that carries a mapping's page-run
  list).
- **The guest sees its own processes, and a mediated card.**
  `nvidia-smi` in the VM lists the guest's processes under their guest PIDs.
  The card is always named `Leandro <model>` — nobody should be able to sit
  in one of these VMs without noticing (`LEA_GPU_NAME_RAW=1` on the backend
  keeps the driver's own string) — and with `LEA_VRAM_LIMIT_MIB` set it
  reports the cap and the profile size joins the name, `Leandro
  <model>-<size>`, after NVIDIA's own vGPU convention (the display name
  only — the env prefix stays `LEA_*`). The per-process
  number is the host's bookkeeping of explicit allocations, so it is larger
  than `torch.cuda.memory_allocated()` and smaller than the true footprint.
- **The VRAM cap is opt-in, and it is enforced on the host.** Without
  `LEA_VRAM_LIMIT_MIB` on a VM's backend, overcommit stays
  first-come-first-served and one guest can exhaust the card for the
  others. With it set, that VM's device-memory allocations are charged
  against the cap and refused as an ordinary out-of-memory, and the same
  number is advertised to everything in the guest — `nvidia-smi` and
  Vulkan report the capped card, its used and its free from one ledger.
  Host-side deliberately: a cap inside the guest is a hint, because a
  guest that wants to overcommit can load its own module.

  Measured 2026-08-15 under a 4096 MiB cap: the books track the card to
  within 214 MiB, and that remainder is the device memory RM allocates
  itself behind a channel (context buffers, USERD — a channel's doorbell page), which never crosses
  the boundary as a request. Managed memory is not counted either — it is
  pinned guest RAM rather than VRAM, bounded by the pin limits instead.

  What the cap mostly does is not refuse. CS2 on an 8 GiB card takes
  4.8 GB when nothing caps it and 3.1 GB under a 4 GiB cap **without a
  single allocation being refused**: told the truth about what is left, a
  streaming engine sizes itself to it. The refusal path exists and no
  measured workload has reached it yet.
- **No snapshot, suspend, resume or migration.** That is an open
  construction site on the cloud-hypervisor side for vhost-user devices,
  not something this project can decide alone.
- **The event back-channel is built, and it is what made rendering
  usable.** A second virtqueue carries RM's event firings host -> guest, so
  a wait is woken rather than polled: `fencetime` fell from 10.10 ms to
  0.12 ms and Sunshine's frame time from 62 ms to 6 ms. Measured under
  load: 43 000 events/s with a game and a live stream together, delivered
  without a single ring-full drop. The four drop reasons each have their
  own counter under
  `/sys/module/virtio_nvrm/parameters/stat_events_drop_*`.
- **The cloud-hypervisor fork is required.** Two patches, and one of
  them is what CUDA needs: `patches/0001-generic-vhost-user-shmem.patch`
  adds SHMEM support to the generic vhost-user device — without it the
  device comes up and ioctls work, but memory mapping does not, so no
  CUDA. The second (`0002-generic-vhost-user-device-features.patch`)
  passes device-specific feature bits through and has no effect on
  virtio-nvrm, by construction.

## Roadmap

### Built while the thesis runs

These were committed as the three things MeisterStack would need from this
project to treat it as a component rather than a rig. Two have landed:

| | Status |
|---|---|
| **NixOS package and module** | **Done.** The flake packages the host side and exports `nixosModules.default` (host: backend, bridge, taps, NAT, persistence) and `nixosModules.guest`; `nix/guest-image.nix` builds a bootable, pre-provisioned guest image as a derivation. [`DEVELOPMENT.md`](DEVELOPMENT.md) section 10. |
| **SLURM, with reproducible tests** | **Done.** `build.sh package` produces a self-contained Apptainer package; `bench.sh slurm` packages, submits (a real `sbatch`), runs and collects gates and benchmarks as SLURM jobs. Verified on single-node SLURM on 2026-08-19 — [`DEVELOPMENT.md`](DEVELOPMENT.md) section 9a. |
| **A deployment story** | **Open, and smaller than it was.** The flake module and the Apptainer package are most of it; what is missing is the guided path a third party can follow without reading `DEVELOPMENT.md` end to end. |

**A rule that came with the SLURM work, and it is now in force rather than
an aspiration: no benchmark enters this repository until it is
reproducible.** Every number here today was measured on one rig, by one
person, and is reported as such — which is honest but is not a benchmark.
A benchmark is a claim somebody else can check, so a new performance claim
waits until the run is expressed as a job another person can submit, and it
ships **with its evidence**: the job definition, the raw output, and the
conditions it was taken under. Numbers that cannot meet that stay labelled
as single-rig measurements.

### Important, beyond the thesis

| | Why it matters |
|---|---|
| **Multiple driver versions** | Everything is pinned to one version (`DRIVER_VERSION`), enforced by a panic. That is honest but restrictive: guest and host must match exactly. Supporting a set of versions means the descriptor tables become version-indexed rather than version-locked. |
| **More architectures** | Only Turing has ever run this. Ampere, Ada and Blackwell are in the class tables and untested on silicon. Every one of them is a real risk of finding a wrong assumption. |

Everything else that is unbuilt, with sketches of how it could be built, is
in [`docs/FUTURE.md`](docs/FUTURE.md).

## Setup

```sh
git clone https://github.com/UniStuttgart-IKR/Leandro && cd Leandro
./scripts/build.sh --driver auto  # fetches, patches and builds everything
./scripts/showcase.sh net up      # bridge, taps, NAT (sudo; runtime state)
./scripts/showcase.sh demo --fast # the guided demonstration
```

Nothing heavy is in git — a fresh clone is under 4 MB and `build.sh`
generates the rest (NVIDIA headers, the patched cloud-hypervisor, the guest
image). On NixOS: `nix develop` first, or import the flake's
`nixosModules.default` and let it declare the network
([`DEVELOPMENT.md`](DEVELOPMENT.md) section 10).

See [`DEVELOPMENT.md`](DEVELOPMENT.md) for prerequisites, the build, the
image bake and the step-by-step sequence.

The version match is not optional. Guest `libcuda` and host kernel
driver must match exactly; NVIDIA gives no ABI stability guarantee for
the `ioctl` structs. A mismatch does not show up as an error but as
misinterpreted struct offsets.

## Quick start: two VMs on one GPU, one of them capped

After `build.sh`. Every step is idempotent, and every guest is an
*instance* with a name, an index and its own directory under `vm/` — which
is what makes the two VMs *differ*: the VRAM cap belongs to a backend, and
each `up` starts exactly one backend for exactly one VM.

```sh
./scripts/showcase.sh net up                       # bridge, taps, NAT

# One backend per VM -- a backend serves exactly ONE VM connection. `up`
# starts it, boots the guest, provisions it (userspace, probes,
# nvrm_nodes.ko) and builds and loads virtio_nvrm.ko in it. On a disk that
# has never booted this takes a few minutes (build tools and kernel headers
# are installed in the guest); afterwards it is seconds.
./scripts/showcase.sh up                           # vm0: index 0, IP .10, uncapped
./scripts/showcase.sh up --name b --index 1 --vram-limit 2048   # capped at 2 GiB
```

`nvidia-smi` is on the guest's `PATH` (the provisioning links the payload
into `/opt/nvrm` and runs `ldconfig`), so neither wrapper nor
`LD_LIBRARY_PATH` is needed — `~/gpu/nv/bin/nvidia-smi` is the same binary
addressed directly:

```sh
./scripts/showcase.sh ssh nvidia-smi
./scripts/showcase.sh ssh --name b '~/gpu/nv/bin/nvidia-smi'
```

Two guests, one card, and the cap is the whole difference between them:

```
|   0  Leandro RTX 2070               On  |   00000000:2D:00.0  On |          N/A |
| 55%   50C    P8             29W /  175W |    1040MiB /   8192MiB |      Default |

|   0  Leandro RTX 2070-2G            On  |   00000000:2D:00.0  On |          N/A |
| 55%   50C    P8             29W /  175W |       0MiB /   2048MiB |      Default |
```

Both guests see a mediated card (the vendor prefix gives way to the project
name, always); the capped one additionally reports the cap and carries the
profile size in its name, after NVIDIA's own vGPU convention. That the card
is real, in a VM that owns no GPU:

```sh
./scripts/showcase.sh ssh --name b 'cd ~/gpu && ./nvprobe 3'
#   stage 3 ok (kernel, result correct)
```

Log in with `./scripts/showcase.sh ssh --name b`. `down` tears a VM down
first and its backend second — a dying backend takes its cloud-hypervisor
with it, so the order is not negotiable and the script owns it:

```sh
./scripts/showcase.sh down --all
```

For the workloads — PyTorch bit-identical to the host, managed memory,
`kill -9` mid-run, several VMs at once — the guided demonstration runs them
and prints the real output of each:

```sh
./scripts/showcase.sh demo --fast   # ~3 minutes; --full for everything
```

## Components

Every crate and both kernel modules document themselves next to their own
code. The map, with one line each, is in
[`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md#5-the-components-and-where-each-is-documented):

| | |
|---|---|
| Host | [`nvrm-sys`](crates/nvrm-sys/README.md) · [`nvrm-abi`](crates/nvrm-abi/README.md) · [`nvrm-wire`](crates/nvrm-wire/README.md) · [`nvrm-client`](crates/nvrm-client/README.md) · [`nvrm-trace`](crates/nvrm-trace/README.md) · [`vhost-user-nvrm`](crates/vhost-user-nvrm/README.md) · [`vhost-user-input`](crates/vhost-user-input/README.md) |
| Guest | [`virtio_nvrm`](guest-module/virtio_nvrm/README.md) · [`nvrm_nodes`](guest-module/nvrm_nodes/README.md) |

The module's C header (`nvrm_wire.h`) is **generated**, not maintained:
`cargo run --release --bin nvrm-genhdr -- guest-module/virtio_nvrm/nvrm_wire.h`
casts the Rust wire structs into C, with a `_Static_assert` per field
offset. The guest therefore needs no Rust toolchain, and a layout
divergence shows up when the module is built instead of at runtime
(`--check` says whether the header is current -- the GPU gate asks that
question first).

## Using the tracer

`nvrm-trace` is an `LD_PRELOAD` tracer that observes the ioctl surface
without changing it — the instrument most of the measurements in this repo
came from. It is not part of the data path.

```sh
cargo build --release -p nvrm-trace
LEA_TRACE_FILE=traces/vectoradd.tsv \
  LD_PRELOAD=$PWD/target/release/libnvrm_trace.so ./vectorAdd
strace -f -e trace=ioctl -c ./vectorAdd     # the cross-check
```

Details and the rules that apply on the interposed path:
[`crates/nvrm-trace/README.md`](crates/nvrm-trace/README.md).

## Acknowledgements

**NVIDIA's [open-gpu-kernel-modules](https://github.com/NVIDIA/open-gpu-kernel-modules).**
This project exists because that source is published. Every struct layout,
every escape number (an "escape" is RM's name for one of its ioctl
commands) and every status code here was read out of those headers
rather than guessed, and `nvrm-sys` generates its bindings straight from
them at the pinned version. Without them this would be blind reverse
engineering instead of engineering.

**gVisor's [nvproxy](https://github.com/google/gvisor).** The prior art that
showed this class of interception works at all. Its `pkg/abi/nvgpu` is what
`crates/nvrm-abi/src/nvgpu.rs` is cross-checked against — transcribed, not
copied — and its object model, in particular that RM's `Free` is transitive
through dependency edges the headers do not state, is knowledge taken
directly from it.

**Large language models.** Claude Opus, Claude Fable, Claude Sonnet and
Gemini 3 Pro wrote most of what is here. Saying so is not modesty, it is
part of the disclaimer at the top: it is why the code has not had a
line-by-line human review yet.

**And the human part, stated plainly, because "AI wrote it" invites the
wrong conclusion.** This took weeks, not an afternoon. The models wrote the
code; what took the time was measuring every claim against a real card,
counter-checking diagnoses that looked obviously right and were not,
sizing rings by breaking them, and walking dead ends far enough to know
they were dead. Several sections of [`docs/llm.md`](docs/llm.md) exist only
because a plausible explanation survived until somebody looked at the
screen and saw the gears standing still. That part does not automate.

## Contributing

The project is semi-active until the thesis is done (see the disclaimers at
the top), so expect slow responses. That said: a run on an architecture
other than Turing, on a different driver version, or on a different
hypervisor is genuinely the most useful thing anyone could contribute —
those are exactly the assumptions nobody here has been able to test.

> [!NOTE]
> If you are researching the use of LLMs to build larger software
> projects: the major prompts from before the publication refactoring and
> the development journal the agents kept are being prepared for
> publication (they need a pass so nothing private leaks). Work in
> progress — the link will be added here when they are out.

`scripts/test.sh check` runs anywhere and needs no GPU. Start there.
