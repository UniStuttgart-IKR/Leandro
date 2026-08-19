// SPDX-License-Identifier: MIT
// SPDX-FileCopyrightText: 2026 Silas Müller <github@silasmueller.de>
// SPDX-FileCopyrightText: 2026 Universität Stuttgart, IKR
//! vhost-user-input - host daemon serving virtio-input over a socket.
//!
//! The second backend of the display rig, beside vhost-user-nvrm. Why it
//! has to exist at all -- neither cloud-hypervisor nor crosvm supplies
//! this end -- is the crate header's story (`lib.rs`).

use anyhow::Result;

use vhost_user_input::{serve, SourceSpec};

fn usage() -> ! {
    eprintln!(
        "vhost-user-input --socket <path> (--evdev <dev> | --fifo <path>) [--name <name>]\n\
         \n\
         --evdev:  forward a host input device verbatim (/dev/input/eventN)\n\
         --fifo:   read `type code value` lines; created if absent. Scriptable,\n\
                   which is what lets a gate press a key without a human.\n\
         \n\
         Example -- press and release KEY_A in the guest:\n\
           printf '1 30 1\\n0 0 0\\n1 30 0\\n0 0 0\\n' > vm/input.fifo"
    );
    std::process::exit(2);
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut socket = None;
    let mut source = None;
    let mut name = "leandro virtual input".to_string();

    let mut i = 0;
    while i < args.len() {
        let val = || -> Option<String> { args.get(i + 1).cloned() };
        match args[i].as_str() {
            "--socket" => socket = Some(val().unwrap_or_else(|| usage())),
            "--evdev" => source = Some(SourceSpec::Evdev(val().unwrap_or_else(|| usage()).into())),
            "--fifo" => source = Some(SourceSpec::Fifo(val().unwrap_or_else(|| usage()).into())),
            "--name" => name = val().unwrap_or_else(|| usage()),
            _ => usage(),
        }
        i += 2;
    }

    let (Some(socket), Some(source)) = (socket, source) else { usage() };
    // A leftover socket from a killed run would make bind fail with
    // EADDRINUSE, which reads like "another backend is running" and usually
    // is not. Same handling as vhost-user-nvrm.
    let _ = std::fs::remove_file(&socket);
    serve(&socket, &source, &name)
}
