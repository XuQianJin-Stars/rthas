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

//! `rthas` command line client.
//!
//! Deliberately dependency-free: it is a thin front-end that forwards your
//! command line to the agent verbatim and streams the reply back. Keeping it
//! to `std` means it builds in seconds and can be `scp`'d to a machine that
//! has nothing but the target binary on it.

use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Sentinel the agent appends after every response.
///
/// The agent keeps the connection open for the next command, so EOF cannot
/// mark the end of a reply — a streaming `trace` would otherwise hang the
/// client until the socket closed.
const END: &str = "<<<end>>>";

/// Marker `rthas` compiles into every instrumented binary.
///
/// Kept in sync with `rthas::MAGIC`. Duplicated rather than linked because the
/// CLI is deliberately dependency-free — it has to keep working when it is
/// scp'd onto a machine that has nothing but the target binary on it.
const MAGIC: &[u8] = b"rthas/probe/v1";

/// How long `attach` waits for the target to bind its socket.
const ATTACH_TIMEOUT: Duration = Duration::from_secs(5);

const USAGE: &str = "\
rthas — Arthas-style runtime probes for Rust

USAGE:
    rthas <command> [options] [--pid PID | --sock PATH | --sock-dir DIR]

DISCOVERY:
    ps                              list processes with a live rthas agent
    ps --all                        list every process, flagging the ones built
                                    with #[rthas::trace]
    attach <pid>                    start the agent inside a running process
                                    that deferred it (RTHAS_AGENT=lazy)
    shell                           interactive session

ATTACHING
    A process that calls rthas::init() at startup is reachable straight away.
    One built with RTHAS_AGENT=lazy — or calling rthas::init_lazy() — costs
    nothing until `rthas attach <pid>` asks it to bind its socket, so no
    restart and no recompile are needed.

    Either way the binary must carry #[rthas::trace] probes. Attaching to a
    process that was never instrumented needs eBPF uprobes (Linux + root + an
    unstripped binary), which rthas does not implement yet.

INSPECTION:
    list [pattern]                  enumerate instrumented functions
    trace <pattern> [opts]          stream call trees (--count N, --seconds F,
                                    --depth N, --min-ms F, --grace-ms N)
    watch <pattern> [opts]          one line per call (--args S, --ret S,
                                    --count N, --seconds F)
    stack <pattern> [opts]          call path reaching each matching call
                                    (--native, --count N, --depth N)
    stats [pattern]                 p50/p95/p99/max over the ring buffer
    top [pattern] [--n N] [--by total|max|count]
    on <pattern> / off [pattern]    toggle probes manually
    clear                           drop buffered events

PROCESS:
    dashboard [opts]                live process overview, refreshes until
                                    Ctrl-C (--interval F, --count N, --n N)
    thread [opts]                   per-thread CPU and last recorded span
                                    (--n N, --by tid|cpu|name)

If no target is given and exactly one rthas agent is running, it is used
automatically. Set RTHAS_SOCK_DIR if your processes use a non-/tmp directory.

EXAMPLES:
    rthas ps
    rthas list 'cache::*'
    rthas trace handle_request --count 3
    rthas watch read_block --ret Err
    rthas stack read_block --native --count 2
    rthas dashboard --interval 0.5
    rthas thread --by cpu --n 5
    rthas top --n 5 --by max
";

/// Accept `rthas --pid 5 list` as a synonym of `rthas list --pid 5`.
///
/// Only the leading one or two tokens are inspected, so this can never misfire
/// on a command's own flags.
fn rotate_target_flags(argv: Vec<String>) -> Vec<String> {
    let flag = argv.first().map(String::as_str).unwrap_or("");
    let inline = flag.starts_with("--pid=")
        || flag.starts_with("--sock=")
        || flag.starts_with("--sock-dir=");
    if inline {
        let mut out = argv[1..].to_vec();
        out.push(argv[0].clone());
        return out;
    }
    if matches!(flag, "--pid" | "--sock" | "--sock-dir") && argv.len() >= 2 {
        let mut out = argv[2..].to_vec();
        out.push(argv[0].clone());
        out.push(argv[1].clone());
        return out;
    }
    argv
}

