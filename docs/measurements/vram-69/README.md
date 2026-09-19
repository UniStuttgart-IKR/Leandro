<!-- SPDX-License-Identifier: MIT -->
# Four-VM VRAM policies (question 69)

Historical density test: four guests on one card, each with 1536 MiB RAM and
2 vCPUs, running `vrampress --max --seconds 120` concurrently. Host sampling: 1 Hz.
The runner reserves at least 4096 MiB host RAM.

| Row | Policy | Guest-visible capacity |
|---|---|---|
| `n4-off` | None | 8192 MiB |
| `n4-limit` | `LEA_VRAM_LIMIT_MIB=1280` | 1280 MiB |
| `n4-profile` | `LEA_VRAM_PROFILE_MIB=1536` | 1280 MiB |
| `n4-grid` | `LEA_VGPU_TYPE=2Q` | 1280 MiB, five VMMU segments |

- Capped rows expose the same capacity. Profile policy includes an overhead allowance;
  it does not guarantee a bound on all driver-owned GPU memory.
- The GRID row applies admission checks and reports `Leandro RTX2070-2Q`.
- `bisect/bisect.txt` tests six `LEA_VGPU_MEDIATE` combinations with one guest.
  Reporting vGPU mode affects CUDA even when `nvidia-smi` succeeds; see
  [the follow-up traces](../vram-69b/README.md).

| Files | Contents |
|---|---|
| `<row>/host-1hz.csv` | Card use/free/utilization, available RAM and VM charges |
| `<row>/vrampress-vm*.csv` | Per-guest allocator logs |
| `<row>/catalogue.txt` | Profiles reported at run start |
| `<row>/run.txt`, `<row>/policy.txt` | Invocation and backend policies |
| `vgpubench.sh`, `benchsum.py` | Historical runner and analysis |
