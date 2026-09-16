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

#![no_std]
#![no_main]

use aya_ebpf::helpers::gen::bpf_get_func_ip;
use aya_ebpf::helpers::{bpf_get_current_pid_tgid, bpf_ktime_get_ns};
use aya_ebpf::macros::{map, uprobe, uretprobe};
use aya_ebpf::maps::RingBuf;
use aya_ebpf::programs::{ProbeContext, RetProbeContext};
use aya_ebpf::EbpfContext;

/// Must stay in lockstep with `rthas_ebpf::event::RawEvent`.
#[repr(C)]
#[derive(Copy, Clone)]
struct RawEvent {
    ts_ns: u64,
    ip: u64,
    tid: u32,
    kind: u8,
    _pad: [u8; 3],
}

#[map]
static EVENTS: RingBuf = RingBuf::with_byte_size(256 * 1024, 0);

const KIND_ENTER: u8 = 0;
const KIND_EXIT: u8 = 1;

#[uprobe]
pub fn rthas_enter(ctx: ProbeContext) -> u32 {
    submit(&ctx, KIND_ENTER)
}

#[uretprobe]
pub fn rthas_exit(ctx: RetProbeContext) -> u32 {
    submit(&ctx, KIND_EXIT)
}

fn submit(ctx: &impl EbpfContext, kind: u8) -> u32 {
    let Some(mut slot) = EVENTS.reserve::<RawEvent>(0) else {
        return 0;
    };
    let ev = RawEvent {
        ts_ns: bpf_ktime_get_ns(),
        // SAFETY: `ctx` is the probe context the kernel passed to this program.
        ip: unsafe { bpf_get_func_ip(ctx.as_ptr()) },
        tid: bpf_get_current_pid_tgid() as u32,
        kind,
        _pad: [0; 3],
    };
    slot.write(ev);
    slot.submit(0);
    0
}

#[cfg(not(test))]
#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    loop {}
}
