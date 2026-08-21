<!-- SPDX-License-Identifier: MIT -->
# Future work

What is not built, and how someone could build it. Everything here is a
sketch unless it says otherwise — the measured state of the project is in
[`../README.md`](../README.md), and what is unresolved is in
[`OPEN-QUESTIONS.md`](OPEN-QUESTIONS.md).

The first section is committed work; everything after it is ordered roughly
by how much it would change for a user.

## Committed while the thesis runs

These three were committed as what
[MeisterStack](https://github.com/UniStuttgart-IKR/MeisterStack) — the VM
orchestrator this project came out of — would need in order to treat it as a
component rather than as a rig. Two have since landed; the README lists
them as the roadmap with the same status.

### NixOS package and module
**Delivered.** The flake packages the host side and exports
`nixosModules.default` (backend environment, bridge, taps, NAT, driver
persistence) and `nixosModules.guest`; `nix/guest-image.nix` builds a
bootable, pre-provisioned guest image as a derivation, and a NixOS guest
passed the compute gate on 2026-08-19. [`DEVELOPMENT.md`](../DEVELOPMENT.md)
section 10 is the reference; `nix/shell.nix` survives as a thin
flake-compat shim.

### SLURM, and reproducible tests
**Delivered.** `build.sh package` produces a self-contained Apptainer
package; `bench.sh slurm` packages, submits (a real `sbatch`), runs and
collects the gates and benchmarks as SLURM jobs — verified end to end on
a single-node SLURM on 2026-08-19, the compute gate green out of the
package. [`DEVELOPMENT.md`](../DEVELOPMENT.md) section 9a is the
reference. What it exists for: a measurement someone else can repeat on
their own hardware is evidence, a number in a README is not — the fleet
numbers (four VMs, 3.9x, bit-identical results) are now checkable.

**The rule this established, now in force:** no benchmark enters the repository until it
is reproducible. Everything measured so far comes from one rig and one
person, and is labelled that way. A performance claim becomes publishable
when it ships with the job definition, the raw output and the conditions —
so that disagreeing with it means re-running it rather than taking someone's
word. Anything that cannot meet that stays a single-rig measurement, which
is a perfectly honest thing to be, just not a benchmark.

### A deployment story
**Open, and smaller than it was.** The flake module (NixOS hosts), the
guest-image derivation and the Apptainer package cover the pieces; what
is missing is the guided path — a third party should not have to read
`DEVELOPMENT.md` end to end to get from a clone to a guest with a GPU.

## Near term

### Zero-copy capture for the display path
**The biggest single win available.** The capture path is still an X11
software grab: Xorg at 24 % and Sunshine at 53 % CPU at 60 fps, and
stopping Sunshine takes Xorg to 0 %. The frames already exist on the GPU;
they are read back, encoded, and sent.

*How:* Sunshine's `kms` capture with a dmabuf handed straight to NVENC,
which is the route it uses natively. It currently fails under our path
(number 17), and the blocker is understood as the compositor not
repainting rather than the capture itself. Fixing number 35 is likely a
precondition.

### Fold the helper module into `virtio_nvrm`
Today both guest modules must be loaded, because `/proc/driver/nvidia`
belongs to `nvrm_nodes.ko`.

*How:* let the host deliver the proc contents over the device. That is a
protocol extension with a design of its own — the contents are read from
the real host file, so the question is only how they travel and when they
refresh. Once that exists, `nvrm_nodes.ko` becomes unloadable and
`virtio_nvrm.ko` runs alone.

### Persistence across guest kernel updates
`nvrm-setup.sh --persist` already persists the whole coexistence order —
`nvrm_nodes.ko create_nodes=0`, params, then `virtio_nvrm.ko` — through
`nvrm-boot.sh` and a oneshot unit (deliberately not `modules-load.d`,
which cannot guarantee the parameter arrives before provisioning), and
the NixOS guest module does the same declaratively. What is not built is
surviving a guest kernel update: the modules are compiled in the guest
against the running kernel and a new kernel needs a rebuild the boot
unit does not yet do.

### Beyond 1080p60
1440p and refresh rates above 60 Hz are untried.

*How:* `vdisplay_vblank_hz` and the EDID are where it starts. The EDID is
derived from the requested size rather than tabulated;
`scripts/test.sh check` sweeps it from 800x600 to 4K through
`edid-decode`, and `test/edidclamp.c` verifies the clamp arithmetic to
8K/240 Hz, so the block itself should hold. What is unknown is whether
the event rate and the capture path follow.

## Medium term

### Make the backend visible on the host
`nvidia-smi` on the host shows the backend process, but its per-process
VRAM figure reads 0 MiB, because the host's own accounting does not see
allocations the guest made through us.

*How:* the backend already keeps the ledger that would answer this
(`crates/vhost-user-nvrm/src/vram.rs`). It is a question of surfacing it,
not of measuring it.

### Multiple driver versions at once
Everything is pinned to the single version in `DRIVER_VERSION`, and
`assert_driver_version()` panics rather than let a mismatch misread struct
offsets. That is honest, and it means guest and host must match exactly.

*How:* the descriptor tables would have to become version-indexed rather
than version-locked — the host already knows which version it is serving,
so it could send the table for that version. The hard part is not the
protocol but keeping several sets of `xlate.rs` knowledge correct at once,
and having a card and a driver to test each against.

### More architectures than Turing
Only Turing has ever run this. The class tables cover Fermi through
Blackwell because they are derived mechanically from the driver's
`resource_list.h`, but derived is not tested.

*How:* run the GPU gate on an Ampere, Ada or Blackwell card. Unverified
classes are already flagged in the descriptor table and the host logs the
first use of each, so a run on new silicon produces a list of exactly what
to look at. This needs hardware more than it needs design.

### Multi-GPU guests
Not implemented. One card per backend, one backend per VM.

*How:* the descriptor tables and the session are not GPU-indexed today.
This is a real design change, not a parameter.

### Snapshot, suspend, resume, migration
None of it works.

*How:* not something this project can decide alone — it is an open
construction site on the cloud-hypervisor side for vhost-user devices
generally. Worth tracking rather than starting.

### QEMU instead of cloud-hypervisor
Unbuilt and unmeasured. The device is a generic vhost-user device, so in
principle nothing about it is cloud-hypervisor specific; in practice the
SHMEM patch is, and QEMU's vhost-user-device support would have to be
checked against what the window needs.

### Make the VRAM cap a cap, or say it is not one
**Answered on the `vram` branch, and neither of the two ways this entry
proposed.** The cap covers device memory allocations that cross the
boundary as requests. It does not cover RM's own device memory behind a
channel (context buffers, USERD), which is about 214 MiB under a 4 GiB
cap, and it does not cover managed memory, which is pinned guest RAM.
Accounting for the invisible half means finding a door for allocations we
never see, and there is none: RM allocates that memory on its own side.

*What was done instead:* a second, opt-in policy that RESERVES rather than
counts (`LEA_VRAM_PROFILE_MIB`, OPEN-QUESTIONS 68). The configured number
is what the VM may cost the CARD; a reservation comes off it first
(`LEA_VRAM_RESERVE_MIB`, 256 MiB by default against a measured ~175 MiB of
per-backend overhead) and what is left is what the guest is told and what
the guest may allocate. That does not make the invisible half visible — it
makes room for it, from a measurement, up front, which is the same thing
NVIDIA's vGPU does with `fbReservation`.

*What is still open here:* managed memory is still pinned guest RAM and
still outside both policies; and the reservation is a constant measured on
one driver, one card and one workload, so it is a knob rather than a
derived quantity.

### Cross-tenant admission control and scheduling: not here
**Decided, and the decision is that these belong to a CONSUMER of this
project** (working name *MeisterStack*), not to this repository. This repo
ships functionality; a product decides policy with it.

Neither is a matter of effort:

- **Admission control** needs a view of every VM on the card at once. One
  backend serves one VM and has no path to a sibling, and giving it one
  means a host daemon with an API and a lifetime of its own — a different
  program, and one that would own the placement policy as well.
  **Overprovisioning is therefore allowed here, deliberately**: nothing
  refuses a set of profiles that sums past the card, `lea_backend_start`
  only warns when it can see that it does. OPEN-QUESTIONS 67 is what
  overprovisioning looks like when it goes wrong, and it is the reason the
  warning exists at all.
- **Scheduling** needs the runlists. vGPU's scheduler works because the
  host driver owns them and preempts between them; this backend forwards
  ioctls into the host's single RM context and never sees a runlist. Its
  controls are at least reachable —
  `NV2080_CTRL_CMD_FIFO_OBJSCHED_GET_STATE`/`SET_STATE` carry
  `flags = 0x48 = ROUTE_TO_PHYSICAL | NON_PRIVILEGED`, so unlike numbers 19
  and 25 they are not behind the kernel-privilege wall — but reaching a
  control is not the same as owning what it configures.

## Longer term, and speculative

### `nvrm-remote`: the card in another machine
The design sketch, in one sentence: the same interception, but the
transport goes over the network to a machine that holds the GPU.

It does **not** follow trivially from the existing design. The shared
memory window is the problem — it is exactly what a network cannot carry
(see [`llm.md`](llm.md) on why a copying fallback is not a degraded mode
of this architecture). The open question that decides the shape is whether
`TURING_USERMODE_A` can be registered as an RDMA memory region through
`nvidia_peermem`. If it can, the design roughly halves.

Unmeasured and needed before anyone starts: how often `libcuda` reads
system memory outside the `poll()` wait path, how many submissions a
launch and a graph replay actually make, and what a trap costs locally.

### Requirements by measurement, not by chase
The method so far has been reactive: run a workload, hit the first
refusal, fix it, repeat. It works, and it does not tell you what is
missing until something asks for it.

The alternative is to enumerate the interface and measure coverage: which
escapes and classes exist, which are exercised by the gates, which are
marked unverified in the descriptor table. That number exists today only
as "the host logs a line the first time a guest uses an unverified
class". Turning it into a coverage figure would say how complete this is
without waiting for a program to complain.

## Working method

It applies to everything above. Measure first, then build. Hold every
status code against the direct native run. Keep measured and conjectured
strictly apart. Record dead ends together with the reason, so nobody walks
them twice — that is what [`llm.md`](llm.md) section 5 is for.