fn main() {
    let argv: Vec<String> = rotate_target_flags(std::env::args().skip(1).collect());
    if argv.is_empty() {
        print!("{USAGE}");
        return;
    }

    match argv[0].as_str() {
        "-h" | "--help" | "help" => print!("{USAGE}"),
        "ps" => {
            let args = &argv[1..];
            let all = args.iter().any(|a| a == "--all");
            let filter = args
                .iter()
                .find(|a| !a.starts_with("--"))
                .map(|s| s.as_str())
                .unwrap_or("");
            cmd_ps(all, filter)
        }
        "attach" => match attach_args(&argv[1..]).and_then(|(pid, dir)| cmd_attach(pid, dir)) {
            Ok(()) => {}
            Err(e) => {
                eprintln!("rthas: {e}");
                std::process::exit(1);
            }
        },
        "shell" => cmd_shell(&argv[1..]),
        cmd @ ("list" | "trace" | "watch" | "stack" | "dashboard" | "thread" | "stats" | "top"
        | "on" | "off" | "clear" | "ping") => {
            if let Err(e) = cmd_remote(cmd, &argv[1..]) {
                eprintln!("rthas: {e}");
                std::process::exit(1);
            }
        }
        other => {
            eprintln!("rthas: unknown command '{other}'\n");
            print!("{USAGE}");
            std::process::exit(2);
        }
    }
}

// ---------------------------------------------------------------------------
// Target resolution
// ---------------------------------------------------------------------------

/// Flags the CLI consumes itself; everything else is forwarded to the agent.
struct Target {
    sock: Option<PathBuf>,
    pid: Option<u32>,
    sock_dir: Option<PathBuf>,
}

fn parse_target(args: &[String]) -> (Target, Vec<String>) {
    let mut target = Target {
        sock: None,
        pid: None,
        sock_dir: None,
    };
    let mut forwarded: Vec<String> = Vec::with_capacity(args.len());
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--sock" => target.sock = it.next().map(PathBuf::from),
            "--sock-dir" => target.sock_dir = it.next().map(PathBuf::from),
            "--pid" => target.pid = it.next().and_then(|v| v.parse().ok()),
            _ if arg.starts_with("--sock=") => target.sock = arg.split_once('=').map(|(_, v)| v.into()),
            _ if arg.starts_with("--pid=") => {
                target.pid = arg.split_once('=').and_then(|(_, v)| v.parse().ok())
            }
            _ => forwarded.push(arg.clone()),
        }
    }
    (target, forwarded)
}

fn sock_dir(target: &Target) -> PathBuf {
    target
        .sock_dir
        .clone()
        .or_else(|| std::env::var("RTHAS_SOCK_DIR").ok().map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("/tmp"))
}

fn resolve_sock(target: &Target) -> Result<PathBuf, String> {
    if let Some(sock) = &target.sock {
        return Ok(sock.clone());
    }
    if let Some(pid) = target.pid {
        return Ok(sock_dir(target).join(format!("rthas-{pid}.sock")));
    }
    if let Ok(sock) = std::env::var("RTHAS_SOCK") {
        return Ok(PathBuf::from(sock));
    }

    let found = discover(&sock_dir(target));
    match found.len() {
        0 => Err(format!(
            "no rthas agent found in {}.\nIs the process running, and did it call rthas::init()?",
            sock_dir(target).display()
        )),
        1 => Ok(found.into_iter().next().unwrap().1),
        n => Err(format!(
            "{n} agents found; disambiguate with --pid PID (see `rthas ps`)"
        )),
    }
}

/// Scan the socket directory for live agents.
fn discover(dir: &Path) -> Vec<(u32, PathBuf)> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out: Vec<(u32, PathBuf)> = entries
        .flatten()
        .filter_map(|e| {
            let path = e.path();
            let name = path.file_name()?.to_string_lossy().into_owned();
            let pid = name
                .strip_prefix("rthas-")?
                .strip_suffix(".sock")?
                .parse::<u32>()
                .ok()?;
            process_alive(pid).then_some((pid, path))
        })
        .collect();
    out.sort_by_key(|(pid, _)| *pid);
    out
}

fn process_alive(pid: u32) -> bool {
    Command::new("kill")
        .arg("-0")
        .arg(pid.to_string())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Path of the binary a process is running, when the OS will tell us.
///
/// Linux answers exactly via `/proc/<pid>/exe`. Everywhere else we ask `ps`,
/// which reports whatever was on the command line — possibly a path relative
/// to the *target's* working directory, so it is only trusted when it happens
/// to resolve from here.
fn process_binary(pid: u32) -> Option<PathBuf> {
    if let Ok(exe) = std::fs::read_link(format!("/proc/{pid}/exe")) {
        return Some(exe);
    }
    let out = Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "comm="])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if text.is_empty() {
        return None;
    }
    let path = PathBuf::from(&text);
    if path.is_file() {
        return Some(path);
    }
    // `comm` is usually a bare name, so find it the way a shell would. Without
    // this the check is skipped on most platforms and `attach` can only explain
    // itself after waiting through the whole timeout.
    resolve_in_path(&text)
}

