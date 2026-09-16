// Copyright (C) 2026 Tencent. All rights reserved.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//   http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing terms and
// limitations under the License.

//! Optional embed of the Linux `rthas-ebpf` helper into the CLI.
//!
//! Set `RTHAS_EMBED_EBPF` to the helper binary when building a release
//! artifact so `rthas attach --ebpf` works from a single file.

fn main() {
    println!("cargo:rerun-if-env-changed=RTHAS_EMBED_EBPF");
    println!("cargo:rustc-check-cfg=cfg(embed_ebpf)");

    if std::env::var("CARGO_CFG_TARGET_OS").ok().as_deref() != Some("linux") {
        return;
    }
    let Ok(raw) = std::env::var("RTHAS_EMBED_EBPF") else {
        return;
    };
    if raw.is_empty() {
        return;
    }
    let path = std::path::PathBuf::from(&raw);
    let abs = if path.is_absolute() {
        path
    } else {
        std::env::current_dir()
            .expect("cwd")
            .join(path)
    };
    if !abs.is_file() {
        panic!(
            "RTHAS_EMBED_EBPF is set but {} is not a file",
            abs.display()
        );
    }
    let canon = abs.canonicalize().unwrap_or(abs);
    println!("cargo:rerun-if-changed={}", canon.display());
    println!("cargo:rustc-cfg=embed_ebpf");
    println!("cargo:rustc-env=RTHAS_EMBED_EBPF_PATH={}", canon.display());
}
