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

use std::path::PathBuf;

use rthas_ebpf::LINUX_REQUIRED;

fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    match argv.first().map(String::as_str) {
        Some("serve") => {
            if let Err(e) = cmd_serve(&argv[1..]) {
                eprintln!("rthas-ebpf: {e}");
                std::process::exit(1);
            }
        }
        _ => {
            eprintln!("usage: rthas-ebpf serve <pid> [--sock-dir DIR]");
            std::process::exit(2);
        }
    }
}

fn cmd_serve(args: &[String]) -> Result<(), String> {
    let mut pid = None;
    let mut dir = None;
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--sock-dir" => dir = it.next().map(PathBuf::from),
            _ if arg.starts_with("--sock-dir=") => {
                dir = arg.split_once('=').map(|(_, v)| PathBuf::from(v))
            }
            _ => pid = arg.parse::<u32>().ok(),
        }
    }
    let pid = pid.ok_or("usage: rthas-ebpf serve <pid> [--sock-dir DIR]")?;
    let dir = dir.unwrap_or_else(|| {
        std::env::var("RTHAS_SOCK_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("/tmp"))
    });

    #[cfg(not(target_os = "linux"))]
    {
        let _ = (pid, dir);
        Err(LINUX_REQUIRED.to_string())
    }

    #[cfg(target_os = "linux")]
    {
        rthas_ebpf::server::serve_linux(pid, &dir)
    }
}
