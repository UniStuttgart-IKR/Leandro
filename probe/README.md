<!-- SPDX-License-Identifier: MIT -->
# probe/ — measurement tools and the manual paths

This directory holds the ioctl probes and the host-local experiments. This
README is also the **manual for running each path by hand** — not just the
gate scripts, but the mechanics behind them, so that a failure tells you
where you are.

The automated checks and the measurement discipline are in
`../docs/TESTING.md`. The results and their derivations are recorded in
`../docs/OPEN-QUESTIONS.md` — evidence, not documentation.

Ground rules that apply everywhere:

- **Driver lockstep is not optional.** Every binary checks
  `/proc/driver/nvidia/version` against `DRIVER_VERSION` at startup. Under
  a different driver every struct offset is a guess →
  `./scripts/build.sh check-driver`.
- **Measure first, then build. Counter-check before any diagnosis.** A
  non-zero status does not mean this project is at fault (known-benign
  codes below).
- Say what a statement rests on: "Measured:" for something measured or
  read in the driver source, "Unverified:" for a conjecture, "WARNING:"
  for a trap.

---

## 0. One-time prerequisites

```sh
cd ..                                        # repository root
./scripts/build.sh check-driver              # lockstep: is the pinned driver running?
./scripts/build.sh vendor                    # open-gpu-kernel-modules headers (headers only)
./scripts/build.sh cargo                     # all crates
cd probe && make                             # nvprobe + kernels.ptx (needs nvcc + libcuda)
```

`make` builds `nvprobe` and `kernels.ptx`. `make probes` builds the other
CUDA probes, `make host-tools` the two GPU-free readers, and any single
one is `make bin/<name>` (the binaries live in `bin/`, not versioned);
`./scripts/build.sh probes` is the sanctioned wrapper. The tool table at
the end says what each is for.
`kernels.ptx` is deliberately PTX rather than cubin: the JIT is part of what
must run unchanged in the guest later, and precompiling here means not
measuring it.

For the VM additionally (takes a while, downloads images and sources):

```sh
cd ..
./scripts/build.sh image                     # Ubuntu cloud image + kernel/initrd (pins in GUEST_IMAGE)
./scripts/build.sh ch                        # cloud-hypervisor @ CH_VERSION + patch series, release build
```

There are two ways to exercise the stack by hand, each with a hard,
reproducible criterion — and a third that is history:

| Path | What it shows | Carrier |
|---|---|---|
| The tracer | The tracer sees *exactly* the ioctls strace sees | LD_PRELOAD tracer |
| The VM | One host daemon serves a **VM** and its guest driver | cloud-hypervisor + `virtio_nvrm.ko` |
| (retired) The bare channel | The submission path is pure memory plus one MMIO write, no ioctl | native host process |

A fourth path existed and is retired: a forwarding daemon over a Unix
SEQPACKET socket with an LD_PRELOAD guest library, host and guest in the
same kernel. It and the virtio-gpu carrier were removed on 2026-08-04.
Where the documentation still mentions them, it is describing what was
measured on them, not something you can run.

---

## 1. The bare channel — retired

The first stage built a GPFIFO channel by hand, pushed a pushbuffer with a
single semaphore release into it and rang the doorbell, to show directly
that submission costs no ioctl. **The crate that did this is no longer in
the tree**, so there is nothing here to run; the result it established is
criterion K2 in [`../docs/VIRTIO-UAPI.md`](../docs/VIRTIO-UAPI.md).

---

## 2. The tracer probes (native, under LD_PRELOAD)

The tracer (`crates/nvrm-trace`) attaches to an unmodified CUDA application
via LD_PRELOAD and logs every ioctl/mmap/poll. Three probes:

| Tool | measures |
|---|---|
| `nvprobe.c` | raw CUDA driver API, stages 0–4 (cuInit / context / memory / PTX+kernel / 100 long kernels) |
| `torchprobe.py` | the full PyTorch stack, stages 0–5, `NVTORCH_CONV=1` pulls in cuDNN |
| `rlprobe.py` | RL workload (CPU env, GPU policy), stages 0–4, boundary crossings per env step |

One runner, `probe/run/trace.sh`, with one subcommand per workload:

