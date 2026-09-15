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

//! Arthas-style background jobs: `trace foo > /tmp/out &`, `jobs`, `kill`.

use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JobSpec {
    pub cmd: String,
    pub redirect: Option<(String, bool)>,
    pub background: bool,
}

/// Split a trailing `&` and `>` / `>> FILE` from a command line.
pub fn parse_job_line(line: &str) -> JobSpec {
    let mut toks: Vec<&str> = line.split_whitespace().collect();
    let mut background = false;
    if toks.last() == Some(&"&") {
        toks.pop();
        background = true;
    }
    let mut redirect = None;
    if toks.len() >= 2 {
        let path = toks[toks.len() - 1].to_string();
        match toks[toks.len() - 2] {
            ">>" => {
                toks.truncate(toks.len() - 2);
                redirect = Some((path, true));
            }
            ">" => {
                toks.truncate(toks.len() - 2);
                redirect = Some((path, false));
            }
            _ => {}
        }
    }
    JobSpec {
        cmd: toks.join(" "),
        redirect,
        background,
    }
}

pub fn cannot_background(verb: &str) -> bool {
    matches!(
        verb,
        "jobs" | "kill" | "help" | "?" | "quit" | "exit" | "q" | "stop" | "auth" | "fg" | "bg"
    )
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JobState {
    Running,
    Done,
    Killed,
}

impl JobState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Done => "done",
            Self::Killed => "killed",
        }
    }
}

struct Job {
    id: u32,
    cmd: String,
    log: PathBuf,
    stop: Arc<AtomicBool>,
    state: Mutex<JobState>,
    started: Instant,
}

static NEXT_ID: AtomicU32 = AtomicU32::new(1);
static TABLE: Mutex<Vec<Arc<Job>>> = Mutex::new(Vec::new());

fn table() -> std::sync::MutexGuard<'static, Vec<Arc<Job>>> {
    TABLE.lock().unwrap_or_else(|e| e.into_inner())
}

pub fn log_path(id: u32) -> PathBuf {
    let dir = std::env::var("RTHAS_SOCK_DIR").unwrap_or_else(|_| "/tmp".to_string());
    PathBuf::from(dir).join(format!("rthas-{}-job-{id}.log", std::process::id()))
}

pub fn submit(
    cmd: String,
    redirect: Option<(String, bool)>,
) -> (u32, Arc<AtomicBool>, PathBuf, bool) {
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let (log, append) = match redirect {
        Some((p, a)) => (PathBuf::from(p), a),
        None => (log_path(id), false),
    };
    let stop = Arc::new(AtomicBool::new(false));
    let job = Arc::new(Job {
        id,
        cmd,
        log: log.clone(),
        stop: stop.clone(),
        state: Mutex::new(JobState::Running),
        started: Instant::now(),
    });
    table().push(job);
    (id, stop, log, append)
}

pub fn finish(id: u32, killed: bool) {
    if let Some(job) = table().iter().find(|j| j.id == id) {
        *job.state.lock().unwrap_or_else(|e| e.into_inner()) = if killed {
            JobState::Killed
        } else {
            JobState::Done
        };
    }
}

pub fn kill(id: u32) -> bool {
    let table = table();
    if let Some(job) = table.iter().find(|j| j.id == id) {
        job.stop.store(true, Ordering::SeqCst);
        true
    } else {
        false
    }
}

pub fn render_table() -> String {
    let table = table();
    if table.is_empty() {
        return "no jobs\n".into();
    }
    let mut out = format!(
        "{:<4} {:<8} {:<10} {}\n",
        "ID", "STATE", "ELAPSED", "COMMAND"
    );
    for job in table.iter() {
        let state = *job.state.lock().unwrap_or_else(|e| e.into_inner());
        let elapsed = format_clock(job.started.elapsed());
        out.push_str(&format!(
            "{:<4} {:<8} {:<10} {}\n",
            job.id,
            state.as_str(),
            elapsed,
            job.cmd
        ));
        out.push_str(&format!("     log {}\n", job.log.display()));
    }
    out
}

fn format_clock(d: std::time::Duration) -> String {
    let secs = d.as_secs();
    format!(
        "{:02}:{:02}:{:02}",
        secs / 3600,
        (secs % 3600) / 60,
        secs % 60
    )
}

/// `Write` adapter that fails with BrokenPipe once `stop` is set, so streaming
/// commands exit the same way they do when a client disconnects.
pub struct StopWrite<W: Write> {
    inner: W,
    stop: Arc<AtomicBool>,
}

impl<W: Write> StopWrite<W> {
    pub fn new(inner: W, stop: Arc<AtomicBool>) -> Self {
        Self { inner, stop }
    }
}

impl<W: Write> Write for StopWrite<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.stop.load(Ordering::Relaxed) {
            return Err(io::Error::new(io::ErrorKind::BrokenPipe, "job killed"));
        }
        self.inner.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::parse_job_line;

    #[test]
    fn trailing_ampersand_is_background() {
        let s = parse_job_line("trace handle --count 0 &");
        assert!(s.background);
        assert_eq!(s.cmd, "trace handle --count 0");
        assert!(s.redirect.is_none());
    }

    #[test]
    fn redirect_and_background() {
        let s = parse_job_line("watch foo >> /tmp/out &");
        assert!(s.background);
        assert_eq!(s.cmd, "watch foo");
        assert_eq!(s.redirect, Some(("/tmp/out".into(), true)));
    }

    #[test]
    fn overwrite_redirect() {
        let s = parse_job_line("list > /tmp/a");
        assert!(!s.background);
        assert_eq!(s.redirect, Some(("/tmp/a".into(), false)));
        assert_eq!(s.cmd, "list");
    }

    #[test]
    fn bar_in_args_is_not_background() {
        let s = parse_job_line("watch --ret Err|Ok");
        assert!(!s.background);
        assert_eq!(s.cmd, "watch --ret Err|Ok");
    }
}
