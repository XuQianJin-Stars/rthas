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

//! Per-tid enter/exit pairing. Sync calls become a tree; a mismatched exit
//! (panic, async poll) drops the unmatched enter rather than wedging the stack.

use std::collections::HashMap;

use crate::format::format_dur;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Node {
    pub id: usize,
    pub dur_ns: u64,
    pub children: Vec<Node>,
}

struct Frame {
    id: usize,
    start_ns: u64,
    children: Vec<Node>,
}

#[derive(Default)]
pub struct Forest {
    by_tid: HashMap<u32, Vec<Frame>>,
}

pub struct Completed {
    pub node: Node,
    pub is_root: bool,
    pub tid: u32,
}

impl Forest {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn enter(&mut self, tid: u32, id: usize, ts: u64) {
        self.by_tid.entry(tid).or_default().push(Frame {
            id,
            start_ns: ts,
            children: Vec::new(),
        });
    }

    pub fn exit(&mut self, tid: u32, id: usize, ts: u64) -> Option<Completed> {
        let stack = self.by_tid.get_mut(&tid)?;
        while stack.last().is_some_and(|f| f.id != id) {
            stack.pop();
        }
        let frame = stack.pop()?;
        if frame.id != id {
            return None;
        }
        let node = Node {
            id,
            dur_ns: ts.saturating_sub(frame.start_ns),
            children: frame.children,
        };
        if let Some(parent) = stack.last_mut() {
            parent.children.push(node.clone());
            Some(Completed {
                node,
                is_root: false,
                tid,
            })
        } else {
            Some(Completed {
                node,
                is_root: true,
                tid,
            })
        }
    }
}

pub fn render_tree(node: &Node, names: &[String], depth: usize, max_depth: usize) -> String {
    let mut out = String::new();
    render_into(node, names, depth, max_depth, &mut out);
    out
}

fn render_into(node: &Node, names: &[String], depth: usize, max_depth: usize, out: &mut String) {
    if max_depth > 0 && depth >= max_depth {
        return;
    }
    let indent = "  ".repeat(depth);
    let name = names.get(node.id).map(|s| s.as_str()).unwrap_or("<unknown>");
    let _ = std::fmt::Write::write_fmt(
        out,
        format_args!("{indent}{name} {}\n", format_dur(node.dur_ns)),
    );
    for child in &node.children {
        render_into(child, names, depth + 1, max_depth, out);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nested_sync_call_becomes_a_tree() {
        let mut f = Forest::new();
        f.enter(1, 0, 0);
        f.enter(1, 1, 10);
        let inner = f.exit(1, 1, 30).unwrap();
        assert!(!inner.is_root);
        assert_eq!(inner.node.dur_ns, 20);
        let root = f.exit(1, 0, 50).unwrap();
        assert!(root.is_root);
        assert_eq!(root.node.dur_ns, 50);
        assert_eq!(root.node.children.len(), 1);
        assert_eq!(root.node.children[0].id, 1);
    }

    #[test]
    fn mismatched_exit_drops_stale_enter() {
        let mut f = Forest::new();
        f.enter(1, 0, 0);
        f.enter(1, 1, 5);
        // Function 1 never returns (panic / async). Outer function does.
        let root = f.exit(1, 0, 40).unwrap();
        assert!(root.is_root);
        assert!(root.node.children.is_empty());
    }
}