```sh
./scripts/build.sh cargo                          # the tracer
probe/run/trace.sh nvprobe                        # nvprobe, lvl0..lvl4blocking
NVTORCH_PY=~/.venvs/torch/bin/python \
  probe/run/trace.sh torch                        # torch0..torch5 + torch5conv
probe/run/trace.sh analyse                        # saturation curve over all traces/*.tsv
```

Each stage produces `probe/traces/<tag>.tsv` (tracer) **and**
`probe/traces/<tag>.strace` (counter-check without the tracer); `--out DIR`
moves both, and `analyse DIR` reads them back.

**Criterion:** every run subcommand prints a summary table at the end; the
**delta column must be 0** — the tracer sees exactly the ioctls strace sees.
A delta ≠ 0 means somebody is bypassing the PLT, and from that moment on
every further measurement is decoration. The delta counts strace lines
carrying `_IOC`, because under Python every `isatty` is a `TCGETS2` that is
an ioctl but not an NVIDIA one — counting them made a tracer that missed
nothing look 1062 calls short (measured 2026-08-18 on `torch0`).

`trace.sh smi` does the same for `nvidia-smi`, which goes through NVML and
not through libcuda: its escape surface can contain escapes and controls
that appear in no libcuda or torch trace. It writes `traces/smi.tsv` and
`traces/smi-q.tsv` (the `-q` path pulls more controls) next to the other
traces so that `analyse` picks them up, and it lists the signatures that are
new against `lvl4blocking.tsv` when that run is present.

PyTorch ships cuBLAS/cuDNN in its wheel but loads the **system** libcuda —
which is exactly what is to be measured. Check once:

```sh
~/.venvs/torch/bin/python -c "import torch; torch.cuda.init(); \
  print(next(l for l in open('/proc/self/maps') if 'libcuda.so' in l))"
```

The path must point at the libcuda of `DRIVER_VERSION`.

Switches: `NVPROBE_CYCLES`, `NVPROBE_ITERS`, `NVPROBE_SCHED`, `NVPROBE_PTX`,
`NVPROBE_NOCLEANUP`, and `NVTORCH_ITERS`, `NVTORCH_CONV`,
`NVTORCH_NOCLEANUP`. Teardown is **deliberately explicit**: the
`NV_ESC_RM_FREE` order in the trace is the source of the dependency edges in
`nvrm-client`; on a plain `exit()` RM's client teardown cleans up and
nothing is visible.

### rlprobe

```sh
~/.venvs/torch/bin/pip install numpy         # missing from the torch venv
~/.venvs/torch/bin/python rlprobe.py                 # stage 4, 50 episodes
NVRL_STAGE=3 ~/.venvs/torch/bin/python rlprobe.py    # only up to the training step
NVRL_PIN=1   ~/.venvs/torch/bin/python rlprobe.py    # obs over the pinned/0x71 path
```

Switches: `NVRL_STAGE` (0–4), `NVRL_EPISODES`, `NVRL_PIN`, `NVRL_NONBLOCK`,
`NVRL_SYNC`, `NVRL_ITEM`. Native reference: 50 episodes ≈ 1.4 s, ~2000
boundary crossings/s (H2D = D2H). Deterministic: the stage-3 `loss` is
reproducible. Run it under the tracer for the managed-memory question:
`LD_PRELOAD=…/libnvrm_trace.so … python/rlprobe.py`, then
`python/uvmtax.py`.

---

## 3. The VM (virtio-nvrm)

Here the guest is a real VM. `vhost-user-nvrm --nvrm <socket>` is the
virtio-nvrm device backend for cloud-hypervisor; in the guest,
`virtio_nvrm.ko` owns the `/dev/nvidia*` nodes and forwards every ioctl
over the device. CUDA in the guest runs **as a normal user, without
LD_PRELOAD and without a wrapper**.

WARNING: one `vhost-user-nvrm` backend serves exactly **one** VM
connection. If it dies while the VM is running, cloud-hypervisor exits
immediately — which looks like a guest crash and is not one.

### The short way: a persistent VM with a GPU

`showcase.sh up` keeps a VM up that is reachable over SSH; its instance
disk is a **persistent** qcow2 overlay (`vm/vm0/rootfs.qcow2`), so
installed packages (python3.14, torch) survive a restart of the VM. One
`up` is the whole path: a fresh backend for this VM, the VM, the guest
userspace and probes, `nvrm_nodes.ko`, and `virtio_nvrm.ko` built and
loaded in the guest.

