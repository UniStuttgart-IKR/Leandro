<!-- SPDX-License-Identifier: MIT -->
# The showcase, end to end

What to install, what to run, in which order, and what each step must
print. This is the path from a fresh clone to a desktop guest streaming a
game off a card the guest does not own -- and to the smoke test beside it.

Confidence markers as elsewhere in this repository: "Measured:" marks
something measured or read in source, "Unverified:"/"presumably" marks
conjecture, "WARNING:" marks a trap that has already bitten someone.

The reference for what the gates cover is
[`TESTING.md`](TESTING.md); the display path itself is
[`DISPLAY.md`](DISPLAY.md). This file is the operator's order of
operations and nothing else.

---

## 1. What has to be there

**On the host.** The preflight checks all of this by name
(`scripts/build.sh preflight`) and says which package it comes from, so
the list below is what it will ask for rather than a second list to keep
in step:

| | Why |
|---|---|
| NVIDIA driver + `nvidia-smi` | the card stays with the host; there is no passthrough |
| `libcuda.so.<version>` | the guest loads the HOST's libcuda, not merely one of the same version |
| `git`, `cargo`, `cc`, `make`, `pkg-config` | the workspace and cloud-hypervisor are built here |
| `qemu-img`, `mkfs.vfat`, `mcopy` | guest disk and the cloud-init seed |
| `curl`, `ip`, `iptables` | image download; bridge, taps and the NAT rule |
| `sudo` (ideally passwordless) | bridge, taps, module load in the guest |
| KVM (`/dev/kvm`) | cloud-hypervisor runs the guest |

**For the display half, additionally:**

| | Why |
|---|---|
| `moonlight-qt` on the HOST | the receiver. Arch: `pacman -S moonlight-qt` |
| a guest image baked `--with-desktop` | GNOME, Xorg and Sunshine live in the image, not in a script |
| Sunshine in the guest at the HOST's version | `build.sh bake --with-desktop` installs the version this host runs, deliberately |

**For `test.sh gpu` (the compute gate), additionally:**

| | Why |
|---|---|
| `vendor/hostvenv` with torch | the gate compares the guest against a NATIVE run of the SAME torch. `uv venv --python 3.12 vendor/hostvenv && uv pip install --python vendor/hostvenv/bin/python torch==2.13.0 numpy` |
| persistence mode on | `sudo nvidia-smi -pm 1`; off, the native reference shifts by up to 58 % |

WARNING: no passthrough, no IOMMU groups, no vGPU licence, and **no second
card needed**. The host keeps using the GPU while the guest computes on it;
that is the point of the project and not a limitation of the demo.

### Which cards this runs on

Measured on a GeForce RTX 2070 (Turing, sm_75). Three things are
card-dependent and each says so rather than failing quietly:

- **PTX architecture.** `probe/kernels/kernels.ptx` is versioned so that a
  checkout needs no `nvcc`, and PTX JITs **forward only**. `make -C probe
  ptx` asks the card (`nvidia-smi --query-gpu=compute_cap`) and regenerates
  the file only when the committed one cannot run here -- a no-op on
  anything Turing or newer, an `nvcc` build on anything older.
- **NVENC.** The desktop path asks for `nvenc` and reads back what Sunshine
  actually found. A card without an encoder gets a warning naming
  `LEA_SUN_ENCODER=software` rather than a silent software stream that
  someone might benchmark.
- **NvFBC** is restricted on GeForce, and Sunshine picks it by itself when
  nothing tells it otherwise -- see the black-screen entry in section 7.

---

## 2. From a fresh clone

```
git clone <repo> Leandro && cd Leandro
./scripts/build.sh preflight --driver auto
./scripts/build.sh all --driver auto
make -C probe ptx
./scripts/test.sh check
```

`--driver auto` is the line that matters on someone else's machine: it
takes the version of the RUNNING driver instead of the pin in
`DRIVER_VERSION`, and fetches the vendor headers to match. Without it
`showcase.sh state --check` stops with `RIG-ERROR: driver mismatch`, and
it is right to -- every struct offset in this tree is read out of those
headers.

`test.sh check` is fourteen steps, GPU-free, about two seconds warm. It
must be green before a VM is started; it is the only thing here that
proves the host side is internally consistent.

---

## 3. The rig

```
sudo nvidia-smi -pm 1
./scripts/showcase.sh state --check
./scripts/showcase.sh net up
```

