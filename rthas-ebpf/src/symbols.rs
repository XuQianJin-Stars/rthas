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

use std::path::{Path, PathBuf};

use goblin::elf::{sym::STT_FUNC, Elf};

use crate::glob::glob_match;
use crate::MAX_ATTACH;

#[derive(Clone, Debug)]
pub struct Symbol {
    pub mangled: String,
    pub demangled: String,
    pub object: PathBuf,
    /// ELF virtual address (`st_value`). Combined with the load bias to get
    /// the runtime IP that `bpf_get_func_ip` reports.
    pub vaddr: u64,
    pub enabled: bool,
}

impl Symbol {
    pub fn runtime_ip(&self, load_bias: u64) -> u64 {
        self.vaddr.saturating_add(load_bias)
    }
}

/// Names we skip unless the user's pattern is clearly aimed at them.
pub fn is_internal(demangled: &str, mangled: &str) -> bool {
    demangled.starts_with("std::")
        || demangled.starts_with("core::")
        || demangled.starts_with("alloc::")
        || demangled.contains("::std::")
        || mangled.starts_with("__")
        || mangled.starts_with("pthread")
        || demangled.starts_with("pthread")
}

pub fn pattern_targets_internal(pattern: &str) -> bool {
    pattern.contains("std::")
        || pattern.contains("core::")
        || pattern.contains("alloc::")
        || pattern.starts_with("__")
        || pattern.contains("pthread")
}

pub fn keep_symbol(pattern: &str, demangled: &str, mangled: &str) -> bool {
    if !glob_match(pattern, demangled) && !glob_match(pattern, mangled) {
        return false;
    }
    if is_internal(demangled, mangled) && !pattern_targets_internal(pattern) {
        return false;
    }
    true
}

pub fn demangle(name: &str) -> String {
    format!("{:#}", rustc_demangle::demangle(name))
}

/// Parse function symbols from an ELF object.
pub fn parse_elf(path: &Path) -> Result<Vec<Symbol>, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    parse_elf_bytes(path, &bytes)
}

pub fn parse_elf_bytes(path: &Path, bytes: &[u8]) -> Result<Vec<Symbol>, String> {
    let elf = Elf::parse(bytes).map_err(|e| format!("ELF {}: {e}", path.display()))?;
    let mut out = Vec::new();
    push_funcs(path, elf.syms.iter(), &elf.strtab, &mut out);
    push_funcs(path, elf.dynsyms.iter(), &elf.dynstrtab, &mut out);
    out.sort_by(|a, b| a.demangled.cmp(&b.demangled));
    out.dedup_by(|a, b| a.mangled == b.mangled && a.object == b.object);
    Ok(out)
}

fn push_funcs<I>(
    path: &Path,
    syms: I,
    strtab: &goblin::strtab::Strtab<'_>,
    out: &mut Vec<Symbol>,
) where
    I: Iterator<Item = goblin::elf::sym::Sym>,
{
    for sym in syms {
        if sym.st_type() != STT_FUNC || sym.st_value == 0 || sym.st_shndx == 0 {
            continue;
        }
        let Some(name) = strtab.get_at(sym.st_name) else {
            continue;
        };
        if name.is_empty() || name == "$a" || name.starts_with('$') {
            continue;
        }
        out.push(Symbol {
            mangled: name.to_string(),
            demangled: demangle(name),
            object: path.to_path_buf(),
            vaddr: sym.st_value,
            enabled: false,
        });
    }
}

/// Load-bias for a PIE: runtime base of the first executable mapping of `path`.
/// Non-PIE (`ET_EXEC`) returns 0 so `vaddr` is already the runtime IP.
pub fn load_bias_from_maps(maps: &str, path: &Path) -> u64 {
    let needle = path.to_string_lossy();
    let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
    for line in maps.lines() {
        let mut bits = line.split_whitespace();
        let Some(range) = bits.next() else { continue };
        let Some(perms) = bits.next() else { continue };
        let Some(offset) = bits.next() else { continue };
        let _dev = bits.next();
        let _inode = bits.next();
        let file = bits.next().unwrap_or("");
        if !perms.contains('x') || file.is_empty() || file.starts_with('[') {
            continue;
        }
        let matches = file == needle || Path::new(file).file_name().and_then(|s| s.to_str()) == Some(name);
        if !matches {
            continue;
        }
        if offset != "00000000" && offset != "0" {
            continue;
        }
        if let Some(start) = range.split('-').next() {
            if let Ok(addr) = u64::from_str_radix(start, 16) {
                return addr;
            }
        }
    }
    0
}

