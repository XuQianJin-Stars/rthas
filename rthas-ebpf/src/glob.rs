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

//! Copied from `rthas::probe::glob_match` so the helper stays independent of
//! the instrumentation crate.

pub fn glob_match(pattern: &str, s: &str) -> bool {
    if pattern.is_empty() || pattern == "*" {
        return true;
    }
    if !pattern.contains('*') {
        return s.contains(pattern);
    }

    let parts: Vec<&str> = pattern.split('*').collect();
    let (first, last) = match (parts.first(), parts.last()) {
        (Some(f), Some(l)) => (*f, *l),
        _ => return true,
    };
    let middles = &parts[1..parts.len() - 1];

    let starts: Vec<usize> = if first.is_empty() {
        vec![0]
    } else {
        s.match_indices(first).map(|(i, _)| i).collect()
    };

    for start in starts {
        let mut rest = &s[start + first.len()..];
        let mut matched = true;
        for middle in middles {
            if middle.is_empty() {
                continue;
            }
            match rest.find(middle) {
                Some(i) => rest = &rest[i + middle.len()..],
                None => {
                    matched = false;
                    break;
                }
            }
        }
        if matched && (last.is_empty() || rest.ends_with(last)) {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::glob_match;

    #[test]
    fn substring_without_star() {
        assert!(glob_match("get_status", "a::b::get_status"));
        assert!(!glob_match("get_status", "a::b::set_status"));
    }

    #[test]
    fn prefix_and_suffix_stars() {
        assert!(glob_match("MasterClient::*", "m::MasterClient::get_status"));
        assert!(!glob_match("MasterClient::*", "m::WorkerClient::get_status"));
        assert!(glob_match("*::query", "db::query"));
        assert!(glob_match("*db*", "a::db::query"));
    }

    #[test]
    fn empty_matches_all() {
        assert!(glob_match("", "anything"));
        assert!(glob_match("*", "anything"));
    }
}