`state --check` prints the RIG line -- driver, persistence, governor,
cloud-hypervisor version, PCIe link, card -- and refuses on a mismatch.
Every measurement in this repository carries that line beside it, which is
what makes two runs comparable.

`net up` builds the bridge, the taps and one NAT rule. It is a
precondition of the `ip` transport and needs `sudo`; the `vsock`
transport needs none of it (NixOS guests only).

---

## 4. Two guests, one card

```
./scripts/showcase.sh up --name desktop --index 5 --session gnome --with-steam --with-torch
./scripts/showcase.sh up --with-torch
./scripts/showcase.sh status
```

The first is the desktop guest (GNOME on the virtual display, Sunshine
beside it, Steam in the session); the second is `vm0`, plain compute. Both
load `virtio_nvrm.ko` and neither has an NVIDIA kernel driver of its own.

`--with-torch` creates the guest's torch venv if the image does not carry
one. It downloads ~2.5 GiB per guest, so it is opt-in -- but without it
`rlprobe.py` and `convburn.py` cannot run in that guest, and those are the
first two things anyone tries. A fleet member (`up --count N`) overlays
`LEA_FLEET_BASE`, which already has the venv; `vm0` does not, which is why
it is spelled out here. The provisioning line says which of the two states
each guest ended in.

Measured: an interactive `showcase.sh ssh` lands in a shell that has
`LD_LIBRARY_PATH`, `NVPROBE_PTX` and the venv's python set (the guest's
`.bashrc`; `ssh <guest> <command>`, which is how every gate reaches it,
reads neither that file nor `.profile` -- so nothing measured moves
because something is convenient). In that shell:

```
./scripts/showcase.sh ssh --name desktop
  cd gpu && python rlprobe.py        # or: nvprobe 3
```

`status` must show `up up up` and `MODULE loaded` for both.

### A game, downloaded once

A game is tens of gigabytes and is not system state, so it lives on its own
disk rather than in the image or on a root disk that `--fresh` throws away:

```
./scripts/showcase.sh games init                 # one qcow2, sparse, 200 G
./scripts/showcase.sh up --name desktop --index 5 --session gnome --games-init
#   in the guest: start Steam (nvidia-run steam), log in, install the game
./scripts/showcase.sh down --name desktop
```

After that every guest gets a thin overlay on it, and two guests can run
from the same library AT THE SAME TIME because neither writes to the base:

```
./scripts/showcase.sh up --name desktop --index 5 --session gnome --games
./scripts/showcase.sh games status
```

It is mounted at `/games`, and every Steam root in the guest gets its
`steamapps` symlinked there -- including roots that do not exist yet, so the
first start of Steam already lands on the disk. That indirection is not
decoration: Ubuntu's Steam package keeps its root at
`~/.steam/debian-installation` and never looks at `~/.local/share/Steam`, so
a disk mounted at the latter stays empty at 186 GiB free while the guest's
38 GiB root disk fills up with Proton, the Steam runtime and a game
(measured 2026-08-20, and it is why the mount is where it is).

A `steamapps` that already has content is never touched -- moving a live
library is the operator's call, and the provisioning output prints the three
commands for it.

`scripts/guest/cs2-settings.sh` in the guest writes CS2's settings at their
lowest and caps `fps_max` at the display rate -- the frame cap is the one
that matters here, because every frame above the virtual display's 60 Hz is
rendered, captured and then thrown away.

---

## 5. Streaming it

```
./scripts/showcase.sh pair
moonlight stream 192.168.100.15 Desktop --resolution 1920x1080 --fps 60 --bitrate 40000
```

`pair` does what the web UI does for a human: `moonlight pair --pin` and a
POST to Sunshine's `/api/pin` have to overlap. It is idempotent, and it
repairs the one thing it can -- a Sunshine with no web login answers every
API call with a 307 to its `/welcome` page (see section 7).

The desktop bring-up prints what Sunshine settled on, read out of its own
log rather than assumed:

```
  sunshine: capture=x11 encoder=nvenc
  Info: Screencasting with X11
  Info: Found H.264 encoder: h264_nvenc [nvenc]
```

Those two lines are where a black stream is decided. Read them.

### On Wayland

The desktop this tree brings up is X11 by construction: `lea_desktop_up`
writes `WaylandEnable=false` into gdm3's configuration, and `xorg` is the
session every number in [`DISPLAY.md`](DISPLAY.md) was measured on. A
Wayland run is a different guest image and a different capture path:

