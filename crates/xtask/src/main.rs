// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
// SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
//! Generate and verify the versioned NVIDIA ABI bindings.
//!
//! ```text
//! cargo xtask abi [--check] [--report PATH]
//! ```

mod abi;

use anyhow::{bail, Result};

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("abi") => abi::run(&args[1..]),
        Some("-h") | Some("--help") | None => {
            println!("{USAGE}");
            Ok(())
        }
        Some(other) => bail!("unknown task: {other}\n\n{USAGE}"),
    }
}

const USAGE: &str = "\
cargo xtask <task>

  abi [--check] [--report PATH]
        Regenerate the multi-version NVIDIA RM bindings from
        crates/nvrm-sys/abi.toml and the headers under
        vendor/nvidia-rm-headers/.

        --check       do not write; fail if anything would change
        --report PATH write the pairwise classification table here
                      (default: print it)
        --config PATH an abi.toml to read instead of the crate's own. For
                      trying a candidate driver version out before the
                      repository claims to support it.
        --manifests DIR
                      write the manifests here instead of into the crate.
        --dump-bindings DIR
                      also write each version's raw bindgen output here.
                      A diagnostic, not an input to anything.
";