/// Look a bare command name up in `PATH`.
fn resolve_in_path(name: &str) -> Option<PathBuf> {
    if name.contains('/') {
        return None;
    }
    let search = std::env::var_os("PATH")?;
    std::env::split_paths(&search)
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
}

/// Whether a binary carries `#[rthas::trace]` probe points.
///
/// Answered by searching the file for the marker the macro embeds, which means
/// it works on a stripped release binary and needs no privileges — the file is
/// just read, the process is never touched.
fn is_instrumented(path: &Path) -> Option<bool> {
    file_contains(path, MAGIC).ok()
}

/// Stream a file looking for `needle`, without holding it all in memory.
///
/// Scanned binaries run to hundreds of megabytes, so this is a memmem rather
/// than `windows().any()`: jump to each occurrence of the first byte — which
/// lowers to `memchr` — and only then compare the rest.
fn file_contains(path: &Path, needle: &[u8]) -> std::io::Result<bool> {
    let mut file = std::fs::File::open(path)?;
    // Chunks overlap by len-1 bytes so a match straddling a boundary is found.
    let overlap = needle.len() - 1;
    let mut buf = vec![0u8; 1 << 20];
    let mut carry: Vec<u8> = Vec::new();

    loop {
        let read = file.read(&mut buf)?;
        if read == 0 {
            return Ok(false);
        }
        let mut haystack = std::mem::take(&mut carry);
        haystack.extend_from_slice(&buf[..read]);
        if contains(&haystack, needle) {
            return Ok(true);
        }
        let keep_from = haystack.len().saturating_sub(overlap);
        carry = haystack[keep_from..].to_vec();
    }
}

/// Substring search over raw bytes — the file is not valid UTF-8 throughout.
fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    let Some(&first) = needle.first() else {
        return true;
    };
    let mut rest = haystack;
    while let Some(i) = rest.iter().position(|&b| b == first) {
        if rest[i..].starts_with(needle) {
            return true;
        }
        rest = &rest[i + 1..];
    }
    false
}

/// Best-effort process name, for `ps`.
fn process_name(pid: u32) -> String {
    if let Ok(cmdline) = std::fs::read_to_string(format!("/proc/{pid}/cmdline")) {
        let name = cmdline.split('\0').next().unwrap_or("").to_string();
        if !name.is_empty() {
            return name;
        }
    }
    // macOS (and Linux fallback): ask ps.
    let out = Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "comm="])
        .output()
        .ok();
    if let Some(out) = out {
        let name = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if !name.is_empty() {
            return name;
        }
    }
    "<unknown>".to_string()
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

fn cmd_ps(all: bool, filter: &str) {
    let dir = sock_dir(&Target {
        sock: None,
        pid: None,
        sock_dir: std::env::var("RTHAS_SOCK_DIR").ok().map(PathBuf::from),
    });

    if !all {
        let found = discover(&dir);
        if found.is_empty() {
            println!("no rthas agents found in {}", dir.display());
            println!("(try `rthas ps --all` to see processes that could start one)");
            return;
        }
        println!("{:<8} {:<34} COMMAND", "PID", "SOCKET");
        for (pid, path) in found {
            println!("{:<8} {:<34} {}", pid, path.display(), process_name(pid));
        }
        return;
    }

    let live: HashSet<u32> = discover(&dir).into_iter().map(|(pid, _)| pid).collect();
    println!("{:<8} {:<7} {:<6} COMMAND", "PID", "AGENT", "PROBES");
    // Scanning a binary means reading it, and whole process trees share one,
    // so answer each distinct path at most once.
    let processes = all_processes();
    let mut probed: HashMap<PathBuf, bool> = HashMap::new();
    let mut scanned = 0usize;
    for (pid, name) in &processes {
        // Scanning means reading whole binaries, and a desktop can hold two
        // gigabytes' worth, so the name filter is applied *before* any I/O.
        if !filter.is_empty() && !name.contains(filter) {
            continue;
        }
        scanned += 1;
        let probes = match process_binary(*pid) {
            Some(binary) => match probed.get(&binary) {
                Some(&v) => v,
                None => {
                    let v = is_instrumented(&binary).unwrap_or(false);
                    probed.insert(binary, v);
                    v
                }
            },
            None => false,
        };
        println!(
            "{:<8} {:<7} {:<6} {}",
            pid,
            if live.contains(pid) { "live" } else { "-" },
            if probes { "yes" } else { "-" },
            name,
        );
    }
    println!(
        "\n{} of {} process(es) listed; {} distinct binary/binaries read.",
        scanned,
        processes.len(),
        probed.len()
    );
    if filter.is_empty() {
        println!("Pass a name filter (`rthas ps --all my-service`) to skip the rest — reading");
        println!("every binary on the system is the expensive part, not the process list.");
    }
    println!("PROBES=yes means the binary was built with #[rthas::trace];");
    println!("attach to one with `rthas attach <pid>`.");
}

