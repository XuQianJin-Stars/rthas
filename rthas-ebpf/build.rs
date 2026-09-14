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

fn main() {
    println!("cargo:rerun-if-changed=../rthas-ebpf-prog");
    println!("cargo:rerun-if-changed=../rthas-ebpf-prog/src");
    println!("cargo:rerun-if-changed=../rthas-ebpf-prog/Cargo.toml");

    if std::env::var("CARGO_CFG_TARGET_OS").ok().as_deref() != Some("linux") {
        return;
    }

    if let Err(e) = build_ebpf() {
        panic!(
            "failed to compile rthas-ebpf-prog: {e}\n\
             eBPF attach needs nightly rustc, bpfel-unknown-none, and bpf-linker:\n\
               rustup toolchain install nightly\n\
               rustup component add rust-src --toolchain nightly\n\
               rustup target add bpfel-unknown-none --toolchain nightly\n\
               cargo install bpf-linker"
        );
    }
}

fn build_ebpf() -> Result<(), String> {
    use std::path::PathBuf;
    use std::process::Command;

    let manifest_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let prog = manifest_dir.join("../rthas-ebpf-prog/Cargo.toml");
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let target_dir = out.join("bpf-target");

    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let mut cmd = Command::new(&cargo);
    cmd.args([
        "+nightly",
        "build",
        "--release",
        "--manifest-path",
    ])
    .arg(&prog)
    .args(["--target", "bpfel-unknown-none", "-Z", "build-std=core"])
    .env("CARGO_TARGET_DIR", &target_dir);
    // Nested cargo deadlocks if it reuses the parent's rustflags encoding.
    cmd.env_remove("CARGO_ENCODED_RUSTFLAGS");
    cmd.env_remove("RUSTFLAGS");

    let status = cmd
        .status()
        .map_err(|e| format!("spawn cargo +nightly: {e}"))?;
    if !status.success() {
        return Err(format!("cargo +nightly exited {status}"));
    }

    let built = target_dir
        .join("bpfel-unknown-none")
        .join("release")
        .join("rthas-ebpf-prog");
    if !built.is_file() {
        return Err(format!("missing bpf object at {}", built.display()));
    }
    std::fs::copy(&built, out.join("rthas-ebpf-prog"))
        .map_err(|e| format!("copy bpf object: {e}"))?;
    Ok(())
}