```sh
cd ..                                        # repository root
./scripts/build.sh cargo
./scripts/showcase.sh net up                 # once per boot: bridge, taps, NAT
LEA_DEBUG=1 ./scripts/showcase.sh up         # LEA_DEBUG reaches the backend
./scripts/showcase.sh ssh                    # log in (leandro@192.168.100.10)
```

What `up` does, in order (every step is a function in `scripts/lib/`):

- starts `vhost-user-nvrm` for this instance (`vm/vm0/nvrm.sock`), then
  boots the VM with the virtio-nvrm device and waits until SSH answers.
  Login user **leandro** (password `leandro`, or key-less via
  `vm/id_leandro`, which is generated on first start). The guest gets a
  **persistent** netplan file from cloud-init (static IP, applied on every
  boot), a real `ssh.service` (instead of socket activation, which refuses
  connections after a restart) and a DNS fix.
- provisions the guest (`lea_guest_setup`): NVIDIA userspace, the probes
  from `probe/`, `params.txt` from the host's `/proc/driver/nvidia/params`,
  `nvrm-setup.sh`, and `nvrm_nodes.ko`. It installs `build-essential` and
  the kernel headers if the image lacks them. Idempotent; the payload is
  re-sent only when it changed, the torch venv is left alone
  (`--with-torch` creates it).
- builds and loads the module (`lea_guest_build_nvrm`): copies
  `guest-module/virtio_nvrm` into the guest, builds it there and
  establishes the coexistence: `nvrm_nodes.ko` loaded with `create_nodes=0`
  (it supplies `/proc/driver/nvidia/params`), `virtio_nvrm.ko` on top (it
  owns the nodes and the forwarding). **Both** modules are needed — without
  the first, `params` is missing.

In the guest, `~/gpu/nvrm-setup.sh --persist` makes that state survive a
reboot; with `virtio_nvrm.ko` present it persists the coexistence.

Options of `showcase.sh up` (the header of the script lists them all):

| Option | Effect |
|---|---|
| `--fresh` | disk **and** seed rebuilt — discards all state, first boot |
| `--console` | cloud-hypervisor interactive on this terminal instead of in the background (Ctrl-C stops; no provisioning) |
| `--cpus N` `--mem MiB` | default 4 / 4096 |
| `--name <n> --index N` | a further instance beside `vm0` (own directory `vm/<n>/`, IP `.10+N`, `tapN`) |
| `--count N` | a fleet `vm0..vm<N-1>` |
| `--display`, `--session gnome\|openbox` | the virtual display; a desktop and Sunshine on it |
| `--vram-limit MiB`, `--max-pin-mib N` | the backend's VRAM cap; the guest module's pin cap |
| `--no-provision`, `--no-load` | skip the guest setup / the module build |

`showcase.sh down` powers the guest off gracefully and stops its backend;
`showcase.sh ssh [cmd…]` logs in or runs one command; `showcase.sh status`
lists every instance.

Networking prerequisite: an existing, bridged tap owned by the invoking
user (default `tap0` on `br-poco`, 192.168.100.1/24, NAT). `showcase.sh
net up` creates them (sudo) and `up` brings them **up** again on every
start.

WARNING: **Always use `showcase.sh down`, never kill the VM hard.** A hard
abort loses unsynced writes — among them the *contents* of the SSH host
keys, which are then left behind as 0-byte files and stop sshd from
starting on the next boot (symptom: `Connection refused` although the VM is
running). `down` therefore shuts down via `poweroff`. If it happens anyway,
`showcase.sh up --fresh` rebuilds the disk.

Installing something permanently, for example python3.14 and torch:

```sh
./scripts/showcase.sh ssh 'sudo add-apt-repository -y ppa:deadsnakes/ppa && \
  sudo apt-get update && sudo apt-get install -y python3.14 python3.14-venv'
./scripts/showcase.sh down         # syncs and shuts down -> survives
```

### Several VMs on one GPU

One `vhost-user-nvrm` per VM, each on its own socket, one directory per
instance. `showcase.sh up --name <n> --index N` runs a further instance
beside `vm0`; `showcase.sh up --count N` a homogeneous fleet, whose members
1..N-1 are thin overlays on `vm/base-torch.qcow2` — a frozen copy of a
fully provisioned dev disk that must never be written again:

```sh
./scripts/showcase.sh down                   # vm0 must be stopped while its disk is copied
cp vm/vm0/rootfs.qcow2 vm/base-torch.qcow2
./scripts/showcase.sh up --count 4
```

Measured: 4 VMs share the GPU fairly (3.9x runtime at 4x `convburn` —
95.4 against 24.4 ms/it, docs/llm.md — and the results are
bit-identical); VRAM is first come, first served, with a
clean OOM for the latecomer.

### Limits and switches on the memory path

`LEA_MAX_PIN_MIB` (host backend, default 256) limits how much guest RAM a
single arena may pin. It is a plausibility barrier against a lying guest,
not a technical limit — 768 MiB arenas carry fine. WARNING: torch's pinned
allocator rounds up to powers of two, so 257 MiB becomes a 512 MiB request.

`LEA_MANAGED_COMPAT=1` (host backend) enables managed-memory support;
`managedprobe` needs it, nothing else notices it.

`LEA_DEBUG=1` gives the backend a log; `LEA_DEBUG=2` logs **every** ioctl,
which is what sequence diffs against a direct trace are made from — and it
is expensive enough that no wall-clock measurement may run alongside it.

WARNING: guest CUDA runs as a normal user only because the kernel module
resolves guest VAs into guest physical addresses. Reading PFNs out of
`/proc/self/pagemap` in userspace requires root; a non-root reader gets PFN
0, hence wrong addresses, and the kernel then computes **consistently 0**
while cuInit, context and memory all still succeed — a wrong result that
looks like a working stack.

---

## 4. The host-local experiments (no guest, no VM)

These settle architecture questions in seconds on the host.

```sh
cd ..
# E1: OS descriptor -> UVM CREATE/MAP_EXTERNAL at a freely chosen GPU VA.
#     Last line: "E1: PASS - the chain carries".
cargo run --release -p nvrm-client --bin e1-extmap

# E2: does libcuda tolerate the UVM sharing mode? The shim sets ONLY flag 0x2.
cd probe && make bin/uvminit_shim.so
LD_PRELOAD=$PWD/bin/uvminit_shim.so ./bin/nvprobe 3   # "stage 3 ok" => the mode is harmless
```

E1 is the chain the semaphore pool rides on: writable guest pages are
registered with RM as an OS descriptor and attached via UVM
`CREATE`/`MAP_EXTERNAL` to a chosen GPU VA — the same GPU VA the guest sees
(libcuda places the pool at `0x204a00000`). E2 isolates the one question
the driver source cannot answer: does libcuda cope with
`NV_WARN_NOTHING_TO_DO` from `UVM_INITIALIZE` when flag 0x2
(`uvm_types.h:67`) is set?

---

## 5. Trace format

Line formats (details in `../crates/nvrm-trace/src/log.rs`):

    ioctl     dev  nr  sub  size  psize  ret  status  fd   (9 fields, nr in hex)
    mmap      dev  fd  len  off  addr
    open      dev  fd
    read      dev  fd  ret
    poll      dev  fd  revents    <- one line PER FD in the array, not per syscall
    eventreg  fd   before

Plus diagnostic key=value lines -- `nvos02`/`nvos32`/`nvos33`/`nvos46`/
`nvos64`/`memparams`/`uvminit`/`uvmpma`/`uvmreg`/`cardinfo`/`ctrlout`
(payload after the call) and the same with suffix `in` (the input before
it); log.rs's header is the authoritative list.

WARNING: **counting trap:** `grep '^nvos64'` also matches `nvos64in` and
counts everything twice. Filter correctly with
`awk -F'\t' '$1=="nvos64"'`.

### One-liners for evaluation

```sh
# signatures (dev, nr, sub) of one run
sig() { awk -F'\t' '$1=="ioctl"{print $2, $3, $4}' "$1" | sort -u; }

# what run B adds in surface over run A
comm -13 <(sig traces/lvl4blocking.tsv) <(sig traces/torch5.tsv)

# ioctls per step in steady state (differential measurement)
a=$(grep -c '^ioctl' traces/torch4.tsv); b=$(grep -c '^ioctl' traces/torch5.tsv)
echo "scale=2; ($b - $a) / 100" | bc

# count alloc classes (hClass table)
awk -F'\t' '$1=="nvos64"' traces/torch5.tsv \
  | grep -o 'hClass=0x[0-9a-f]*' | sort | uniq -c | sort -rn

# resolve UVM commands (managed-memory question) / taxonomize mmap regions
python python/uvmtax.py traces/torch5.tsv
python python/maptax.py traces/torch5.tsv
```