/// Every process on the system as `(pid, command)`.
fn all_processes() -> Vec<(u32, String)> {
    let Some(out) = Command::new("ps").args(["-eo", "pid=,comm="]).output().ok() else {
        return Vec::new();
    };
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|line| {
            let (pid, name) = line.trim().split_once(char::is_whitespace)?;
            Some((pid.trim().parse().ok()?, name.trim().to_string()))
        })
        .collect()
}

/// Pull the pid out of `attach <pid> [--sock-dir DIR]`.
fn attach_args(args: &[String]) -> Result<(u32, Option<PathBuf>), String> {
    let mut pid = None;
    let mut dir = None;
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--sock-dir" => dir = it.next().map(PathBuf::from),
            _ if arg.starts_with("--sock-dir=") => {
                dir = arg.split_once('=').map(|(_, v)| PathBuf::from(v))
            }
            _ => pid = arg.parse().ok(),
        }
    }
    match pid {
        Some(p) => Ok((p, dir)),
        None => Err("usage: rthas attach <pid> [--sock-dir DIR]".to_string()),
    }
}

/// Ask a running process to start its agent, then wait for the socket.
fn cmd_attach(pid: u32, sock_dir: Option<PathBuf>) -> Result<(), String> {
    if !process_alive(pid) {
        return Err(format!("no process with pid {pid}"));
    }
    let dir = sock_dir.unwrap_or_else(default_sock_dir);
    let sock = dir.join(format!("rthas-{pid}.sock"));

    if sock.exists() {
        println!("pid {pid} already has an agent at {}", sock.display());
        println!("next: rthas list --pid {pid}");
        return Ok(());
    }

    // Fail fast with a real explanation rather than waiting out the timeout.
    // Best effort: some platforms will not tell us the binary path at all.
    if let Some(binary) = process_binary(pid) {
        match is_instrumented(&binary) {
            Some(false) => {
                return Err(format!(
                    "{} carries no #[rthas::trace] probes, so there is nothing to attach to.\n\
                     Attaching to a process that was never instrumented needs eBPF uprobes\n\
                     (Linux + root + an unstripped binary), which rthas does not implement yet.",
                    binary.display()
                ));
            }
            Some(true) => {}
            None => eprintln!("rthas: could not read {} to check for probes", binary.display()),
        }
    }

    let trigger = dir.join(format!(".rthas-attach-{pid}"));
    std::fs::write(&trigger, b"")
        .map_err(|e| format!("cannot write {}: {e}", trigger.display()))?;
    eprintln!("rthas: asked pid {pid} to start its agent, waiting...");

    let deadline = Instant::now() + ATTACH_TIMEOUT;
    while Instant::now() < deadline {
        if sock.exists() {
            println!("attached to pid {pid} at {}", sock.display());
            println!("next: rthas list --pid {pid}");
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    Err(format!(
        "pid {pid} did not start an agent within {}s.\n\
         Deferred start is opt-in: launch it with RTHAS_AGENT=lazy, or call\n\
         rthas::init_lazy() instead of rthas::init().",
        ATTACH_TIMEOUT.as_secs()
    ))
}

fn default_sock_dir() -> PathBuf {
    std::env::var("RTHAS_SOCK_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/tmp"))
}

fn cmd_remote(cmd: &str, args: &[String]) -> Result<(), String> {
    let (target, forwarded) = parse_target(args);
    let sock = resolve_sock(&target)?;
    if !sock.exists() {
        return Err(format!(
            "no agent at {}. Is the process still running?",
            sock.display()
        ));
    }

    let mut stream = UnixStream::connect(&sock)
        .map_err(|e| format!("connect {}: {e}", sock.display()))?;

    let mut line = String::from(cmd);
    for a in &forwarded {
        line.push(' ');
        // Quote-free forwarding: the agent splits on whitespace and these
        // values are glob patterns, not shell input.
        line.push_str(a);
    }
    line.push('\n');
    stream
        .write_all(line.as_bytes())
        .and_then(|_| stream.flush())
        .map_err(|e| format!("send: {e}"))?;

    stream_to_stdout(&mut stream)
}

/// Copy the agent's reply to stdout, stopping at the end sentinel.
fn stream_to_stdout(stream: &mut UnixStream) -> Result<(), String> {
    let mut reader = BufReader::new(stream.try_clone().map_err(|e| format!("clone: {e}"))?);
    let mut line = String::new();
    let stdout = std::io::stdout();
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => break,
            Ok(_) => {
                if line.trim() == END {
                    break;
                }
                let mut lock = stdout.lock();
                let _ = lock.write_all(line.as_bytes());
                let _ = lock.flush();
            }
            Err(e) => return Err(format!("read: {e}")),
        }
    }
    Ok(())
}

