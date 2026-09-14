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

/// Must stay in lockstep with `rthas-ebpf-prog`.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct RawEvent {
    pub ts_ns: u64,
    pub ip: u64,
    pub tid: u32,
    pub kind: u8,
    pub _pad: [u8; 3],
}

pub const KIND_ENTER: u8 = 0;
pub const KIND_EXIT: u8 = 1;

impl RawEvent {
    pub fn from_bytes(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < core::mem::size_of::<Self>() {
            return None;
        }
        // SAFETY: `RawEvent` is repr(C), POD, and we checked the length.
        Some(unsafe { core::ptr::read_unaligned(bytes.as_ptr().cast::<Self>()) })
    }
}
