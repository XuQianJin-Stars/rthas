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

pub fn format_dur(ns: u64) -> String {
    if ns < 1_000 {
        return format!("{ns}ns");
    }
    let us = ns as f64 / 1_000.0;
    if us < 1_000.0 {
        return format!("{us:.3}us");
    }
    let ms = us / 1_000.0;
    if ms < 1_000.0 {
        return format!("{ms:.3}ms");
    }
    format!("{:.3}s", ms / 1_000.0)
}

#[cfg(test)]
mod tests {
    use super::format_dur;

    #[test]
    fn duration_units_scale() {
        assert_eq!(format_dur(999), "999ns");
        assert_eq!(format_dur(1_500), "1.500us");
        assert_eq!(format_dur(1_500_000), "1.500ms");
    }
}