```
./scripts/build.sh bake --with-desktop --desktop-session wayland --with-steam --set-default
./scripts/showcase.sh up --name desktop --index 5 --session gnome --fresh
LEA_SUN_CAPTURE=portal ./scripts/showcase.sh pair       # capture=auto finds it too
```

`LEA_SUN_CAPTURE=auto` (the default) asks the guest: a Wayland socket
means `portal` -- xdg-desktop-portal plus pipewire, which carries real
desktop content -- and anything else means `x11`.

WARNING: Measured -- an **unpatched Sunshine stops at the portal's
permission dialog**, which nobody can click on a guest nobody is sitting
in front of, and `capture = kms` under Wayland has shown a black stream
with a live cursor ([`OPEN-QUESTIONS.md`](OPEN-QUESTIONS.md) no. 17, still
open). Treat the Wayland path as an experiment with someone watching, not
as the unattended smoke test.

---

## 6. The three caps, and what a run does without them

Nothing in the chain above sets a cap. That is deliberate -- the default is
a VM with the whole card -- but it means the defaults are what a
demonstration runs into, and there are three of them, all independent:

| | where | default | what it bounds |
|---|---|---|---|
| `LEA_VRAM_LIMIT_MIB` / `--vram-limit N` | backend, per VM | **off** | VRAM this VM may hold |
| `max_pin_mib` / `--max-pin-mib N` | guest module | **1024 MiB** | pinned guest memory, cumulative |
| `LEA_MAX_PIN_MIB=N` | backend, per arena | **256 MiB** | one pinned region |

The third has no flag and is set as an environment variable on `up`. It is
also the tightest: a single pinned region above 256 MiB is refused, and the
refusal reaches the guest as a failed mapping.

Measured 2026-08-20 across the VRAM axis (`vramcap.py`, `convoom.py`,
`nvprobe`) at off / 4096 / 2048 / 1024 MiB: every cap holds, every workload
either completes or fails with a clean CUDA OOM, and no run produced a
kernel warning, an oops or a lost guest. Under a cap `torch` reports the
capped size as the card's total (2048 MiB cap -> "total: 2147483648"), the
allocation stops below it, and -- the question a VRAM cap actually lives or
dies on -- the counter comes back DOWN after a free, so the same guest can
allocate again.

### The pin caps have a window, and both walls are quiet

The two pin caps are the part to get right before running a game, and
neither wall announces itself in the guest.

**Too low.** A single pinned region above `LEA_MAX_PIN_MIB` is refused, and
`pinwin.py` simply prints no line for that size: no traceback, no kernel
message, nothing in the guest at all. Only the backend log counts it.
Measured 2026-08-20 with the defaults: a 512 MiB pin fails; with
`LEA_MAX_PIN_MIB=1024` the same pin reports "512 MiB pinned ok
roundtrip=correct". Raising the guest module's `max_pin_mib` alone changes
nothing -- it was 3072 in the failing runs. This restates a measurement
from 2026-08-16 in [`llm.md`](llm.md), which is where the error code lives
(`cudaHostRegister` returns 304, `cudaErrorOperatingSystem`).

**Too high.** Pinned pages cannot be swapped, so the guest's RAM becomes
the real limit. With both caps raised and a workload that holds every pin,
the guest's OOM killer took the workload: "Out of memory: Killed process
(python) shmem-rss:2846592kB" in a guest with 4 GB. Nothing crashed -- no
oops, no BUG, SSH still answering, 3.5 GB free afterwards -- the kernel did
exactly what it should. But the process died, and a game dying that way
looks like a game bug.

So the caps have to be set together with `--mem`. The desktop guest gets
16 GB by default (`LEA_DESKTOP_MEM`) and a compute guest 4 GB, which is why
a pin cap that is fine on one is not automatically fine on the other:

```
LEA_MAX_PIN_MIB=2048 ./scripts/showcase.sh up --name desktop --index 5 \
    --session gnome --games --max-pin-mib 4096
```

## 7. When it does not work

**Black stream, everything else green.** Sunshine chose NvFBC. It is
NVIDIA's own frame capture, restricted on GeForce: it initialises, logs
`Couldn't release NvFBC context`, encodes with NVENC and sends black
frames -- every line reads like success. Measured 2026-08-19: 789373
non-black pixels on the guest's X root while the stream showed nothing.
The fix is in the tree (a capture setting is always written now); if you
meet it on an older guest, `showcase.sh up ... --keep-vm --session gnome`
rewrites the configuration and restarts Sunshine.

