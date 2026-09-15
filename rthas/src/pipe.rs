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

//! Arthas-style pipes: `<cmd> | grep PATTERN | tee FILE | wc`.

use std::collections::VecDeque;
use std::fs::{File, OpenOptions};
use std::io::{self, Write};

/// Split `list | grep foo | wc` from a command whose args may contain `|`
/// (e.g. `watch --ret Err|Ok`). A segment is a pipe stage only if it starts
/// with `grep`, `tee`, or `wc`.
pub fn split_pipeline(line: &str) -> (String, Vec<String>) {
    let parts: Vec<&str> = line.split('|').map(str::trim).collect();
    if parts.len() == 1 {
        return (line.trim().to_string(), Vec::new());
    }
    let mut cmd = parts[0].to_string();
    let mut pipes = Vec::new();
    for part in &parts[1..] {
        if is_pipe_verb(part) {
            pipes.push(part.to_string());
        } else if let Some(last) = pipes.last_mut() {
            last.push('|');
            last.push_str(part);
        } else {
            cmd.push('|');
            cmd.push_str(part);
        }
    }
    (cmd, pipes)
}

fn is_pipe_verb(s: &str) -> bool {
    matches!(s.split_whitespace().next(), Some("grep" | "tee" | "wc"))
}

enum Stage {
    Grep(Grep),
    Tee { file: File },
    Wc { lines: usize },
}

impl Stage {
    fn parse(spec: &str) -> Result<Self, String> {
        let verb = spec.split_whitespace().next().unwrap_or("");
        match verb {
            "grep" => Ok(Stage::Grep(Grep::parse(spec)?)),
            "tee" => {
                let (path, append) = parse_tee(spec)?;
                let file = OpenOptions::new()
                    .create(true)
                    .write(true)
                    .append(append)
                    .truncate(!append)
                    .open(&path)
                    .map_err(|e| format!("tee {path}: {e}"))?;
                Ok(Stage::Tee { file })
            }
            "wc" => Ok(Stage::Wc { lines: 0 }),
            other => Err(format!(
                "unknown pipe '{other}'. try grep, tee, or wc"
            )),
        }
    }

    fn feed(&mut self, line: &str) -> io::Result<Vec<String>> {
        match self {
            Stage::Grep(g) => Ok(g.feed(line)),
            Stage::Tee { file } => {
                writeln!(file, "{line}")?;
                Ok(vec![line.to_string()])
            }
            Stage::Wc { lines } => {
                *lines += 1;
                Ok(Vec::new())
            }
        }
    }

    fn finish(&mut self) -> io::Result<Vec<String>> {
        match self {
            Stage::Grep(g) => Ok(g.finish()),
            Stage::Tee { file } => {
                file.flush()?;
                Ok(Vec::new())
            }
            Stage::Wc { lines } => Ok(vec![lines.to_string()]),
        }
    }
}

/// Filters command output through `grep` / `tee` / `wc` as lines arrive.
pub struct Pipeline<W: Write> {
    inner: W,
    stages: Vec<Stage>,
    buf: Vec<u8>,
}

impl<W: Write> Pipeline<W> {
    pub fn new(inner: W, specs: &[String]) -> Result<Self, String> {
        let mut stages = Vec::with_capacity(specs.len());
        for spec in specs {
            stages.push(Stage::parse(spec)?);
        }
        Ok(Self {
            inner,
            stages,
            buf: Vec::new(),
        })
    }

    fn emit_line(&mut self, line: &str) -> io::Result<()> {
        let mut lines = vec![line.to_string()];
        for stage in &mut self.stages {
            let mut next = Vec::new();
            for l in lines {
                next.extend(stage.feed(&l)?);
            }
            lines = next;
        }
        for l in lines {
            writeln!(self.inner, "{l}")?;
        }
        Ok(())
    }

    pub fn finish(&mut self) -> io::Result<()> {
        if !self.buf.is_empty() {
            let rest = std::mem::take(&mut self.buf);
            let line = String::from_utf8_lossy(&rest);
            self.emit_line(line.trim_end_matches('\r'))?;
        }
        let mut lines: Vec<String> = Vec::new();
        for stage in &mut self.stages {
            let mut next = Vec::new();
            if lines.is_empty() {
                next.extend(stage.finish()?);
            } else {
                for l in &lines {
                    next.extend(stage.feed(l)?);
                }
                next.extend(stage.finish()?);
            }
            lines = next;
        }
        for l in lines {
            writeln!(self.inner, "{l}")?;
        }
        self.inner.flush()
    }
}