---

## 6. Known-benign status codes (not this project)

Counter-check these before any fault diagnosis — they occur in the direct
run as well:

| ioctl / cmd | status | Context |
|---|---|---|
| `0x2080012f`, `0x20800157` | `0x56` | GPU control under load |
| `0x2080014b` | `0x57` | " |
| `0x20800146` | `0x63` | " |
| `UVM_MM_INITIALIZE` (75) | `0x10006` | `NV_WARN_NOTHING_TO_DO` — the sharing mode, expected |

---

## 7. The per-library coverage matrix

Sections 2 to 4 answer "does this path work". This one answers a different
question: **for every NVIDIA library the guest is handed, which ioctls does
it emit, and would the backend carry them?** Nobody could say, because most
of those libraries had no probe at all.

```sh
cd ..
./scripts/ioctl-matrix.sh all          # discover, probes, trace, catalogue
./scripts/ioctl-matrix.sh trace vk-rt  # one probe, re-measured
./scripts/ioctl-matrix.sh guest        # the same probes in a VM, against those traces
./scripts/ioctl-matrix.sh verify       # the answer bytes of the two runs
```

It produces the artefacts under `../matrix/`, all regenerated and none
hand-maintained: `DISCOVERY.md` (what the pipeline is built on, including
both library inventories and their difference), `PROBES.md` (one row per
feature path), `catalog-<driver>.md`/`.json` (one row per
`(device, nr, sub)` signature with its name, description, header reference,
parameter size, status and mediation flags), and `MATRIX-<driver>.md` plus
`TASKS-<driver>.md`.

Three things about it are worth knowing before reading its output.

**Every probe has a criterion beyond its exit code**, printed as a
`CRITERION:` line. A missing library fails with `ENOENT` before any ioctl is
issued, so the tracer sees nothing and an empty trace looks like a feature
nobody used. "Denied", "absent" and "silently degraded" are three findings
and only a checked result tells them apart — the EGL probes read a cleared
pixel back out of a pbuffer for exactly that reason.

**The counter-check is narrower than section 2's.** `_IOC` as a substring
also matches `DRM_IOCTL_VERSION`, so on a graphics workload the old rule
reports hundreds of phantom missed calls; and the tracer's `sub` column
holds a runtime handle for `NV_ESC_RM_MAP_MEMORY`, so keying on it inflates
the signature count. Both are recorded as question 47, and this pipeline
counts `_IOC(` on `strace -y` fd paths and collapses 0x4e.

**The first four phases run nothing in a guest.** `MATRIX-<driver>.md` says
`predicted-green` for a probe whose every signature is governed or
passthrough, and that prediction comes from the descriptor tables, not from
a gate.

**`guest` is the phase that goes and looks.** It brings up (or reuses) a
rig, stages a miniature of this tree into it -- the probes, `scripts/lib`,
and the tracer itself -- and runs each probe there through
`run/matrix-guest.sh`, under the same two instruments and the same
counter-check. The comparison happens on the host
(`python/guestdiff.py`) and is deliberately narrow: the signature SET, and
the `rm_status` fingerprint per signature. Call counts are not compared
(a workload may allocate one surface more), DRM is not compared (a
different namespace and a different driver instance), and answer BYTES are
not compared -- that is still question 50. A probe that survives it is
`guest-validated` in the matrix, which is more than `predicted-green` and
less than `implemented-verified`.

Its first run is worth reading as an example of what the two extra columns
buy: `nvidia-smi -q` passed in the guest and printed a plausible report,
while two of its controls answered `INVALID_ADDRESS` and
`INVALID_ARGUMENT` where the native run answered `NV_OK` (question 51).
An exit code cannot see that; a fingerprint can.

**`verify` goes one level further down**, to the answer BYTES
(`python/answerdiff.py`). It logs nothing new either: the tracer's
`ctrlout` line has dumped the first 32 bytes of a control's answer since
the enumeration work, so both runs recorded the evidence already. The mask
— the hard half of question 50 — is derived rather than declared: a
differing word is allowed only if it is this side's own `gpu_id` from its
`cardinfo` line, or a handle this side allocated. Everything else that
differs is reported, and one unexplained word disqualifies the signature
even if it matched under another probe. Read question 55 before reading its
output: 67 signatures match and none of them is `implemented-verified`,
which is a statement about the reach of a 32-byte dump and about what
"verified" can mean for a command the backend answers itself.

