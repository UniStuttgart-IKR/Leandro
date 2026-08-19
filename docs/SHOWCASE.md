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
  nothing tells it otherwise -- see the black-screen entry in section 6.

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

---

## 5. Streaming it

```
./scripts/showcase.sh pair
moonlight stream 192.168.100.15 "Steam Big Picture" --resolution 1920x1080 --fps 60 --bitrate 40000
```

`pair` does what the web UI does for a human: `moonlight pair --pin` and a
POST to Sunshine's `/api/pin` have to overlap. It is idempotent, and it
repairs the one thing it can -- a Sunshine with no web login answers every
API call with a 307 to its `/welcome` page (see section 6).

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

## 6. When it does not work

**Black stream, everything else green.** Sunshine chose NvFBC. It is
NVIDIA's own frame capture, restricted on GeForce: it initialises, logs
`Couldn't release NvFBC context`, encodes with NVENC and sends black
frames -- every line reads like success. Measured 2026-08-19: 789373
non-black pixels on the guest's X root while the stream showed nothing.
The fix is in the tree (a capture setting is always written now); if you
meet it on an older guest, `showcase.sh up ... --keep-vm --session gnome`
rewrites the configuration and restarts Sunshine.

**`moonlight pair` times out.** Sunshine has no web login, so its API
answers `307` to `/welcome` and the PIN can never arrive. `showcase.sh
pair` detects exactly this, writes the login and restarts Sunshine once.

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

## 7. Taking it down

```
./scripts/showcase.sh down --all
./scripts/showcase.sh clean --dry-run     # only after a crash; then without --dry-run
```

`clean` refuses while an instance is up -- a running rig is not garbage --
and `--dry-run` prints what it would remove.

---

## 8. What this shows, and what it does not

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
