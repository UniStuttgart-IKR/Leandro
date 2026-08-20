<!-- SPDX-License-Identifier: MIT -->
# Development

Everything practical: prerequisites, building, getting from nothing to
`nvidia-smi` inside a guest, running the tests, and the errors you will
actually hit.

For **what** the project is, read [`README.md`](README.md); for how the
pieces fit together and where each component documents itself,
[`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md). For measuring discipline, the gate
catalogue and the traps that have already caught someone,
[`docs/TESTING.md`](docs/TESTING.md) stays the reference — this document
points at it rather than repeating it.

---

## 1. Host prerequisites

| | |
|---|---|
| OS | Linux with a recent kernel. Developed and measured on Arch (kernel 6.x). |
| GPU | One NVIDIA card. Only **Turing** has ever been run on real silicon. |
| Driver | Exactly the version in [`DRIVER_VERSION`](DRIVER_VERSION). Not "close enough" — see the Limitations section of the README. |
| Userspace | The matching `nvidia-utils` (libcuda, libnvidia-ml, nvidia-smi) |
| Tools | `rustup`, `cargo`, `cc`, `make`, `qemu-img`, `mkfs.vfat` (dosfstools), `mcopy` (mtools), `curl`, `git`, `iptables`, `ip`, `pkg-config` |
| Rust | Pinned by [`rust-toolchain.toml`](rust-toolchain.toml) |

Check the version match before anything else — it is the failure that does
*not* announce itself:

```sh
./scripts/build.sh check-driver
```

### Persistence mode

The measuring rig depends on it, and the GPU gate refuses to run without it:

```sh
sudo nvidia-smi -pm 1
sudo systemctl enable --now nvidia-persistenced   # permanently
./scripts/showcase.sh state --check
```

Persistence off moves the native reference from 132 ms to 209 ms (+58 %).
The reasoning is in [`docs/TESTING.md`](docs/TESTING.md) §3.2.

### Docker and the guest network

**If Docker is installed, this matters.** Docker sets the `FORWARD` policy to
`DROP` and installs its own chains. Guest traffic must be accepted *before*
those chains are reached, so `scripts/showcase.sh net up` inserts its rules
at position 1 (`-I FORWARD 1`) rather than appending them. Appending produces
a guest that boots, gets an address, and silently has no route out.

Nothing about the host network survives a reboot — links, iptables rules and
the `ip_forward` sysctl are all runtime state. Re-run `showcase.sh net up`
after booting the host. On NixOS the module declares the same bridge, taps
and NAT and none of this is needed (section 10).

---

## 2. Build

The short way — one script, from a fresh clone to a runnable rig:

```sh
./scripts/build.sh --driver auto     # retarget at the running driver and build
./scripts/build.sh --dry-run         # what it would do, and what it costs
./scripts/build.sh --full            # the default steps plus the torch venv (the bake is already in the default; --minimal skips it)
```

`build.sh` runs its preflight **before** downloading anything, because
discovering a driver mismatch after a 641 MB image is the failure worth
avoiding. On NixOS start with `nix develop` (section 10).

### What a wrong driver version actually does

It does not build — and that is by construction, not by luck. `assert_layout!`
in `crates/nvrm-abi/src/nvgpu.rs` checks size, alignment and **every field
offset** against the bindgen-generated structs, so a field that moved between
driver versions is a compile error naming the field. `assert_driver_version()`
at runtime is the second net, not the first.

Measured by retargeting this tree at 580.178.04: the upstream tag exists and
fetches fine, and the build then stops with exactly one error —
`no field hHandleVASpace on type NV_CHANNEL_ALLOC_PARAMS`, the field added in
610.43.02, which the comment above that line already documented.

The practical consequence: **"does version X work?" is answerable without a
GPU, in about two minutes** — `build.sh vendor`, `build.sh cargo`, the
`class-sizes` step of `test.sh check`. Do that before travelling to someone
else's machine, not on it.

**Where the artefacts go** is decided once, in `LEA_VM_DIR`: guest
instances, base and baked images, the upstream cloud image and every
measurement output live below it, so pointing it at the big disk moves all
of it. Put the answer in `local.env` (copy `local.env.example`); the
default is `vm/` in the checkout.

A bake ends by printing the `LEA_BASE_IMAGE` line for that file;
`build.sh bake --set-default` **writes** it into `local.env` instead,
replacing an earlier one rather than adding a second (`local.env` is read
with `: "${VAR:=…}"`, so the first assignment wins and a second line for
the same variable is dead text that reads as if it were in force). It
refuses where `LEA_ROOT` is read-only — every Nix store install — and names
the environment variable to use there.

The steps by hand (each is a subcommand of the one build script):

```sh
./scripts/build.sh vendor         # open-gpu-kernel-modules @ DRIVER_VERSION (headers only)
./scripts/build.sh check-driver   # running driver == vendor == DRIVER_VERSION?
./scripts/build.sh ch             # cloud-hypervisor @ CH_VERSION + patches/, built
./scripts/build.sh cargo          # host backend, tools, tracer (cargo build --release)
./scripts/build.sh probes         # the probes (into probe/bin/, not versioned)
```

The cloud-hypervisor fork is **required**. It is two patches, and the
one CUDA depends on is
[`patches/0001-generic-vhost-user-shmem.patch`](patches/): SHMEM support
for the generic vhost-user device. Without it the device comes up and
ioctls work, but memory mapping does not — so no CUDA. The second patch
(`0002-generic-vhost-user-device-features.patch`) passes device-specific
feature bits through and has no effect on virtio-nvrm, by construction. The version is pinned in
[`CH_VERSION`](CH_VERSION).

The guest module's C header (`nvrm_wire.h`) is **generated**, not maintained
by hand. `cargo run --release --bin nvrm-genhdr -- --check` says whether it
is current; the GPU gate asks that first.

---

## 3. From nothing to `nvidia-smi` in a guest

One continuous sequence. Copy-paste from the top of a fresh checkout.

```sh
# --- one-time: sources, hypervisor, base image -------------------------
./scripts/build.sh vendor
./scripts/build.sh ch
./scripts/build.sh image               # Ubuntu cloud image + kernel/initrd, checksummed
./scripts/build.sh cargo
./scripts/build.sh probes

# --- host network: bridge, taps, NAT (idempotent) ----------------------
./scripts/showcase.sh net up
./scripts/showcase.sh net status

# --- bake a provisioned guest image ------------------------------------
# Puts build tools, kernel headers, the render/video groups, the NVIDIA
# userspace and the /opt/nvrm/lib search path INTO the image, so no VM has
# to be patched at runtime. Takes a few minutes.
./scripts/build.sh bake --set-default
# -> vm/guest-baked-<serial>-<driver>-<stamp>.qcow2  (+ .manifest)
# --set-default puts that path into local.env as LEA_BASE_IMAGE, so every
# VM from here on overlays it. Without the flag the bake only PRINTS the
# line; for one run, `export LEA_BASE_IMAGE=...` does the same.

# --- one guest with the virtio-nvrm device -----------------------------
# `up` is the whole path: a FRESH backend for this VM (one backend serves
# exactly ONE VM connection), the VM, the guest userspace and probes,
# nvrm_nodes.ko, and virtio_nvrm.ko built and loaded in the guest.
./scripts/showcase.sh up

# --- log in and look --------------------------------------------------
./scripts/showcase.sh ssh
#   nvidia-smi                       # with a baked image: no LD_LIBRARY_PATH needed
#   cd ~/gpu && ./nvprobe 3          # "stage 3 ok (kernel, result correct)"

# --- clean up ----------------------------------------------------------
./scripts/showcase.sh down             # VM first (ALWAYS graceful), then its backend
./scripts/showcase.sh net down         # optional: also removes bridge/taps/rules
```

Every guest is an **instance**: a name, an index, a guest OS, and one
directory `vm/<name>/` holding its disk, seed, pidfiles, sockets and logs.
The OS is remembered in `vm/<name>/guest` (`ubuntu` unless `--guest nixos`
said otherwise) and the transport in `vm/<name>/transport` (`ip` unless
`--transport vsock` said otherwise), so `down`, `ssh` and `status` never have
to be told again; a disk belongs to an OS, and changing one recreates the
other. On the `ip` transport a slot's host-side endpoint is `tap<i>`; on
`vsock` it is `vm/vsock<i>.sock` (section 7). `up` is
`vm0` at index 0 (IP `.10`, `tap0`) unless told otherwise; `--name b
--index 1` is a second guest beside it, `--display` puts NVIDIA's virtual
display and an X server in it, `--session gnome` a desktop and Sunshine on
top; `--base IMAGE` overlays a different base image for that instance (a
desktop-baked one for the desktop, the compute one for `vm0`), consulted
when its disk is created. `showcase.sh status` lists them all;
`showcase.sh -h` has every flag.
Every `up` records the pid of the script that did it in `vm/<name>/owner`,
and `down` refuses an instance whose owner is still alive (a gate, a bench,
a bake) unless told `--force` — measured 2026-08-18: a `down --all` in
another terminal took the bake instance down in the middle of its apt run.

All of the GPU-side verification in one command instead:

```sh
./scripts/test.sh gpu
```

The same sequence with a **NixOS** guest instead of the Ubuntu one — the
image is built rather than provisioned, so `bake` and `image` fall away and
nothing in it depends on what an archive served today (section 10):

```sh
./scripts/build.sh bake --nixos --set-default    # nix build .#guest-image -> LEA_NIXOS_DIR
./scripts/showcase.sh net up
./scripts/showcase.sh up --guest nixos --with-torch
./scripts/test.sh gpu --guest nixos
```

### Several VMs on one GPU

```sh
./scripts/showcase.sh up --count 4     # network, overlays, backends, VMs, modules
./scripts/showcase.sh up --count 4 --guest nixos       # the same, NixOS members
./scripts/showcase.sh status
./scripts/showcase.sh ssh -i 1         # into member 1
./scripts/showcase.sh exec 'cd ~/gpu && ./nvprobe 3'   # all at once
./scripts/showcase.sh down --all
```

`up --count N` is the whole path and reports success only once every member
answers over SSH. Member 0 **is** the standard dev VM (`vm/vm0/`), so the
gate keeps addressing it by the same instance. Members 1..N are thin qcow2
overlays on `vm/base-torch.qcow2`, a frozen copy of a fully provisioned
disk — which is why that base must never be written again.

Measured: four `convburn` runs at once, 95.4 ms/it each against 24.4 ms/it
alone — 3.9× at 4 VMs, i.e. fair time-sharing — with all four results
bit-identical.

### Native reference on the host

`scripts/bench.sh transport` and the GPU gate compare against the bare card,
and that reference must use the *same* torch version as the guest, otherwise
two libraries are being compared instead of two transport paths. The system
Python is too new for the wheels, hence a dedicated venv:

```sh
uv venv --python 3.12 vendor/hostvenv
uv pip install --python vendor/hostvenv/bin/python torch==2.13.0 numpy
```

---

## 4. Tests

Three different things, and they are not interchangeable:

| | command | duration | what it says |
|---|---|---|---|
| GPU-free band | `./scripts/test.sh check` | ~2 s warm | 14 steps, one exit code |
| The gates | `./scripts/test.sh gates [all]` | see the catalogue | acceptance, against a reference |
| The suites | `./probe/run/suites.sh` | ~10 min | breadth over real CUDA APIs |

A gate is **binary**: anything other than PASS is a defect. The suites
can contain expected failures (they depend on knobs the gate does not set) —
folding them into the gate would mean teaching the gate to accept red.
More on the measurement traps: [`docs/TESTING.md`](docs/TESTING.md).

### The gate catalogue

| gate | command | needs | duration | what it measures |
|---|---|---|---|---|
| — | `./scripts/test.sh check` | `cc`, `edid-decode` | ~2 s warm | the GPU-free band: 14 steps, own vocabulary, **not** a gate |
| `gpu` | `./scripts/test.sh gpu` | GPU, VM, persistence mode, `vendor/hostvenv`, host `ffmpeg` | ~100 s | the compute chain: `tables`, `smi`, `memory`, `kernel`, `torch`, `robustness`, `own`, `encode`, plus an unscored counter-check |
| `vdisplay` | `./scripts/test.sh vdisplay` | GPU, VM | minutes warm; the FIRST run on a fresh instance additionally builds NVKMS (nvidia-modeset.ko, the display half) in the guest and takes several minutes more | the virtual display, fast: `setup`, `tables`, `device`, `edid`, `pixel`, `teardown` |
| `display` | `./scripts/test.sh display` | GPU, VM, X and Sunshine in the guest, `moonlight` on the host, Vulkan | ~10 min | the whole desktop path: `rig`, `card`, `connector`, `edid`, `modeset`, `capture`, `swapchain`, `present`, `rt`, `events`, `stream`, `kernel` |

Run them one at a time, or as a band:

```sh
./scripts/test.sh gates               # all three, in that order
./scripts/test.sh vdisplay            # one
./scripts/test.sh gates gpu display   # several
```

Each gate has its own instance: `gpu` runs on `vm0`, `vdisplay` on
`vdisplay` (index 6), `display` on `desktop` (index 5) — the same instance
`showcase.sh up --name desktop --index 5 --display` uses by hand, so a
desktop somebody left running is torn down by the display gate, on purpose
and announced.

**No gate builds the workspace.** `build.sh cargo` is a *precondition*: a
missing binary in `LEA_BIN_DIR` is a `skip` naming the command that fixes
it, not a red gate. A gate that builds first turns a compile error into a
failed measurement and hides which of the two is broken.

That trade has a second failure mode, and it is the dangerous one: a
*missing* binary is loud, a **stale** one is not. Edit `session.rs`, forget
to rebuild, run the gate — every stage passes and it reports green about
code nobody is running. So `lea_require_built` also compares mtimes: if any
`*.rs`, `*.h`, `*.toml` or `Cargo.lock` under `crates/` (plus the workspace
`Cargo.toml`/`Cargo.lock`) is newer than the oldest required binary, the
gate skips and names the file. Two facts record it either way —
`binaries_built` on every run, `stale_source` when one was found.

```sh
LEA_ALLOW_STALE=1 ./scripts/test.sh vdisplay   # measure the old binary anyway
```

The escape hatch is deliberate. Without one the way past this check is
`touch`, and a check people learn to defeat is worse than no check at all —
so it is one variable, it warns loudly, and it still records
`stale_source`. `vendor/` is **not** in the comparison: `build.sh vendor`
rewrites those headers whether or not their content changed, and a re-fetch
would otherwise look like an edit.

`vdisplay` does not replace `display`. What it deliberately does **not**
cover: no GPU-side rendering (its frame is written by the CPU into a dumb
buffer), no scanout capture (a virtual display has no scanout — "read back"
means re-reading the framebuffer through the driver's export path, which
proves the buffer round trip and not that a compositor would see anything),
no event back-channel and no streaming. Those stay in `display`.

### The gate output contract

Every gate ends in exactly **one** JSON object, as the last line of stdout,
and stdout carries nothing else. All human text goes to stderr.

```
{"gate":"gpu","result":"pass","dur_s":100,"facts":{...}}
{"gate":"gpu","result":"fail","dur_s":31,"reason":"failed: torch","facts":{...}}
{"gate":"vdisplay","result":"skip","dur_s":0,"reason":"no /dev/nvidiactl","facts":{}}
```

- Keys in that order. `reason` is omitted on `pass`; `facts` is always
  present and may be `{}`.
- Exit **0** pass, **1** fail, **2** skip.
- `skip` means a **precondition** was not met — no GPU, binaries not built,
  rig not ready, a host tool absent. It is *not* for "a stage could not
  measure": inside a running gate that stays a `fail`, because a skipped
  stage inside a green gate is a success claim without a reader.
- Fact values are unquoted numbers when they match
  `^-?[0-9]+(\.[0-9]+)?$` and JSON strings otherwise; nesting goes one
  level deep and no further. The library writes three keys itself: `stages`
  (stage names in run order), `failed`, and `detail` (the free text a gate
  passes to `lea_gate_finish`). Everything else comes from `lea_gate_fact`.
- A gate that dies before `lea_gate_finish` — `set -e`, a `die` in a helper,
  SIGTERM — still emits a line, from the EXIT trap `lea_gate_begin`
  installs. A clean-looking exit 0 without a verdict is reported as a
  **fail**, never a pass.

`test.sh gates` is an aggregator, not a gate: it runs each gate as its own
process (`test.sh <gate>`), prints one line per gate (and writes
`vm/out-gates/summary.jsonl`), and cross-checks each verdict against that
gate's exit code — a mismatch counts as a fail. It exits 0 when everything
passed, 1 when anything failed, and **2 when nothing failed but something
was skipped**: a band that did not fully run is not green.

The implementation is `lea_gate_*` in
[`scripts/lib/common.sh`](scripts/lib/common.sh).

WARNING: the `gpu` and `display` gates run without `set -e` (they do their
own cleanup and want every stage to run). `lea_gate_finish` therefore
terminates with `exit` rather than a return value — which would be lost
there, and the gate would report PASS although a stage had failed. The
`vdisplay` gate is the opposite by design: `set -e` and fail-fast, because
each of its stages is a precondition of the next.

### Shell hygiene

`shellcheck -x` over every script, in three passes
(`.github/workflows/check.yml`, job `shell`): severity `error` across the
whole tree and severity `warning` over `scripts/lib/*.sh` and `test.sh`
both block; severity `warning` elsewhere is printed and blocks nothing. Run
it by hand with the repository root as the working directory:

```sh
shellcheck -x scripts/*.sh scripts/lib/*.sh scripts/guest/*.sh
```

`.shellcheckrc` holds the configuration. Since the 2026-08-18
consolidation every file is clean at severity `warning`: every script
resolves `LEA_ROOT` from its own location and works with absolute paths,
which is what closed the old SC2164 backlog.

There is **no** `shfmt` gate, and that is measured rather than forgotten:
`shfmt -i 4 -ci -bn -d`, measured 2026-08 over the pre-consolidation tree
(44 scripts then; 14 today), produced 5904 diff lines,
and no flag combination tried came below that. The disagreement is not about
indentation — shfmt expands this repository's two commonest idioms, the
one-line guard `[[ -f $x ]] && { echo "$x"; return 0; }` and the aligned
one-line function. `.editorconfig` records the indentation rules and the
measurement.

### Suite results

Measured with the host backend running `LEA_MANAGED_COMPAT=1`:

| suite | result | note |
|---|---|---|
| `test_cuda_graphs.py` | PASS | |
| `test_high_freq_event_polling.py` | PASS | |
| `test_multi_process_cuda.py` | PASS | |
| `test_nvrtc.py` | PASS | |
| `test_vram_churn.py` | PASS | reaches OOM in phase 1, which is expected |
| `rapids_cuml_cudf.py` | PASS | |
| `test_uvm_migration.py` | PASS | **only** with `LEA_MANAGED_COMPAT=1` on the host |
| `test_async_streams.py` | PASS | **only** with `max_pin_mib >= 2048` in the guest |

**All eight pass** once both knobs are set. Neither of the two failures once
recorded as architecture limits was one:

```sh
LEA_MANAGED_COMPAT=1 ./scripts/showcase.sh up --max-pin-mib 3072
./probe/run/suites.sh
```

`max_pin_mib` is a **guest module** parameter capping concurrently pinned
memory (default 1024 MiB); `scripts/showcase.sh up --max-pin-mib N` sets
it. It is not the only pin limit and usually not the binding one:
`LEA_MAX_PIN_MIB` on the **host backend** bounds a SINGLE pin at 256 MiB by
default, so a 512 MiB `cudaHostRegister` is refused with the guest limit
untouched at 1024 (measured 2026-08-16, `pinwin.py`). Only the backend log
tells the two apart -- `arena length ... over the pin limit 256 MiB
(LEA_MAX_PIN_MIB)`. Raise both when a workload needs large single pins. `LEA_MANAGED_COMPAT` is a **host** switch, read in `session.rs`. Setting
it inside the guest does nothing. Without it, `test_uvm_migration.py` fails with
`cudaErrorInvalidValue` — which is the intended default, not a defect. The
runner derives that expectation from the running backend instead of
hard-coding it, and reports `XFAIL`/`XPASS` accordingly.

Full detail: [`probe/suites/README.md`](probe/suites/README.md).

### A new test must have been red once

A test that was never red is not a test. Break the line it checks, watch it
fail, put the line back — with a `cp` backup, never with `git checkout`.
Twice that exercise stayed green, and *that* was the finding.

---

## 5. Repository layout

The per-file responsibilities and the reasoning behind the cuts are in
[`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md); this is the map.

| path | what |
|---|---|
| `crates/nvrm-sys` | bindgen over the NVIDIA SDK headers, one driver version |
| `crates/nvrm-abi` | curated ABI: escapes (RM's ioctl commands), DRF (NVIDIA's `hi:lo` bit-field notation), class tables, translation |
| `crates/nvrm-client` | RM (NVIDIA's Resource Manager, the kernel driver behind `/dev/nvidia*`) object tree from the client's point of view; also the small host tools (`mmapping`, `smipids`, `vsockconnect`) |
| `crates/nvrm-wire` | wire protocol, congruent with `nvrm_wire.h` |
| `crates/nvrm-trace` | LD_PRELOAD tracer and line format |
| `crates/vhost-user-nvrm` | host end: the virtio-nvrm device |
| `crates/vhost-user-input` | virtio-input as an external vhost-user backend (keyboard/mouse for the desktop guests) |
| `guest-module/nvrm_nodes/` | device nodes, `/proc/driver/nvidia`, VA→GPA (guest-virtual to guest-physical) |
| `guest-module/virtio_nvrm/` | the guest driver serving the NVIDIA device nodes |
| `scripts/` | the four entry points (see below) |
| `scripts/lib/` | `config.sh` (constants), `common.sh` (helpers, gate contract), `rig.sh` (network, backends, VMs, fleets), `provision.sh` (what goes into a guest) |
| `scripts/guest/` | files shipped INTO guests: probe sources, `nvrm-setup.sh`, `display-identity.sh`, `bench-one.sh` |
| `nix/`, `flake.nix` | packages, the NixOS host and guest modules, the guest image, the dev shell (section 10) |
| `nix/guest-image.nix` | the NixOS guest: one configuration, built for direct kernel boot and for UEFI |
| `probe/c`, `probe/python` | single-purpose instruments |
| `probe/kernels/` | `kernels.cu` and the versioned `.ptx` |
| `probe/run/` | probe entry points, including the suite runner |
| `probe/suites/` | the Python suites |
| `probe/bin/` | build output, **not** versioned |
| `patches/` | the two cloud-hypervisor patches |
| `vm/` (= `LEA_VM_DIR`) | **the artefact store**: instances, base and baked images, the upstream guest image, the NixOS guest image (`LEA_NIXOS_DIR`), outputs — gitignored, and movable as a whole |
| `local.env` | this checkout's `LEA_*` answers, read by `config.sh` first (gitignored; `local.env.example` shows the shape) |

Scripts are named after their function, never after milestones
([`docs/NAMING.md`](docs/NAMING.md) rule 4):

| script | does |
|---|---|
| `build.sh` | from a fresh clone to a rig: `preflight`, `vendor`, `ch`, `cargo`, `probes`, `image`, `bake` (`--nixos` builds the NixOS guest instead of provisioning an Ubuntu one), `check-driver`; `all` runs them in order |
| `test.sh` | `check` (the GPU-free band), `gpu`/`vdisplay`/`display` (the gates), `gates` (the aggregator) |
| `bench.sh` | the measurement track: `transport`, `summary`, `fleet`, `render`, `stream`, `diag`, `vk`, `slurm` (section 9a) |
| `showcase.sh` | the rig and the demo: `net up/down/status`, `up` (one guest, a fleet, a display, a desktop), `down`, `status`, `ssh`, `exec`, `state`, `clean`, `audit`, `display`, `demo` |
| `lib/config.sh`, `lib/common.sh`, `lib/rig.sh`, `lib/provision.sh` | the shared library — one implementation of each thing, sourced by the four |

`build.sh package` and `bench.sh slurm` are the cluster path (section 9a):
one Apptainer image plus the guest image, and one allocation per measurement.

Every long-running subcommand holds a pidfile named after itself
(`vm/showcase.pid`, `vm/test-gpu.pid`, `vm/bench-transport.pid`, ...); wait
on the file, never on a command line (section 8).

### What was removed rather than kept

The LD_PRELOAD shim, the interim virtio-gpu carrier and their gates are
**gone from the tree** — −4890 lines, removed 2026-08-04. There is one
carrier now, and the correctness reference is the native host run.

crosvm went the same way on 2026-08-18: it had only ever supplied the
virtio-gpu display device, and the display path that stayed is NVIDIA's own
virtual display (NVKMS inventing a connector inside the guest), which needs
no second DRM device. With it went `vm-desktop.sh`, `present-sweep.sh`,
`patches/crosvm/` and `CROSVM_VERSION`; and the 43 host scripts collapsed
into the four entry points above plus the four-file library under `scripts/lib/`.

Where the documentation still describes those paths, it is reporting
what was **measured** on them. None of it is runnable here, and the S0–S2
stage labels name arguments, not commands.

### Known small gaps

Things that are true of the tree as it stands, small enough that none of
them stops anything and large enough that finding them twice would be a
waste. Written down here rather than in a private list, so that the next
person meets them as facts instead of surprises.

- **`probe/run/*.sh` are outside both syntax nets.** `test.sh check`'s
  `bash -n` step and the CI ShellCheck invocation both glob `scripts/`
  only. The five runners under `probe/run/` are checked by nothing; they
  are also the scripts a reader is most likely to copy from.
- **Three display probes are staged but never built or run.**
  `lea_guest_setup` copies `eglprobe.c`, `fbprobe.c` and `atomicflip.c`
  into every display guest, and the block above them describes them as
  readers of the display path. Nothing compiles or runs them today; they
  were the instruments of the EGLImage hunt (numbers 32-35) and are kept
  for the next one.
- **`Rsp.scm_fd_count` names a transport that is gone.** The field counted
  the FDs passed back with `SCM_RIGHTS` on the retired Unix-socket
  transport; across a VM boundary there is no FD to pass and it is always
  0. The word stays because the layout is the wire contract, and renaming a
  field of it is a protocol change for a cosmetic reason.
- **The measurement CSVs carry German tokens.** `nativ` and `modul` as
  variant names, and `messungen.csv` as a file name. Renaming them touches
  the aggregator, the plotting scripts and archived data that cannot be
  regenerated, so the tokens stay until something else has to change there
  anyway.
- **`nvrm-sys` sets `doctest = false`.** Possibly redundant since the
  bindgen builder runs with `generate_comments(false)`, which is what used
  to produce doc comments that rustdoc tried to compile. Not verified
  either way; removing it costs a full `cargo test --doc` run to find out.

---

## 6. Branch policy

**Development happens on `main`.** Long-lived side branches are what let
`main` fall behind the actual state of the work — it once stood 18 commits
behind, which is why this is written down at all.

- Work on `main`, in small, thematically clean commits.
- A branch is for something genuinely speculative, and it gets merged or
  deleted — not left standing.
- Before deleting a branch: if its tip is an ancestor of `main`, `main`
  already preserves it and it can simply go. If it carries anything unique,
  tag it first (`git tag -a archive/<branch> <sha> -m "…"`), then delete.
- No force pushes, no `git reset --hard` on anything published.

---

## 7. Reaching a guest: two transports, and the host keys

Every SSH call in this repository goes through one definition in
`scripts/lib/common.sh`:

```sh
-o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null
```

This is the **root fix, not a workaround**, and it was chosen over keeping a
repo-local `known_hosts`:

- These guests are short-lived, they recycle a fixed set of RFC1918
  addresses, and `--fresh` regenerates their host keys **by design**. A
  `known_hosts` entry for them would be stale more often than not.
- A repo-local `known_hosts` would need clearing on every `--fresh` — which
  is the crutch script all over again, merely scoped to the repo.
- Sending entries to `/dev/null` means **nothing is ever written to the
  user's `~/.ssh/known_hosts`**, so there is nothing to clean up afterwards
  and no `clear-known-hosts.sh` is needed.

Because the options live in `lib/common.sh` and every consumer uses
`lea_ssh` / `lea_scp`, they cannot drift apart between the fleet path, the
provisioning path and ad-hoc `scp`.

This trades away MITM protection, which is acceptable **here** because the
path never leaves the host bridge — or, on the second transport, never
becomes a network at all. Do not copy these options to anything crossing a
real network.

There is a separate root cause worth knowing: a guest that is killed hard
leaves **empty** SSH host keys behind, and `sshd` then refuses to start on
the next boot (connection refused, which looks like a network fault). Always
stop guests with `./scripts/showcase.sh down`, which powers down gracefully.
The cloud-init seed also repairs zero-length host keys on boot.

### The second transport: vsock

```sh
./scripts/showcase.sh up --name nixv --index 3 --guest nixos --transport vsock
./scripts/showcase.sh up --count 4 --guest nixos --transport vsock
./scripts/test.sh gpu --instance nixv --index 3 --guest nixos --transport vsock
```

**Why.** The `ip` transport needs `showcase.sh net up`, and that is a bridge,
a tap per slot, a MASQUERADE rule and `net.ipv4.ip_forward` — four host-wide
changes, every one of them through `sudo`. On a workstation that is fine. On
a cluster node you have an unprivileged shell, `/dev/kvm` and an assigned
GPU, and nothing else; there is no root to give and no bridge you are allowed
to create. `--transport vsock` gives the guest **no network device at all**
and reaches it through one unix socket instead.

**It needs no privilege of its own, and that is measured rather than hoped
for.** cloud-hypervisor implements virtio-vsock in **userspace**: measured
2026-08-19 with v53.0, the `vhost_vsock` module was not loaded before a
vsock guest ran and was still not loaded after it, and the VM process held no
file descriptor on `/dev/vhost-vsock`. The host side is not an `AF_VSOCK`
socket either — it is a UNIX socket speaking the *hybrid* protocol
Firecracker defined.

Two consequences follow, and the second one matters for clusters:

- Nothing here touches a kernel vsock device, so nothing needs a module
  loaded, a device node opened or a capability held.
- **There is no shared CID space.** The guest CID is a number that guest
  sees; everything host-side is addressed by the socket *path*. Two guests
  may hold the same CID as long as their sockets differ — so two jobs on one
  node cannot collide on it.

**NixOS only.** An Ubuntu guest takes its address from a cloud-init NoCloud
seed and its sshd listens on TCP; both halves would have to be replaced, and
`--transport vsock --guest ubuntu` is refused with that reason. On the NixOS
image nothing had to be added at all: **systemd's own ssh generator already
puts sshd on `AF_VSOCK` port 22** inside a VM (systemd 260 in this image),
which the guest announces on its console as *"Try contacting this VM's SSH
server via 'ssh vsock%43' from host"*.

**How it stays one definition.** `lea_ip` is bijective, so an address
recovers the index it was made from, and the index names that slot's socket
by formula (`lea_vsock_sock`). `lea_ssh_opts` therefore resolves the
transport from the address it is handed and appends an `ssh -o ProxyCommand`
when the slot's socket exists. Every call site of `lea_ssh`, `lea_scp`,
`lea_guest` and `lea_wait_ssh` goes on passing exactly what it always
passed; none of them knows a transport exists — the only places that spell
`vsock` outside the library are usage text and the SLURM cell generator's
`--transport vsock` arguments. On the vsock transport the address is a pure
label — the guest has none.

The ProxyCommand is `vsockconnect`, a binary in the Rust workspace beside
`mmapping` and `smipids`, so `build.sh cargo`, the nix package and the store
install all carry it. `socat` cannot do this job: its `VSOCK-CONNECT` speaks
real `AF_VSOCK` to a host kernel device, which is not what is on the other
end. The protocol is three lines — connect, write `CONNECT <port>\n`, read
one line, and the stream is raw from there — and the whole binary is shaped
around one trap: **the reply line and the first payload bytes arrive in the
same read**. A fixed-size read swallows the beginning of the SSH banner and
ssh then reports a protocol error that looks like a broken transport. It
reads to the newline and pushes the surplus back; a unit test feeds exactly
that shape and asserts nothing is lost.

**What it costs: nothing worth choosing between.** Measured 2026-08-19, the
same 494 MiB payload staged into the same guest image, transports
interleaved:

| transport | run 1 | run 2 |
|---|---|---|
| vsock | 1.75 s (282 MiB/s) | 1.58 s (313 MiB/s) |
| ip | 1.68 s (294 MiB/s) | 1.56 s (317 MiB/s) |

and the compute measurement is untouched — `bench.sh fleet` over four NixOS
guests at stages 1/2/4 gave 32.41 / 26.22 / 47.24 ms per iteration over
vsock against 33.59 / 26.99 / 49.00 over ip the same afternoon, with the
identical single `acc` value on both. Choose the transport for what it
removes, not for what it costs.

**What it removes, verified 2026-08-19 with the bridge torn down and a
`sudo` on `PATH` that logs its arguments and exits 1:** `showcase.sh up`, the
compute gate (8/8 stages, exit 0), a four-guest fleet and `bench.sh fleet` at
stages 1/2/4 all ran to completion and the log of attempted `sudo` calls was
**empty**. No bridge, no tap, no NAT rule, no `ip_forward`, no root.

**What it does not do.** No display, no streaming, no X — a guest with no
network device is not where those belong, and `--display` on a vsock instance
is refused. And `--with-torch` cannot work there either: the guest has no
route to pypi, so the venv must come from the frozen base
(`LEA_NIXOS_FLEET_BASE`), which is also what a cluster node needs — everything
in the image or the base, nothing downloaded where the job runs.

---

## 8. Troubleshooting

Errors actually encountered, with what they really mean.

**`make: command not found` in the guest.**
The cloud image ships neither build tools nor kernel headers. Use
`scripts/build.sh bake` so the image carries them; `showcase.sh up`
installs them at runtime as a fallback.

**A VM boots to a login prompt but SSH times out ("start failed").**
Almost always a **stale cloud-init seed**: the seed carries the guest
identity (user, key, hostname) and is only rebuilt on `--fresh` or when
missing. A seed written before the guest identity changed produces a VM that
boots perfectly and refuses every login. Fix: `showcase.sh up --fresh` for
that instance, or delete `vm/<name>/seed.img` together with its
`rootfs.qcow2`. A seed and its disk describe **one** instance and must be
created and dropped together.

**A provisioned disk from before the 2026-08-18 layout change.** Instances
moved from flat `vm/ssh-*` files into `vm/<name>/`. A dev disk worth keeping
moves by hand: `mkdir -p vm/vm0 && mv vm/ssh-rootfs.qcow2 vm/vm0/rootfs.qcow2
&& mv vm/seed-ssh.img vm/vm0/seed.img`; everything else under the old names
is regenerable and can go.

**cloud-hypervisor exits the instant a backend dies.**
Not a guest crash. One `vhost-user-nvrm` backend serves exactly **one** VM
connection, and killing it takes the VM with it. Order matters: backend
first on the way up, VM first on the way down.

**A wait loop on a long-running script never ends.**
Same family as the `pkill -f` entry below, and it costs a whole run's worth
of wall clock because the script it waits for has long finished. This

```sh
until ! pgrep -f "bash ./scripts/bench.sh"; do sleep 15; done   # WRONG
```

waits forever: the pattern stands verbatim in the **waiting shell's own
command line**, and `pgrep` excludes only itself, never its parent. A
tighter regex hides that; it does not fix it. The rule is **do not match on
command lines at all** — neither for waiting nor for killing. Every
long-running script here holds a pidfile instead
(`lea_hold_pidfile` in `scripts/lib/common.sh`), so the wait is:

```sh
source ./scripts/lib/common.sh
until ! lea_running vm/bench-transport.pid; do sleep 15; done
```

The pidfile is named after the subcommand, and every subcommand that can
run for minutes holds one. Two questions decide whether it is safe to wait
on — does it hold a pidfile, and does it say anything while it runs:

| command | pidfile | speaks while running |
|---|---|---|
| `bench.sh transport` | `vm/bench-transport.pid` | `--progress`: one line per measurement |
| `bench.sh fleet` | `vm/bench-fleet.pid` | one line per stage and repetition |
| `bench.sh render` | `vm/bench-render.pid` | one line per stage and guest |
| `bench.sh stream` | `vm/bench-stream.pid` | one line per phase |
| `bench.sh diag` / `vk` | `vm/bench-diag.pid` / `vm/bench-vk.pid` | one line per run |
| `test.sh check` | `vm/test-check.pid` | `PASS`/`FAIL` plus seconds per step |
| `test.sh gpu` / `vdisplay` / `display` | `vm/test-gpu.pid` / `test-vdisplay.pid` / `test-display.pid` | one line per stage |
| `test.sh gates` | `vm/test-gates.pid` | per-gate banner and each gate's stderr, through `tee` |
| `build.sh bake` | `vm/build.pid` | one section header per provisioning step |
| `showcase.sh up` / `down` / `demo` | `vm/showcase.pid` | one line per step; the demo continuously |
| `bench.sh slurm --run CELL` | `vm/slurm-<cell>.pid` | one line per repetition |
| `probe/run/suites.sh` | `vm/suites.pid` | one line per suite |
| `probe/run/trace.sh` | `vm/trace-<workload>.pid` | one line per stage |
| `probe/run/drmtrace.sh` | `vm/drmtrace.pid` | one line per workload |

`showcase.sh` holds it for `up`, `down` and `demo` **only**. `ssh`, `exec`
and `status` take seconds and `bench.sh` calls them dozens of times per
run; a pidfile that appeared and vanished that often would let a waiter see
a run end that never started.

Nesting is not a collision, because the name is the *subcommand's*:
`test.sh gates` holds `vm/test-gates.pid` while the `test.sh gpu` it runs
holds `vm/test-gpu.pid`. Wait on the outer one for the whole band.

**Two runs of the same script at once** are legitimate and now say so:
`lea_hold_pidfile` warns when it takes over a live pidfile, and its EXIT
trap removes the file only while it still names *that* shell — otherwise
the first run to finish would delete the second run's pidfile, and every
waiter would read that as "done".

**A progress line piped into `tail` is not a progress line.** That is how
`--progress` was built and then never actually seen: the run went through
`| tail -40`, which cannot print anything until the stream ends. `head`
cannot flush at all, `grep` needs `--line-buffered`, `awk` needs
`fflush()`. Watch a long run by redirecting to a file and reading the file,
not by filtering the pipe.

**`pkill -f vhost-user-nvrm` kills unrelated things.**
Use `pkill -x`. A `-f` pattern also matches your own shell and any editor
with the source open. And `comm` is truncated to 15 characters, so the
binary has to be killed under **both** spellings (`vhost-user-nvr` and
`vhost-user-nvrm`).

**Something crashed and left backends, sockets or pidfiles behind.**
`./scripts/showcase.sh clean --dry-run` lists what a cleanup would take;
without the flag it takes it. It kills orphaned backends (`pkill -x`, both
spellings), removes `vm/*/*.sock` and fifos that nobody holds, and removes
pidfiles whose process is dead -- a pidfile with dead content is the worst
of the three, because every other script reads it as "running". On an empty
rig it says `nothing to clean` and exits 0, so it is safe to run twice. A
**running instance is not garbage**: it refuses with exit 3 while any
instance's cloud-hypervisor is live, and only `--force` makes it take that
instance down (VM first, gracefully, then its backends -- never by signal).
A `cloud-hypervisor` without one of our pidfiles is reported and **not**
touched -- the `pkill -x` entry above says why killing by name is the
mistake this rule exists to prevent.

**A gate refuses to start ("instances of another rig are running").**
Deliberate, and a `skip`. Every instance has its own backends and its own
directory, so nothing kills by name any more -- but the GPU is one, and a
measurement taken beside somebody else's guest is a measurement of both.
`./scripts/showcase.sh status` shows who is up; `showcase.sh down --name X`
stops it.

**Guests have an address but no route out.**
The FORWARD rules are behind Docker's chains, or the NAT rule names the
wrong uplink. `./scripts/showcase.sh net status` shows the detected uplink;
`net up` inserts the FORWARD rules at position 1. Remember none of it
survives a reboot.

**`cudaMallocManaged` returns `cudaErrorInvalidValue`.**
`LEA_MANAGED_COMPAT=1` is missing on the **host** backend. It is a host
switch read in `session.rs`; setting it in the guest does nothing.
Oversubscription genuinely does not work either way.

**`pin_memory=True` / `cudaHostRegister` returns 304.**
Usually a **cap**, not the boundary, and there are two of them:

| limit | where | scope | default |
|---|---|---|---|
| `max_pin_mib` | guest module parameter | cumulative | 1024 MiB |
| `LEA_MAX_PIN_MIB` | host backend env | one allocation | 256 MiB |

Raise the guest one with `./scripts/showcase.sh up --max-pin-mib N` (or
`LEA_GUEST_MAX_PIN_MIB`), the host one with `LEA_MAX_PIN_MIB=N` in the
environment of `showcase.sh up` (it reaches the backend). Both refuse with CUDA error 304, so the **backend log**
is what tells them apart: the host prints `over the pin limit ...
(LEA_MAX_PIN_MIB)`. Host pinning itself works -- the GPU gate asserts that pins happened
and were released (27 OS descriptors per run on this rig).

**A VM sees less VRAM than the card has.**
`LEA_VRAM_LIMIT_MIB` is set on that VM's backend. It is a third limit and
deliberately unlike the two above -- in name, in unit and in effect:

| limit | where | what it bounds | default |
|---|---|---|---|
| `max_pin_mib` | guest module parameter | pinned guest RAM, cumulative | 1024 MiB |
| `LEA_MAX_PIN_MIB` | host backend env | pinned guest RAM, **one** allocation | 256 MiB |
| `LEA_VRAM_LIMIT_MIB` | host backend env | device memory, cumulative per **VM** | 0 = off |

The pin limits refuse with CUDA error 304. The VRAM cap refuses with an
ordinary **out of memory** -- deliberately, because that is what a full card
answers (measured: status 0x51 `NV_ERR_NO_MEMORY` in the NVOS64 allocation
block — the RM_ALLOC parameter struct from nvos.h — with ioctl return 0),
and it is the only refusal a workload can catch. In torch it arrives as
`torch.cuda.OutOfMemoryError`, not as an `AcceleratorError`. The backend log
names it:

```
vhost-user-nvrm: VRAM cap reached (1. refusal) -- 2048 of 2048 MiB in use,
allocation answered with NV_ERR_NO_MEMORY (LEA_VRAM_LIMIT_MIB)
```

```sh
./scripts/showcase.sh up --name b --index 1 --vram-limit 2048
```

**Under a cap the guest also sees a smaller card.** That is deliberate: a
VM capped at 2048 MiB that is told it has 8192 MiB plans against a number it
can never reach. Measured before this existed: `test_vram_churn.py` in a
capped guest reported `detected VRAM: 7773 MiB` and then fell over on its
second 512 MiB block.

```
|   0  Leandro RTX 2070-2G          On  |   00000000:2D:00.0  On |         N/A |
| 58%   52C    P8      30W / 175W   |     982MiB /   2048MiB |  6%  Default |
```

The name follows NVIDIA's own vGPU convention -- an `A100` becomes a
`GRID A100-10C` -- so that nobody can be inside a mediated VM without
noticing: the vendor prefix gives way to the project name, and under a cap
the profile size joins it. The name is rewritten with or without a cap
(`Leandro RTX 2070` uncapped, `Leandro RTX 2070-2G` capped;
`LEA_GPU_NAME_RAW=1` on the backend keeps the driver's own string). `torch`
agrees: `get_device_name` returns `Leandro RTX 2070-2G` and `total_memory`
2048 MiB.

Total, heap and free all come from the ledger, so they cannot contradict
each other. Without a cap the SIZES are not rewritten -- the VM has the
whole card, and saying otherwise would be the lie.

**The VM's process list works with or without a cap.** `nvidia-smi` in the
guest shows the guest's own processes under their guest PIDs. This is not
only cosmetics: measured, the guest previously received the HOST's PID table
verbatim -- eight host PIDs with their per-process FB usage. It printed "No
running processes found" only because it could not resolve those PIDs in its
own `/proc`; the numbers crossed the boundary regardless.

WARNING: the number shown per guest process is this host's own bookkeeping.
It counts explicit memory-class allocations, so it is LARGER than
`torch.cuda.memory_allocated()` (it includes the CUDA context's own
allocations -- measured +106 MiB per process) and SMALLER than the true
footprint (RM's internal device memory never crosses the boundary).

What the cap does **not** bound: RM's own device memory behind a channel
(instance memory, USERD — a channel's doorbell page — and context
buffers) never crosses the boundary as an
allocation request, so a VM's real footprint is its cap plus a context's
worth. And managed memory under `LEA_MANAGED_COMPAT=1` is pinned *guest RAM*
behind an OS descriptor, not FB -- it is bounded by the two pin limits, not
by this one.

**The gate greps a string that no longer exists.**
An output string and the grep that reads it belong in the **same commit**.
This used to be masked by prebuilt probe binaries being versioned — the
stale binary still produced the old text. They are out of git now
(`probe/bin/` is ignored) and every consumer builds them.

**Numbers that disagree with yesterday's.**
Check `./scripts/showcase.sh state --check` first. Persistence mode alone moves
the native reference by 58 %. See [`docs/TESTING.md`](docs/TESTING.md) §3.

## 9. NVIDIA OpenGL in a guest

The compute payload (`lea_payload_stage` in `scripts/lib/provision.sh`)
carries `libcuda` and the encode libraries. NVIDIA's **GL/EGL** half is
staged separately (`lea_gl_stage`), into a directory of its own:

```sh
./scripts/showcase.sh up --with-gl                  # ~200 MiB -> /opt/nvrm-gl, wired into the system
./scripts/showcase.sh ssh 'source /opt/nvrm-gl/env.sh; env -u DISPLAY -u WAYLAND_DISPLAY eglinfo'
```

A separate step on purpose: the compute path is what the GPU gate walks,
and it stays byte for byte unchanged. Removing the GL half is `rm -rf`.

**Without the display rig only the surfaceless platform works, and that is
not a limitation of the staging.** GBM, Wayland and X11 all want a DRM node
whose driver is `nvidia-drm`; a compute guest has no DRM node at all (the
display rig, `showcase.sh up --display`, is what adds nvidia-drm's).
Measured the same way on the host, with `/dev/dri` replaced by an empty
tmpfs: only `EGL_MESA_platform_surfaceless` survives, and it produces a full
OpenGL 4.6 core context over `/dev/nvidiactl` and `/dev/nvidia0` alone.

**So unset both display variables.** With `DISPLAY` or `WAYLAND_DISPLAY`
set, `eglinfo` and anything like it try GBM/Wayland/X11 first and report
their failure as if EGL itself were broken. `/opt/nvrm-gl/env.sh` says so
next to the variables it sets.

`probe/bin/glinterop` is the instrument: surfaceless EGL → GL context →
texture → render → readback → `cuGraphicsGLRegisterImage` → map. It prints
a byte sum of the rendered pixels so a guest run and a native run compare as
numbers rather than as two "ok"s.

**When a new GL program fails**, the loop is short and does not involve
guessing. Start the backend with `LEA_DEBUG=1` (`LEA_DEBUG=1
./scripts/showcase.sh up`); it logs one line per RM call whose *status field*
is non-zero — the ioctl itself returns 0, so `strace` shows nothing.
Resolve the command number against `vendor/open-gpu-kernel-modules/.../ctrl/`,
read its `_PARAMS` struct, and add the entry to `xlate::nested_ptrs` (a
pointer) or `xlate::ctrl_fd_offset` (a file descriptor).

**Vulkan works the same way and needs the ICD manifest** (ICD: the
loader's installable-client-driver JSON that names the vendor library), which the GL
staging writes too (`VK_ADD_DRIVER_FILES` in `env.sh`). It *adds* NVIDIA to
the list rather than replacing whatever the guest already has, so a guest
with a second ICD lists two devices and `-init_hw_device vulkan=vk:1` picks
the NVIDIA one.

WARNING: the `*_GET_CAPS` / `*_GET_INFO` family looks uniform and its length
semantics are not. `capsTblSize` is a **byte** count; `grInfoListSize` and
`fbInfoListSize` are **element** counts of an 8-byte
`NVXXXX_CTRL_XXX_INFO`. Read each header; do not derive one entry from the
one above it.

And two traps that cost time here, both about reading the right thing:

- **Some fields mean different things depending on a neighbour.**
  `NV0005_ALLOC_PARAMETERS.data` is a file descriptor when `hClass` in the
  same buffer is `NV01_EVENT_OS_EVENT` and a callback pointer otherwise, so
  `ClassDesc` carries `fd_if_off`/`fd_if_val` and translates only on a
  match. The outer class does not decide it: NVIDIA's Vulkan allocates under
  `0x0005` where CUDA uses `0x0079`.
- **RM overwrites input fields in place.** Dumping params *after* the ioctl
  shows what RM wrote back, not what went in — `data` reads as
  `0xffff8c5e...` (a kernel pointer) when the caller passed `0x18` (an fd).
  An interposer that wants the input has to copy it before the call.

## 9a. Running the benchmarks on a cluster (SLURM)

The point of this section: an academic with an allocation on somebody else's
cluster can reproduce the compute measurements on hardware nobody here has
seen, and the results are trustworthy enough to compare across sites.

```sh
# once, on a machine with the sources, a toolchain and a network
./scripts/build.sh bake --nixos --with-torch        # the guest, venv included
./scripts/build.sh package --out /shared/leandro-pkg

# then, from the login node
./scripts/bench.sh slurm --package /shared/leandro-pkg --out ./jobs --counts "1 2 4"
while read -r s; do sbatch "$s"; done < ./jobs/submit.txt
./scripts/bench.sh slurm --collect ./jobs/results
```

`--run` is the whole job body and needs no SLURM at all, which is how it is
tested and how you should try it once before queueing a hundred:

```sh
./scripts/bench.sh slurm --run gate-1 --package /shared/leandro-pkg --out ./results
```

### What the site has to allow

| | |
|---|---|
| `/dev/kvm` exposed to batch jobs, writable by the job's user | **required** — there is no software substitute |
| a GPU through GRES (`--gres=gpu:1`) | **required** |
| a writable node-local scratch (`$SLURM_TMPDIR`, else `$TMPDIR`) | **required** |
| `apptainer` on the compute nodes | **required** — singularity is *not* a tested substitute |
| the host's NVIDIA userspace, at the package's exact driver version | **required** — `--nv` injects it |

### What it does NOT need, and this is the short list on purpose

**No root, anywhere.** No bridge, no tap devices, no NAT rule, no
`ip_forward`, no `CAP_NET_ADMIN`, no setuid helper, and no
nested-virtualisation trick beyond plain KVM. The guests are reached over
virtio-vsock (section 7), which cloud-hypervisor implements in userspace —
measured 2026-08-19, it never opens `/dev/vhost-vsock`. Verified on this
machine with the bridge torn down and a `sudo` on `PATH` that logged its
arguments and exited 1: the log stayed empty through `up`, the compute gate,
a four-guest fleet and `bench.sh fleet`.

### The four things that are different about a cluster

**(a) Node-local scratch, not the shared filesystem.** `LEA_VM_DIR` holds the
qcow2 images and every instance's disk, held open for the life of a VM. On
NFS that is slow; on Lustre or GPFS it is slow *and* the file locking a qcow2
wants is either unavailable or a distributed-lock storm. The job copies the
image to `$SLURM_TMPDIR` and runs entirely there — measured 2026-08-19, 13–14
s for the 12 GiB image — and only the *results* go back to the shared path.

**(b) Two jobs on one node do not collide, and one variable does it.**
Everything an instance owns hangs off `LEA_VM_DIR`: the instance directories,
the pidfiles, the SSH key, the backend sockets and the vsock endpoint. The
job namespaces it by `SLURM_JOB_ID` (and `SLURM_ARRAY_TASK_ID`), which
namespaces all of them at once. The guest **CID needs no namespacing**: with
vsock in userspace there is no host-wide CID space, so the socket *path* is
the address — and that is already under `LEA_VM_DIR`. Verified with two
concurrent runs on this machine, both green:

```
/mnt/vmstore/leandro/tmp-node/lea-1001/vsock0.sock
/mnt/vmstore/leandro/tmp-node/lea-1002/vsock0.sock
A exit=0    B exit=0
```

Same slot number, same CID, different paths, neither touching the other.

**(c) A failed job leaves files, not a VM.** `--keep` is meaningless in a
batch allocation: the node is reclaimed with everything on it, and you get
one shot per queue wait. On any failure the job collects the out directory,
the backend log, the guest serial log, the guest's `dmesg` while it still
answers, and the rig line into **one tarball** on the shared path
(`<cell>.forensics.tar.gz`).

**(d) The aggregator refuses invalid comparisons.** `--collect` groups by the
rig line — GPU model, driver, persistence, persistenced, governor and PCIe
**width** — and reports groups separately rather than averaging them.
Persistence alone once moved this project's native reference by 58 %. Note
*width*, not link generation: the gen drops to gen1 idle and rises under
load, so grouping by it declares one machine incomparable with itself
(measured 2026-08-19: gen2x16 and gen3x16 minutes apart on this host).

A cell that produced **no JSON line is a failure, not a gap** — a job killed
at the walltime leaves a rig line and a log and no verdict, and averaging
over what is left quietly reports on a subset nobody chose.

### The driver must match, and it is checked on arrival

`vhost-user-nvrm` is compiled against one driver version — `assert_layout!`
checks every struct offset at compile time — so a package is only valid for
the driver it was built for. The job checks the node's running driver against
the package MANIFEST **before** anything boots, and refuses by name:

```
ERROR: this node runs NVIDIA driver '610.43.03'; the package was built for '570.86.16'.
       Fix, on a machine that has the sources and a toolchain:
         scripts/build.sh all --driver 610.43.03
         scripts/build.sh bake --nixos --with-torch
         scripts/build.sh package --out <pkg-610.43.03>
```

**One package is one driver version**, deliberately: this project targets
exactly one at a time, so there is no variant selection and none is wanted.
Building **on** the node was rejected:
it would need a Rust toolchain, 170 MB of vendored NVIDIA headers and a
network, on a machine that has none of them. Note that datacenter/Tesla
drivers live under a different NVIDIA download path than the desktop ones,
and vGPU/GRID drivers are not on the public CDN at all — for those the
userspace has to come off the node itself, which is what the payload path
already does.

### MIG is refused, as an open question rather than an answer

If the node's GPU is in MIG mode (Multi-Instance GPU, the datacentre
partitioning feature) the job stops before measuring anything. MIG
partitions the RM object surface — different device handles, a partitioned
instance tree — and **nothing in this project has ever been measured against
it**. Producing numbers there would produce numbers nobody can interpret.

### What is in the package, and what is deliberately not

`build.sh package` produces an Apptainer image (the Nix closure: scripts,
`vhost-user-nvrm`, `vsockconnect`, the patched cloud-hypervisor, ssh,
qemu-img, python3, ffmpeg), the NixOS guest image, the probe binaries built
from the same tree, the **native reference venv** the gate's torch stage
compares against, and a MANIFEST recording the driver, the commit and a
sha256 of every part.

**No NVIDIA driver library is in it, and that is checked mechanically** — the
guest is handed the host's own `libcuda` at run time and it is not
redistributable (LICENSES.md). `bake --nixos --with-torch` likewise refuses
to publish an image that picked one up while the venv was being built. What
the guest image *does* contain is PyTorch and the NVIDIA redistributable CUDA
wheels it depends on.

Not a `.deb`, and not a bare Nix closure: there is no root on a cluster to
install the first with, and no Nix on the nodes to unpack the second into.

### What is verified on this machine, and what is not

| | |
|---|---|
| the package builds; the gate runs **out of it**, in the container, over vsock | yes — 8/8 stages, exit 0 |
| a wrong-driver package is refused by name, before anything boots | yes |
| job scripts generate; one cell runs locally without SLURM | yes |
| fan-in: a killed cell shows as a failure; two rigs are refused, not averaged | yes |
| two jobs on one node, both green, no shared state | yes |
| submitted through a **real** `sbatch`, queued, GRES-serialised, all green | yes — single-node SLURM 26.05, 2026-08-19 |
| **a multi-node cluster, a foreign site, another GPU or driver** | **no** |

The queue interaction is verified on a single-node SLURM installed on this
machine: three cells submitted with `sbatch`, held `PD (Resources)` behind
the one GPU, run in turn, all `exit 0`, and the rig lines carry the real
`SLURM_JOB_ID`. Two things it found that hand-running had hidden, both now
fixed: **SLURM does not set `SLURM_TMPDIR`** (it is a site convention, not a
SLURM variable), and the job must `--bind` the scratch it actually resolves
to — binding only `$SLURM_TMPDIR` left the container writing into a path
apptainer had auto-created read-only.

What is still unverified is everything a single node cannot show: more than
one compute node, another site's cgroup and GRES policy, a different GPU, a
different driver version.

---

## 10. NixOS

The flake at the root packages the host side and declares the host rig:

```sh
nix develop                     # the dev shell: cargo (rustup, pinned toolchain), bindgen, qemu-img, ...
nix develop .#prebuilt          # the same, plus LEA_BIN_DIR/LEA_CH pointing at store builds --
                                # `scripts/showcase.sh up` without `build.sh cargo` or `build.sh ch`
nix build .#vhost-user-nvrm .#cloud-hypervisor
nix flake check                 # evaluates BOTH modules, against nix/example-host.nix
                                # and nix/example-guest.nix
nix build .#guest-modules       # the two guest kernel modules (see "The guest side")
```

In a host configuration:

```nix
inputs.leandro.url = "github:UniStuttgart-IKR/Leandro";
imports = [ inputs.leandro.nixosModules.default ];
services.leandro = {
  enable = true;
  user = "me";                 # owns the taps; give it extraGroups = [ "kvm" ]
  dev.enable = true;           # leandro-showcase, leandro-test, leandro-bench, leandro-build on PATH
  nat.externalInterface = "enp39s0";   # or leave null: masquerade on any egress
};
```

The module declares `br-poco`, `tap0..tap7` owned by that user, NAT out of
the host and `nvidia-persistenced` (`services.leandro.*` in
`nix/module.nix` lists every option). It does **not** provide the driver
and cannot: the guest is handed the host's own `libcuda`, which is not
redistributable and must match `nvidia.ko` exactly. It asserts that
`hardware.nvidia` is configured and warns when its version is not
`DRIVER_VERSION`.

`vendor/`, `target/` and `vm/` are ignored by git and therefore invisible
to the flake, which is what makes the packages hermetic — and it means
every new `nix/*.nix` file has to be `git add`ed before `nix build` sees it.
`nix-shell nix/shell.nix` still works, as a shim onto the flake's shell.

### The guest side

`nixosModules.guest` declares what `scripts/lib/provision.sh` does to an
Ubuntu guest by hand. It was added on 2026-08-18 and unverified; **on
2026-08-19 a NixOS guest was booted against a backend and the compute gate
passed on it**. What is measured, and what is not, is spelled out under
"The NixOS guest image" below — the short version is that compute and
fleets are verified and the display path is not attempted.

```sh
nix build .#guest-modules       # nvrm_nodes.ko + virtio_nvrm.ko, against a nixpkgs kernel
```

```nix
imports = [ inputs.leandro.nixosModules.guest ];
services.leandro-guest = {
  enable = true;
  params.file = ./params.txt;      # the HOST's /proc/driver/nvidia/params
  users = [ "me" ];                # into render and video
  maxPinMiB = 2048;
  nvidiaUserspaceDir = "/opt/nvrm/lib";
};
```

What it declares: `boot.extraModulePackages` with the two modules, a
`modprobe.d` entry carrying their options (`nvrm_nodes create_nodes=0`
above all — virtio_nvrm owns the nodes), a oneshot unit that loads them
**in order** and provisions `/proc/driver/nvidia/params` in between, and the
render/video membership without which CUDA in the guest needs root. The
order is a unit rather than `modules-load.d` for the same reason
`scripts/guest/nvrm-setup.sh --persist` writes a boot script: `modules-load.d`
knows "load this module" and nothing about "this one first, then provision,
then that one".

It does **not** provide NVIDIA's userspace and must not: the guest is handed
the host's own `libcuda`, which is not redistributable and must match the
host `nvidia.ko` exactly. `nvidiaUserspaceDir` takes a path the operator
supplies — what `lea_payload_stage` stages into `/opt/nvrm/lib` on the
Ubuntu guests. On NixOS that directory reaches the linker through
`LD_LIBRARY_PATH`, and that is a **real difference, not a detail**: the
Ubuntu images deliberately use `/etc/ld.so.conf.d` because
`LD_LIBRARY_PATH` is lost across `sudo`, `su` and systemd units.

Measured 2026-08-19, in a booted guest: on NixOS the `ld.so.conf` mechanism
does not merely differ, it **does not exist**. `ldconfig` there cannot write
a cache at all — nixpkgs' glibc has its cache path inside the store
(`/nix/store/…-glibc-2.42/etc/ld.so.cache`, read-only), so
`/etc/ld.so.conf.d` is read by nothing. `LD_LIBRARY_PATH` through
`environment.sessionVariables` is not a preference there; it is the only
channel. It does reach a non-interactive `ssh host cmd`, because NixOS
applies `sessionVariables` through `pam_env` rather than through
`/etc/profile`.

**Which guest kernel.** Walked forward one nixpkgs kernel at a time
(measured 2026-08-18): the modules build as written up to 6.11; 6.12 removed
`no_llseek` and 6.13 made `MODULE_IMPORT_NS` take a string — both are now
version guards in the tree, in the style `virtio_nvrm.c` already used for
the 6.11 `fd_file` change. 6.15 removed `hrtimer_init` and 6.18 removed
`nth_page`; those are behaviour, not spelling, on a module measured only on
the guests' 6.8, and they are **not** done. So `nix build .#guest-modules`
pins the 6.12 LTS — the newest LTS it builds against, and the closest to
what the guests actually run. A guest configuration passes its own
`boot.kernelPackages.kernel` and therefore finds out at build time rather
than at `insmod` time.

### The NixOS guest image

`nix/guest-image.nix` is one `nixosConfiguration` built two ways: for
**direct kernel boot** (the rig, and SLURM) and as a **UEFI-bootable qcow2**
(MeisterStack, and anything else that boots an image the normal way). The
Ubuntu path is a convenience — it runs `apt` and is reproducible only up to
what the archive served that day; this one is a derivation.

```sh
./scripts/build.sh bake --nixos --set-default   # build it, copy it into LEA_VM_DIR, make it this checkout's
nix build .#guest-image -o /somewhere/else      # or by hand; -o keeps the ./result symlink out of the checkout (it is gitignored)
nix build .#guest-image-uefi -o /somewhere/else

./scripts/showcase.sh up --guest nixos          # one guest
./scripts/showcase.sh up --count 4 --guest nixos
./scripts/test.sh gpu --guest nixos             # the compute gate against it
```

`bake --nixos` publishes a **directory**: `kernel`, `initrd`, `rootfs.qcow2`
and `image.env`. The last one is the point — direct kernel boot needs three
files *and* one fact, which `init=` this image's system generation is, and a
rig that guesses it boots the wrong generation with a working shell.
`LEA_NIXOS_DIR` names that directory; `lea_nixos_image` in `rig.sh`
validates it before any VM uses it.

**No cloud-init seed.** The Ubuntu guests get their identity from a FAT
`CIDATA` image; a stock NixOS reads nothing of the sort, and teaching it to
would put a second configuration system inside a system whose whole point is
not needing one. With direct kernel boot the host already writes the command
line, so the identity rides on it — `lea_user`, `lea_host`, `lea_ip`,
`lea_prefix`, `lea_gw` and `lea_sshkey` (base64: the kernel splits on
spaces). `leandro-identity.service` reads them before the network comes up.
A **firmware** boot has no host-written command line at all — measured
2026-08-19, a UEFI boot with `--cmdline` set reaches multi-user and ignores
every word of it — so there the same six words arrive as SMBIOS type 11 OEM
strings (`cloud-hypervisor --platform oem_strings=[…]`), and the unit reads
whichever channel answered.

**The Ubuntu argv is unchanged.** `--guest` alters three words of the
cloud-hypervisor command line and nothing else; the Ubuntu one was diffed
before and after, from `/proc/<pid>/cmdline` of a real `showcase.sh up`, and
is byte-identical.

**The payload is the same payload.** The probes reach both guests as
host-built ELF, because the gate compares a guest run against a *native* run
of the same binaries — and because `nvidia-smi` is NVIDIA's own prebuilt
binary and no nix package can produce it. So the image carries `nix-ld`
(`/lib64/ld-linux-x86-64.so.2`) and `lea_guest_setup` stays one function for
both guests. Three things differ inside it, each with its reason on the
spot: the loader wiring, where `nvrm_nodes.ko` comes from, and how a missing
tool is obtained.

**What is verified, on 2026-08-19, on this machine (RTX 2070, 610.43.03):**

| | |
|---|---|
| image builds, `nix flake check` passes | yes |
| boots, reaches SSH, `showcase.sh status` shows VM/NVRM/MODULE up | yes |
| `nvidia-smi` and `nvprobe 3` ("stage 3 ok") in the guest | yes |
| guest `libcuda` is the host's build, by hash | yes — `lea_libcuda_check` |
| `test.sh gpu` — all 8 stages, exit 0, contract unchanged | yes |
| 4 NixOS guests at once, `bench.sh fleet` 1/2/4, one `acc` value | yes |
| UEFI image boots under cloud-hypervisor with `CLOUDHV.fd` | yes |
| `--display` / `--session` on a NixOS guest | **no — refused, see below** |

The **display path is not implemented** for a NixOS guest and says so rather
than half-attempting it: `lea_display_stage` stages NVIDIA's GL/EGL/Vulkan
userspace and `lea_guest_build_nvkms` builds `nvidia-modeset.ko` and
`nvidia-drm.ko` *inside* the guest against its kernel headers. On NixOS both
would have to become derivations, the modules through
`boot.extraModulePackages` like the two Leandro ones. `showcase.sh up
--guest nixos --display` returns an error naming that.

**The torch venv is not in the image**, and cannot be: the gate's torch
stage compares against the host's reference venv (`vendor/hostvenv`, torch
2.13.0+cu130), so it has to be the same pip wheels rather than nixpkgs'
torch. `up --with-torch` creates it, and a manylinux wheel then needs a C++
and OpenMP runtime that NixOS supplies to nobody — the image declares one at
`/opt/nvrm/wheel-runtime` (`libstdc++`, `libgcc_s`, `libgomp`, `libz`; found
by running the gate and reading off what failed, not by listing what might
be needed). Because the venv is a 2.5 GiB download per guest, a fleet
overlays `LEA_NIXOS_FLEET_BASE`, a frozen provisioned disk — and unlike the
Ubuntu `LEA_FLEET_BASE`, its absence is not an error, only slower.

Measured on four NixOS guests, one RTX 2070, `convburn` 100 iterations
(2026-08-19, `governor=powersave`, `persistenced=0` — absolute numbers are
therefore high; the shape is the result):

| VMs | ms/it | aggregate it/s | fairness % |
|---|---|---|---|
| 1 | 23.54 | 42.48 | — |
| 2 | 40.24 | 49.43 | 1.1 |
| 4 | 80.79 | 49.56 | 3.3 |

with **one** distinct `acc` across all stages and all VMs
(`1.065710664e+00`), which is the invariant the bench exists to check.
