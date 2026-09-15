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

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;

pub struct Args<'a> {
    pub pos: Vec<&'a str>,
    flags: HashMap<&'a str, &'a str>,
}

impl<'a> Args<'a> {
    pub fn parse(line: &'a str) -> Self {
        let mut pos = Vec::new();
        let mut flags = HashMap::new();
        let mut it = line.split_whitespace().peekable();
        while let Some(tok) = it.next() {
            if let Some(rest) = tok.strip_prefix("--") {
                if let Some((k, v)) = rest.split_once('=') {
                    flags.insert(k, v);
                } else {
                    let value = match it.peek() {
                        Some(next) if !next.starts_with("--") => it.next().unwrap_or(""),
                        _ => "",
                    };
                    flags.insert(rest, value);
                }
            } else {
                pos.push(tok);
            }
        }
        Self { pos, flags }
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        self.flags.get(key).copied()
    }

    pub fn flag(&self, key: &str) -> bool {
        self.flags.contains_key(key)
    }

    pub fn num<T: std::str::FromStr>(&self, key: &str, default: T) -> T {
        self.get(key)
            .and_then(|v| v.parse().ok())
            .unwrap_or(default)
    }

    pub fn pattern(&self) -> &str {
        self.pos.get(1).copied().unwrap_or("")
    }
}

pub const HELP: &str = "\
rthas eBPF attach (uninstrumented process)
  list [pattern]                 enumerate symbols in the target binary
  on <pattern> / off [pattern]   mark symbols enabled (off by default)
  trace <pattern> [opts]         call trees from uprobe enter/exit
     --count N / --seconds F / --depth N / --min-ms F
  watch <pattern> [opts]         one line per return (--count N, --seconds F)
  stop                           detach and exit this helper
  ping / help

No Debug args/return values. Async call trees are not reliable.
stack/tt/monitor/stats/dashboard/memory/jvm/sysprop/profiler are not available on eBPF attach.
";

pub fn handle_client<F>(stream: UnixStream, mut dispatch: F) -> std::io::Result<()>
where
    F: FnMut(&str, &mut dyn Write) -> std::io::Result<bool>,
{
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut writer = stream;
    let mut line = String::new();
    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            break;
        }
        let cmd = line.trim();
        if cmd.is_empty() {
            continue;
        }
        let keep_open = dispatch(cmd, &mut writer)?;
        writeln!(writer, "{}", crate::END)?;
        writer.flush()?;
        if !keep_open {
            break;
        }
    }
    Ok(())
}

pub fn bind_socket(path: &PathBuf) -> std::io::Result<UnixListener> {
    if let Some(dir) = path.parent() {
        if !dir.as_os_str().is_empty() {
            std::fs::create_dir_all(dir).ok();
        }
    }
    let _ = std::fs::remove_file(path);
    UnixListener::bind(path)
}

#[cfg(test)]
mod tests {
    use super::Args;

    #[test]
    fn parses_trace_flags() {
        let a = Args::parse("trace handle_request --count 3 --min-ms=1.5");
        assert_eq!(a.pattern(), "handle_request");
        assert_eq!(a.num("count", 0usize), 3);
        assert!((a.num("min-ms", 0.0f64) - 1.5).abs() < f64::EPSILON);
    }
}
