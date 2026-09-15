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
// See the License for the specific language governing permissions and
// limitations under the License.

//! Stamp compile-time rustc / target facts into the library so `jvm` and
//! `sysprop` can report them without spawning `rustc` at runtime.

fn main() {
    set("RTHAS_TARGET", env("TARGET"));
    set("RTHAS_PROFILE", env("PROFILE"));
    set("RTHAS_OPT_LEVEL", env("OPT_LEVEL"));
    set("RTHAS_RUSTC", rustc_version());
}

fn env(key: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| "-".into())
}

fn set(key: &str, value: String) {
    let value = value.trim().replace('\n', " ");
    println!("cargo:rustc-env={key}={value}");
}

fn rustc_version() -> String {
    let rustc = std::env::var("RUSTC").unwrap_or_else(|_| "rustc".into());
    std::process::Command::new(rustc)
        .arg("-V")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".into())
}
