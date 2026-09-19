<!-- SPDX-License-Identifier: MIT -->
# nvrm-sys

- Committed bindgen output and layout metadata for configured NVIDIA driver versions.
- Consumers use the committed sources; normal builds need neither libclang nor
  vendored headers.
- Version policy and hardware evidence: [driver versions](../../docs/abi-versions.md).

## Source map

| File | Responsibility |
|---|---|
| `abi.toml` | Maintained version entries, footprint and layout acknowledgements |
| `manifests/<version>.json` | Generated sizes, alignments and field offsets |
| `manifests/classification.md` | Generated differences between versions |
| `versions.toml` | Generated manifest hashes and module mapping |
| `src/stable.rs` | Definitions shared across configured versions |
| `src/v<NNN>.rs` | Version-specific definitions |
| `src/lib.rs` | Generated `DriverVersion`, `RmAbi`, dispatch and assertions |
| `src/version.rs` | Handwritten running-driver detection |

## Regeneration

```sh
tools/build.sh vendor-abi <version>
cargo xtask abi
cargo xtask abi --check
```

- Regeneration requires libclang and the versioned headers.
- Edit `abi.toml` or the generator, not generated Rust or manifests.
- Features are additive; software checks compile each version and all versions.
- `detect()` accepts exact configured versions enabled in the build.
  `assert_driver_version()` requires the default version.
- Layout checks establish ABI structure, not end-to-end GPU compatibility.
- Generated bindings are excluded from workspace rustdoc and doctest checks.
