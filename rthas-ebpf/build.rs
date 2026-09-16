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

    // Do not use `$CARGO`: that is the current toolchain's cargo binary, which
    // treats `+nightly` as a subcommand (`no such command: +nightly`). The
    // parent build also exports `RUSTC` / `RUSTUP_TOOLCHAIN` for stable; strip
    // those so the nested compile actually uses nightly + `-Z build-std`.
    let mut cmd = Command::new("rustup");
    cmd.args(["run", "nightly", "cargo", "build", "--release", "--manifest-path"])
        .arg(&prog)
        .args(["--target", "bpfel-unknown-none", "-Z", "build-std=core"])
        .env("CARGO_TARGET_DIR", &target_dir);
    cmd.env_remove("CARGO");
    cmd.env_remove("RUSTC");
    cmd.env_remove("RUSTC_WRAPPER");
    cmd.env_remove("RUSTC_WORKSPACE_WRAPPER");
    cmd.env_remove("RUSTUP_TOOLCHAIN");
    cmd.env_remove("CARGO_ENCODED_RUSTFLAGS");
    cmd.env_remove("RUSTFLAGS");

    let status = cmd
        .status()
        .map_err(|e| format!("spawn rustup run nightly cargo: {e}"))?;
    if !status.success() {
        return Err(format!("rustup run nightly cargo exited {status}"));
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