impl<W: Write> Write for Pipeline<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.buf.extend_from_slice(buf);
        while let Some(pos) = self.buf.iter().position(|&b| b == b'\n') {
            let mut line: Vec<u8> = self.buf.drain(..=pos).collect();
            if line.last() == Some(&b'\n') {
                line.pop();
            }
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            let s = String::from_utf8_lossy(&line);
            self.emit_line(&s)?;
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

pub struct Grep {
    pattern: String,
    ignore_case: bool,
    invert: bool,
    line_number: bool,
    count_only: bool,
    max_count: usize,
    before: usize,
    after: usize,
    line_no: usize,
    hits: usize,
    pending_after: usize,
    before_buf: VecDeque<(usize, String)>,
}

impl Grep {
    pub fn parse(spec: &str) -> Result<Self, String> {
        let mut ignore_case = false;
        let mut invert = false;
        let mut line_number = false;
        let mut count_only = false;
        let mut max_count = 0usize;
        let mut before = 0usize;
        let mut after = 0usize;
        let mut pattern = String::new();
        let mut tokens = tokenize(spec);
        if tokens.first().map(String::as_str) == Some("grep") {
            tokens.remove(0);
        }
        let mut i = 0;
        while i < tokens.len() {
            let t = tokens[i].as_str();
            if t == "--" {
                i += 1;
                if i < tokens.len() && pattern.is_empty() {
                    pattern = tokens[i].clone();
                }
                break;
            }
            if let Some(rest) = t.strip_prefix("--") {
                match rest {
                    "ignore-case" => ignore_case = true,
                    "invert-match" => invert = true,
                    "line-number" => line_number = true,
                    "count" => count_only = true,
                    "max-count" => {
                        i += 1;
                        max_count = tokens.get(i).and_then(|s| s.parse().ok()).unwrap_or(0);
                    }
                    "after-context" => {
                        i += 1;
                        after = tokens.get(i).and_then(|s| s.parse().ok()).unwrap_or(0);
                    }
                    "before-context" => {
                        i += 1;
                        before = tokens.get(i).and_then(|s| s.parse().ok()).unwrap_or(0);
                    }
                    "context" => {
                        i += 1;
                        let n = tokens.get(i).and_then(|s| s.parse().ok()).unwrap_or(0);
                        before = n;
                        after = n;
                    }
                    "regex" => {
                        return Err("grep --regex is not supported (substring only)".into());
                    }
                    other => {
                        return Err(format!("grep: unknown option --{other}"));
                    }
                }
            } else if let Some(rest) = t.strip_prefix('-') {
                if rest.is_empty() {
                    i += 1;
                    continue;
                }
                let mut chars = rest.chars().peekable();
                while let Some(c) = chars.next() {
                    match c {
                        'i' => ignore_case = true,
                        'v' => invert = true,
                        'n' => line_number = true,
                        'c' => count_only = true,
                        'm' | 'A' | 'B' | 'C' => {
                            let glued: String = chars.by_ref().collect();
                            let val = if glued.is_empty() {
                                i += 1;
                                tokens.get(i).and_then(|s| s.parse().ok()).unwrap_or(0)
                            } else {
                                glued.parse().unwrap_or(0)
                            };
                            match c {
                                'm' => max_count = val,
                                'A' => after = val,
                                'B' => before = val,
                                'C' => {
                                    before = val;
                                    after = val;
                                }
                                _ => {}
                            }
                        }
                        'e' => {
                            return Err("grep -e regex is not supported (substring only)".into());
                        }
                        other => return Err(format!("grep: unknown option -{other}")),
                    }
                }
            } else if pattern.is_empty() {
                pattern = t.to_string();
            }
            i += 1;
        }
        if pattern.is_empty() {
            return Err("grep needs a pattern".into());
        }
        Ok(Self {
            pattern,
            ignore_case,
            invert,
            line_number,
            count_only,
            max_count,
            before,
            after,
            line_no: 0,
            hits: 0,
            pending_after: 0,
            before_buf: VecDeque::new(),
        })
    }

    fn matches(&self, line: &str) -> bool {
        let hit = if self.ignore_case {
            line.to_ascii_lowercase()
                .contains(&self.pattern.to_ascii_lowercase())
        } else {
            line.contains(&self.pattern)
        };
        if self.invert {
            !hit
        } else {
            hit
        }
    }

    fn format(&self, n: usize, line: &str) -> String {
        if self.line_number {
            format!("{n}:{line}")
        } else {
            line.to_string()
        }
    }

    fn stopped(&self) -> bool {
        self.max_count > 0 && self.hits >= self.max_count
    }

    pub fn feed(&mut self, line: &str) -> Vec<String> {
        if self.stopped() && self.pending_after == 0 {
            return Vec::new();
        }
        self.line_no += 1;
        let n = self.line_no;
        let is_hit = self.matches(line);
        let mut out = Vec::new();
        if is_hit {
            if self.stopped() {
                return out;
            }
            self.hits += 1;
            if !self.count_only {
                while let Some((bn, bl)) = self.before_buf.pop_front() {
                    out.push(self.format(bn, &bl));
                }
                out.push(self.format(n, line));
            }
            self.pending_after = self.after;
            self.before_buf.clear();
        } else if self.pending_after > 0 {
            self.pending_after -= 1;
            if !self.count_only {
                out.push(self.format(n, line));
            }
        } else if self.before > 0 && !self.count_only {
            self.before_buf.push_back((n, line.to_string()));
            while self.before_buf.len() > self.before {
                self.before_buf.pop_front();
            }
        }
        out
    }

    pub fn finish(&mut self) -> Vec<String> {
        if self.count_only {
            vec![self.hits.to_string()]
        } else {
            Vec::new()
        }
    }
}

fn parse_tee(spec: &str) -> Result<(String, bool), String> {
    let mut append = false;
    let mut path = String::new();
    let mut tokens = tokenize(spec);
    if tokens.first().map(String::as_str) == Some("tee") {
        tokens.remove(0);
    }
    for t in tokens {
        if t == "-a" || t == "--append" {
            append = true;
        } else if t.starts_with('-') {
            return Err(format!("tee: unknown option {t}"));
        } else if path.is_empty() {
            path = t;
        }
    }
    if path.is_empty() {
        return Err("tee needs a file path".into());
    }
    Ok((path, append))
}

fn tokenize(s: &str) -> Vec<String> {
    s.split_whitespace().map(|t| t.to_string()).collect()
}

#[cfg(test)]
mod tests {
    use super::{split_pipeline, Grep};

    #[test]
    fn splits_pipes_but_keeps_bars_in_args() {
        let (cmd, pipes) = split_pipeline("watch --ret Err|Ok");
        assert_eq!(cmd, "watch --ret Err|Ok");
        assert!(pipes.is_empty());

        let (cmd, pipes) = split_pipeline("list | grep handle | wc");
        assert_eq!(cmd, "list");
        assert_eq!(pipes, vec!["grep handle", "wc"]);
    }

    #[test]
    fn grep_substring_and_invert() {
        let mut g = Grep::parse("grep foo").unwrap();
        assert_eq!(g.feed("hello foo bar"), vec!["hello foo bar".to_string()]);
        assert!(g.feed("nope").is_empty());
        let mut g = Grep::parse("grep -v foo").unwrap();
        assert!(g.feed("hello foo").is_empty());
        assert_eq!(g.feed("nope"), vec!["nope".to_string()]);
    }

    #[test]
    fn grep_ignore_case_count_and_line_numbers() {
        let mut g = Grep::parse("grep -i FOO").unwrap();
        assert_eq!(g.feed("x foo y").len(), 1);
        let mut g = Grep::parse("grep -c foo").unwrap();
        assert!(g.feed("a foo").is_empty());
        assert!(g.feed("foo 2").is_empty());
        assert!(g.feed("no").is_empty());
        assert_eq!(g.finish(), vec!["2".to_string()]);
        let mut g = Grep::parse("grep -n foo").unwrap();
        g.feed("a");
        assert_eq!(g.feed("foo"), vec!["2:foo".to_string()]);
    }

    #[test]
    fn grep_max_count_and_context() {
        let mut g = Grep::parse("grep -m 1 foo").unwrap();
        assert_eq!(g.feed("foo 1").len(), 1);
        assert!(g.feed("foo 2").is_empty());
        let mut g = Grep::parse("grep -B1 -A1 foo").unwrap();
        assert!(g.feed("keep").is_empty());
        let hit = g.feed("foo");
        assert_eq!(hit, vec!["keep".to_string(), "foo".to_string()]);
        assert_eq!(g.feed("after"), vec!["after".to_string()]);
    }

    #[test]
    fn grep_needs_a_pattern() {
        assert!(Grep::parse("grep -i").is_err());
    }
}
