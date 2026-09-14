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
use std::path::PathBuf;

use aya::maps::RingBuf;
use aya::programs::UProbe;
use aya::Ebpf;

use crate::event::RawEvent;
use crate::symbols::Symbol;

const PROG: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/rthas-ebpf-prog"));

pub struct BpfEngine {
    pid: u32,
    bpf: Option<Ebpf>,
    ring: Option<RingBuf<aya::maps::MapData>>,
    ips: HashMap<u64, usize>,
}

impl BpfEngine {
    pub fn new(pid: u32) -> Self {
        Self {
            pid,
            bpf: None,
            ring: None,
            ips: HashMap::new(),
        }
    }

    pub fn attach(&mut self, ids: &[usize], symbols: &[Symbol], biases: &[(PathBuf, u64)]) -> Result<(), String> {
        self.detach();
        let mut bpf = Ebpf::load(PROG).map_err(|e| format!("load eBPF object: {e}"))?;

        {
            let enter: &mut UProbe = bpf
                .program_mut("rthas_enter")
                .ok_or("missing rthas_enter")?
                .try_into()
                .map_err(|e| format!("rthas_enter: {e}"))?;
            enter.load().map_err(|e| format!("load rthas_enter: {e}"))?;
        }
        {
            let exit: &mut UProbe = bpf
                .program_mut("rthas_exit")
                .ok_or("missing rthas_exit")?
                .try_into()
                .map_err(|e| format!("rthas_exit: {e}"))?;
            exit.load().map_err(|e| format!("load rthas_exit: {e}"))?;
        }

        let mut ips = HashMap::new();
        for &id in ids {
            let sym = &symbols[id];
            let bias = biases
                .iter()
                .find(|(p, _)| p == &sym.object)
                .map(|(_, b)| *b)
                .unwrap_or(0);
            ips.insert(sym.runtime_ip(bias), id);

            {
                let enter: &mut UProbe = bpf
                    .program_mut("rthas_enter")
                    .unwrap()
                    .try_into()
                    .map_err(|e| format!("{e}"))?;
                enter
                    .attach(Some(sym.mangled.as_str()), 0, &sym.object, Some(self.pid as i32))
                    .map_err(|e| format!("uprobe {}: {e}", sym.demangled))?;
            }
            {
                let exit: &mut UProbe = bpf
                    .program_mut("rthas_exit")
                    .unwrap()
                    .try_into()
                    .map_err(|e| format!("{e}"))?;
                exit.attach(Some(sym.mangled.as_str()), 0, &sym.object, Some(self.pid as i32))
                    .map_err(|e| format!("uretprobe {}: {e}", sym.demangled))?;
            }
        }

        let map = bpf.take_map("EVENTS").ok_or("missing EVENTS ringbuf")?;
        let ring = RingBuf::try_from(map).map_err(|e| format!("EVENTS ringbuf: {e}"))?;
        self.bpf = Some(bpf);
        self.ring = Some(ring);
        self.ips = ips;
        Ok(())
    }

    pub fn detach(&mut self) {
        self.ring = None;
        self.bpf = None;
        self.ips.clear();
    }

    pub fn lookup(&self, ip: u64) -> Option<usize> {
        self.ips.get(&ip).copied()
    }

    pub fn poll(&mut self) -> Vec<RawEvent> {
        let Some(ring) = self.ring.as_mut() else {
            return Vec::new();
        };
        let mut out = Vec::new();
        while let Some(item) = ring.next() {
            if let Some(ev) = RawEvent::from_bytes(&item) {
                out.push(ev);
            }
        }
        out
    }
}
