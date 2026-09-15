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

//! Minimal RFC 4648 base64 for `base64` / `base64 -d` (no extra crate).

const ALPH: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

pub fn encode(data: &[u8]) -> String {
    let mut out = String::new();
    let mut i = 0;
    while i < data.len() {
        let b0 = data[i];
        let b1 = data.get(i + 1).copied();
        let b2 = data.get(i + 2).copied();
        out.push(ALPH[(b0 >> 2) as usize] as char);
        match (b1, b2) {
            (Some(b1), Some(b2)) => {
                out.push(ALPH[(((b0 & 0x03) << 4) | (b1 >> 4)) as usize] as char);
                out.push(ALPH[(((b1 & 0x0f) << 2) | (b2 >> 6)) as usize] as char);
                out.push(ALPH[(b2 & 0x3f) as usize] as char);
            }
            (Some(b1), None) => {
                out.push(ALPH[(((b0 & 0x03) << 4) | (b1 >> 4)) as usize] as char);
                out.push(ALPH[((b1 & 0x0f) << 2) as usize] as char);
                out.push('=');
            }
            (None, _) => {
                out.push(ALPH[((b0 & 0x03) << 4) as usize] as char);
                out.push('=');
                out.push('=');
            }
        }
        i += 3;
    }
    out
}

pub fn decode(s: &str) -> Result<Vec<u8>, String> {
    let mut buf = Vec::new();
    let mut quad = [0u8; 4];
    let mut n = 0;
    for c in s.chars() {
        if c.is_whitespace() {
            continue;
        }
        if n == 4 {
            flush_quad(&quad, 4, &mut buf)?;
            n = 0;
        }
        quad[n] = if c == '=' { 0xff } else { decode_val(c)? };
        n += 1;
    }
    if n > 0 {
        if n != 4 {
            return Err("invalid base64 length".into());
        }
        flush_quad(&quad, 4, &mut buf)?;
    }
    Ok(buf)
}

fn decode_val(c: char) -> Result<u8, String> {
    match c {
        'A'..='Z' => Ok(c as u8 - b'A'),
        'a'..='z' => Ok(26 + (c as u8 - b'a')),
        '0'..='9' => Ok(52 + (c as u8 - b'0')),
        '+' => Ok(62),
        '/' => Ok(63),
        _ => Err(format!("invalid base64 character {c:?}")),
    }
}

fn flush_quad(q: &[u8; 4], _n: usize, out: &mut Vec<u8>) -> Result<(), String> {
    let pad = q.iter().filter(|b| **b == 0xff).count();
    let v0 = if q[0] == 0xff { 0 } else { q[0] };
    let v1 = if q[1] == 0xff { 0 } else { q[1] };
    let v2 = if q[2] == 0xff { 0 } else { q[2] };
    let v3 = if q[3] == 0xff { 0 } else { q[3] };
    out.push((v0 << 2) | (v1 >> 4));
    if pad < 2 {
        out.push((v1 << 4) | (v2 >> 2));
    }
    if pad < 1 {
        out.push((v2 << 6) | v3);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let raw = b"abc\n";
        let enc = encode(raw);
        assert_eq!(enc, "YWJjCg==");
        assert_eq!(decode(&enc).unwrap(), raw);
        assert_eq!(decode("YWJj").unwrap(), b"abc");
    }
}