**Moonlight lists "Steam Big Picture" and it starts nothing.** That entry
is Sunshine's own default application list, not a statement about the
guest. An image baked without `--with-steam` has no Steam to start, and
the entry stays anyway. Measured 2026-08-19: `command -v steam` empty in a
guest whose Moonlight list offered it. Stream `Desktop` instead, or bake
with `--with-steam`.

**`moonlight pair` times out.** Sunshine has no web login, so its API
answers `307` to `/welcome` and the PIN can never arrive. `showcase.sh
pair` detects exactly this, writes the login and restarts Sunshine once.

**A Vulkan application in the guest cannot create a swapchain, or a game
will not start, after the desktop has been up for a while.** The backend
ran out of file descriptors. It opens a real `/dev/nvidiactl` or
`/dev/nvidia0` for every RM client the guest creates -- that is the design,
the mirror keeps the FDs and hands the guest tokens -- and a desktop
session with Steam and its dozen helpers reaches hundreds. Measured
2026-08-20: 913 open FDs against the 1024 soft limit, after which every new
client got `EMFILE` and the guest saw
`vkCreateSwapchainKHR VK_ERROR_INITIALIZATION_FAILED` with nothing anywhere
naming a file descriptor. The backend now starts with a 65536 soft limit
(`LEA_NOFILE` overrides it); before the fix one `vkprobe` in five
succeeded, after it eight of eight. If you meet it anyway, the backend log
says it plainly: "open /dev/nvidia0: Too many open files".

**A backend refuses to start: `driver mismatch: running X, bindings for
Y`.** The host driver changed and the binaries did not: `nvrm-sys` derives
its bindings from the vendor headers and asserts the running version at
startup, so a stale backend says so instead of talking to the driver with
old offsets. `./scripts/build.sh cargo`. After a driver change the guest
side needs the same treatment -- `test.sh vdisplay --fresh`, because the
guest builds `nvidia-modeset.ko` and `nvidia-drm.ko` from the host's
sources and a kept overlay carries the old ones.

**`nvidia-smi` says `Driver/library version mismatch`.** The packages were
updated and the machine was not rebooted: libcuda is the new version and
the loaded kernel module is the old one. Nothing in this tree can work in
that state. Reboot -- a module reload only helps if nothing holds the card,
which on a desktop is never true.

**`RIG-ERROR: driver mismatch`.** The tree targets `DRIVER_VERSION` and
the host runs something else. `./scripts/build.sh all --driver auto`, or
`--driver <version>` for a specific one.

**`RIG-ERROR: persistence mode is off`.** `sudo nvidia-smi -pm 1`. It is
an error rather than a warning because it moves the native reference by up
to 58 %.

**A probe dies in the JIT on an older card.** The committed PTX is newer
than the card; `make -C probe ptx` regenerates it (needs `nvcc`).

**The torch stage of `test.sh gpu` fails with `vendor/hostvenv missing`.**
The native reference is not installed -- section 1 has the two lines.

**`--with-torch` refused on a vsock guest.** A vsock guest has no network
device at all, so pip has nowhere to fetch from. Overlay a base that
already carries the venv; the error names the command.

---

## 8. Taking it down

```
./scripts/showcase.sh down --all
./scripts/showcase.sh clean --dry-run     # only after a crash; then without --dry-run
```

`clean` refuses while an instance is up -- a running rig is not garbage --
and `--dry-run` prints what it would remove.

---

## 9. What this shows, and what it does not

The demonstration `./scripts/showcase.sh demo --fast --pause` walks the
same ground with a verdict per section and prints every command before it
runs. What it proves is bounded, and the bounds are worth saying out loud
to anyone being shown this:

- **Proven here:** an unmodified CUDA application in a VM computing on a
  card the guest owns no driver for; a desktop and a compute guest sharing
  it; the accounting surviving a `kill -9`; the module refusing to unload
  while an FD is open.
- **Not proven here:** anything about security isolation beyond the VM
  boundary, performance parity (sharing cost 15-20 % of throughput,
  measured), or the display path under Wayland.

The open questions are all written down, including the ones that make this
look worse: [`OPEN-QUESTIONS.md`](OPEN-QUESTIONS.md).
