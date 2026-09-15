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

//! In-process command history (Arthas `history`).

use std::sync::Mutex;

const CAP: usize = 500;

fn cell() -> &'static Mutex<Vec<String>> {
    static HIST: Mutex<Vec<String>> = Mutex::new(Vec::new());
    &HIST
}

pub fn record(line: &str) {
    let line = line.trim();
    if line.is_empty() {
        return;
    }
    let mut h = cell().lock().unwrap_or_else(|e| e.into_inner());
    h.push(line.to_string());
    if h.len() > CAP {
        let extra = h.len() - CAP;
        h.drain(..extra);
    }
}

pub fn clear() {
    cell().lock().unwrap_or_else(|e| e.into_inner()).clear();
}

pub fn snapshot() -> Vec<String> {
    cell().lock().unwrap_or_else(|e| e.into_inner()).clone()
}

pub fn render(last_n: usize) -> String {
    let all = snapshot();
    let start = if last_n == 0 || last_n >= all.len() {
        0
    } else {
        all.len() - last_n
    };
    let mut out = String::new();
    for (i, line) in all.iter().enumerate().skip(start) {
        out.push_str(&format!("{:>5}  {line}\n", i + 1));
    }
    if out.is_empty() {
        out.push_str("(empty)\n");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_and_slices_last_n() {
        clear();
        record("list");
        record("jvm");
        record("history");
        let text = render(2);
        assert!(text.contains("jvm"));
        assert!(text.contains("history"));
        assert!(!text.contains("list"));
        clear();
        assert!(render(0).contains("(empty)"));
    }
}