The probes live in `matrix/` beside this file, one each, and the file IS the
registry: `PROBES.md` is generated by reading their headers, so adding a
probe is adding one file with a `# matrix-libs:` block in it.

## 8. Tool overview

| | |
|---|---|
| `nvprobe.c` / `torchprobe.py` / `rlprobe.py` | the three tracer probes |
| `run/trace.sh` | `nvprobe` / `torch` / `smi` run a workload under the tracer, `analyse` evaluates the traces |
| `uvminit_shim.c` | E2: sets only the UVM sharing flag |
| `maptax.py` / `uvmtax.py` | mmap and UVM command taxonomies |
| `ofdtest.c` `granttest.c` `forkprobe.c` | process-identity probes (OFD binding, DUP grant, fork) |
| `ioctlping.c` | round-trip latency per ioctl (`NV_ESC_CHECK_VERSION_STR`, no CUDA) — the transport figure; also runs as load `ioctlping` in `../scripts/bench.sh transport` |
| `ctrlping.c` | the same with growing payload: does the transport surcharge hang on payload size? |
| `managedprobe.c` | managed memory (`cuMemAllocManaged`) in stages, with a correctness check |
| `hostregprobe.c` | the pinned / host-registered (`hClass=0x71`) path |
| `oomprobe.c` | exhaust VRAM deliberately, check the error path and the recovery |
| `convburn.py` `convoom.py` `mmsweep.py` `pinwin.py` `streamprobe.py` `mtprobe.py` | torch-level loads used by the benchmark and isolation runs |
| `framecopy.py` `rfbshot.py` `vramcap.py` | display-frame copy cost; a VNC screenshot reader; drives the VRAM cap red and checks the refusal kind |
| `glinterop.c` `primeimport.c` `vkalloc.c` | GL-from-no-DRM-node interop, PRIME import in isolation, Vulkan memory-type probing |
| `run/drmtrace.sh` `run/suites.sh` | the DRM-surface tracer and the suite runner |
| `run/matrix-guest.sh` | the guest half of `ioctl-matrix.sh guest`: one matrix probe inside a VM, under the tracer and under strace, with the same counter-check |
| `python/guestdiff.py` | the host half: native trace against guest trace, signature set and rm_status fingerprint |
| `python/answerdiff.py` | one level down: the answer BYTES of the two runs, word by word, with a mask derived from the traces |
| `drmfd.py` | Sunshine's DRM-FD lookup for a CUDA device, step by step (ctypes, no toolkit) — says which step fails, guest and native |
| `../crates/nvrm-client` (`--bin e1-extmap`) | E1: the OS-descriptor → external-mapping chain, host-local |
| `../crates/nvrm-client` (`--bin mmapping`) | round trip per window mapping |
| `edid-verify.c` | reads an EDID block with a parser that shares **no** code with the module's builder — the independent half of the display gates' EDID check (`make host-tools`, no CUDA, no hardware) |
| `vdisp-frame.c` | writes one deterministic frame onto the virtual display's CRTC and reads it back through `GETFB2` + PRIME export; `--reference WxH` prints the expected hash without touching DRM, which is how `data/vdisp-frame.ref` is generated |
| `oclprobe.c` | OpenCL end to end: NVIDIA's platform, a vector-add kernel built from source, every element checked |
| `eglplat.c` | one EGL platform (`gbm`/`wayland`/`xcb`/`xlib`) with a pixel read back out of a pbuffer — five external-platform libraries, five different paths |
| `vkrt.c` | Vulkan raytracing initialisation, the only moment the driver dlopens `libnvidia-rtcore` |
| `matrix/*.sh` | the coverage-matrix probes; each declares its libraries, entry API and criterion in its own header |
| `../scripts/ioctl-matrix.sh` | the coverage matrix: probe, trace, resolve against the vendor headers, catalogue (section 7) |
| `../scripts/test.sh gpu` | the GPU gate (see `../docs/TESTING.md`) |
| `../scripts/test.sh vdisplay` | the fast virtual-display gate — builds both files above |
| `../scripts/showcase.sh` | the rig: guests up and down, ssh, fleets, the display, the demo |