fn cmd_shell(args: &[String]) {
    let (target, _) = parse_target(args);
    let sock = match resolve_sock(&target) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("rthas: {e}");
            std::process::exit(1);
        }
    };
    let mut stream = match UnixStream::connect(&sock) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("rthas: connect {}: {e}", sock.display());
            std::process::exit(1);
        }
    };
    eprintln!("connected to {} (type 'help', 'quit' to leave)", sock.display());

    let stdin = std::io::stdin();
    let mut input = String::new();
    loop {
        eprint!("rthas> ");
        let _ = std::io::stderr().flush();
        input.clear();
        match stdin.read_line(&mut input) {
            Ok(0) => break,
            Ok(_) => {}
            Err(e) => {
                eprintln!("read: {e}");
                break;
            }
        }
        let cmd = input.trim();
        if cmd.is_empty() {
            continue;
        }
        if let Err(e) = stream.write_all(cmd.as_bytes())
            .and_then(|_| stream.write_all(b"\n"))
            .and_then(|_| stream.flush())
        {
            eprintln!("send: {e}");
            break;
        }
        if let Err(e) = stream_to_stdout(&mut stream) {
            eprintln!("{e}");
            break;
        }
        if matches!(cmd, "quit" | "exit" | "q") {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A scratch file, unique per process so parallel tests cannot collide.
    fn scratch(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("rthas-{name}-{}", std::process::id()))
    }

    #[test]
    fn finds_a_marker_straddling_a_chunk_boundary() {
        let path = scratch("straddle");
        // Exactly one byte short of the 1 MiB chunk, so the marker is split.
        let mut bytes = vec![b'x'; (1 << 20) - 2];
        bytes.extend_from_slice(MAGIC);
        bytes.extend_from_slice(&[b'y'; 64]);
        std::fs::write(&path, &bytes).unwrap();

        assert!(file_contains(&path, MAGIC).unwrap());
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn reports_an_absent_marker() {
        let path = scratch("absent");
        std::fs::write(&path, b"nothing to see here").unwrap();

        assert!(!file_contains(&path, MAGIC).unwrap());
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn parses_attach_arguments() {
        let args: Vec<String> = ["4711", "--sock-dir", "/tmp/x"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(attach_args(&args).unwrap().0, 4711);
        assert_eq!(attach_args(&args).unwrap().1, Some(PathBuf::from("/tmp/x")));

        assert!(attach_args(&[]).is_err());
    }

    #[test]
    fn rotates_leading_target_flags_only() {
        let argv: Vec<String> = ["--pid", "5", "list"].iter().map(|s| s.to_string()).collect();
        assert_eq!(rotate_target_flags(argv), vec!["list", "--pid", "5"]);

        let argv: Vec<String> = ["--pid=5", "list"].iter().map(|s| s.to_string()).collect();
        assert_eq!(rotate_target_flags(argv), vec!["list", "--pid=5"]);

        // A command's own flags must be left where they are.
        let argv: Vec<String> = ["trace", "db::*", "--count", "5"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(rotate_target_flags(argv.clone()), argv);
    }
}