#[cfg(target_os = "linux")]
pub fn process_maps(pid: u32) -> Result<String, String> {
    std::fs::read_to_string(format!("/proc/{pid}/maps"))
        .map_err(|e| format!("read /proc/{pid}/maps: {e}"))
}

#[cfg(target_os = "linux")]
pub fn process_exe(pid: u32) -> Result<PathBuf, String> {
    std::fs::read_link(format!("/proc/{pid}/exe"))
        .map_err(|e| format!("read /proc/{pid}/exe: {e}"))
}

/// Objects mapped executable for `pid` (the exe plus loaded .so files).
#[cfg(target_os = "linux")]
pub fn mapped_objects(pid: u32) -> Result<Vec<PathBuf>, String> {
    let maps = process_maps(pid)?;
    let mut out = Vec::new();
    for line in maps.lines() {
        let mut bits = line.split_whitespace();
        let _range = bits.next();
        let Some(perms) = bits.next() else { continue };
        let _off = bits.next();
        let _dev = bits.next();
        let _inode = bits.next();
        let file = bits.next().unwrap_or("");
        if !perms.contains('x') || file.is_empty() || file.starts_with('[') {
            continue;
        }
        let p = PathBuf::from(file);
        if !out.iter().any(|q| q == &p) {
            out.push(p);
        }
    }
    Ok(out)
}

#[cfg(target_os = "linux")]
pub fn load_process_symbols(pid: u32) -> Result<(Vec<Symbol>, Vec<(PathBuf, u64)>), String> {
    let maps = process_maps(pid)?;
    let objects = mapped_objects(pid)?;
    let mut symbols = Vec::new();
    let mut biases = Vec::new();
    for obj in &objects {
        if !obj.is_file() {
            continue;
        }
        let bias = load_bias_from_maps(&maps, obj);
        match parse_elf(obj) {
            Ok(mut syms) => {
                if !syms.is_empty() {
                    biases.push((obj.clone(), bias));
                }
                symbols.append(&mut syms);
            }
            Err(_) => continue,
        }
    }
    if symbols.is_empty() {
        return Err(format!(
            "pid {pid} looks stripped (no function symbols); rebuild without strip"
        ));
    }
    Ok((symbols, biases))
}

pub fn select_ids(symbols: &[Symbol], pattern: &str) -> Result<Vec<usize>, String> {
    if pattern.is_empty() {
        return Err("eBPF attach needs a pattern (refusing to probe every symbol)".into());
    }
    let mut ids: Vec<usize> = symbols
        .iter()
        .enumerate()
        .filter(|(_, s)| keep_symbol(pattern, &s.demangled, &s.mangled))
        .map(|(i, _)| i)
        .collect();
    if ids.is_empty() {
        return Err(format!("no symbols matching '{pattern}'"));
    }
    if ids.len() > MAX_ATTACH {
        ids.truncate(MAX_ATTACH);
    }
    Ok(ids)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn demangle_alternate_drops_hash() {
        let mangled = "_ZN11example_app14handle_request17h1234567890abcdefE";
        let d = demangle(mangled);
        assert!(d.contains("handle_request"), "{d}");
        assert!(!d.contains("h1234567890abcdef"), "{d}");
    }

    #[test]
    fn skips_std_unless_pattern_asks() {
        assert!(is_internal("std::vec::Vec::push", "_ZN3std3vec"));
        assert!(!keep_symbol("push", "std::vec::Vec::push", "_ZN3std"));
        assert!(keep_symbol("std::vec", "std::vec::Vec::push", "_ZN3std"));
        assert!(keep_symbol("handle_request", "example_app::handle_request", "_ZN11example"));
    }

    #[test]
    fn load_bias_reads_first_executable_mapping() {
        let maps = "\
00400000-00401000 r--p 00000000 00:00 1 /tmp/app\n\
00401000-0040b000 r-xp 00001000 00:00 1 /tmp/app\n\
7f000000-7f001000 r-xp 00000000 00:00 2 /tmp/app\n";
        // offset 0 + x: the third line
        assert_eq!(load_bias_from_maps(maps, Path::new("/tmp/app")), 0x7f000000);
    }

    #[test]
    fn select_ids_requires_pattern_and_caps() {
        let symbols: Vec<Symbol> = (0..80)
            .map(|i| Symbol {
                mangled: format!("fn{i}"),
                demangled: format!("demo::fn{i}"),
                object: PathBuf::from("/tmp/app"),
                vaddr: i,
                enabled: false,
            })
            .collect();
        assert!(select_ids(&symbols, "").is_err());
        let ids = select_ids(&symbols, "demo::").unwrap();
        assert_eq!(ids.len(), MAX_ATTACH);
    }
}
