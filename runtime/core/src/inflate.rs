// SPDX-License-Identifier: MIT
//! zlib streams (RFC 1950) and the deflate data in them (RFC 1951), decoded: what xnuhost reads of a kernel's
//! gpuaddon sidecar (oneios-tool's NGPUADD2 packs each file as a zlib stream), so the session can tell which Metal
//! plug-in it carries and which guest build it names before the guest is up. A plain decoder in the manner of zlib's
//! puff.c (Mark Adler's reference inflate): canonical Huffman codes decoded a bit at a time with counts and symbols.
//! Not fast, and it need not be: the files it reads are a few bytes (a plug-in's `oneios-target`).

/// Why a stream was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// The input ended inside the stream.
    Truncated,
    /// Not a zlib header (method 8, window up to 32 KiB, the check bits), or a preset dictionary.
    Header,
    /// A block of type 3, a stored block's length that its complement does not match, or a code set that is not one.
    Invalid,
    /// A distance before the start of the output.
    Distance,
    /// More output than the caller allows.
    TooLarge,
    /// The Adler-32 at the end is not the output's.
    Checksum,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Error::Truncated => "the stream is cut short",
            Error::Header => "not a zlib stream",
            Error::Invalid => "not valid deflate data",
            Error::Distance => "a distance before the start",
            Error::TooLarge => "larger than allowed",
            Error::Checksum => "the Adler-32 does not match",
        })
    }
}

struct Bits<'a> {
    data: &'a [u8],
    at: usize,
    buf: u32,
    count: u32,
}

impl Bits<'_> {
    fn bits(&mut self, need: u32) -> Result<u32, Error> {
        let mut val = self.buf as u64;
        while self.count < need {
            let byte = *self.data.get(self.at).ok_or(Error::Truncated)?;
            self.at += 1;
            val |= (byte as u64) << self.count;
            self.count += 8;
        }
        self.buf = (val >> need) as u32;
        self.count -= need;
        Ok((val & ((1u64 << need) - 1)) as u32)
    }

    fn align(&mut self) {
        self.buf = 0;
        self.count = 0;
    }
}

const MAX_BITS: usize = 15;

/// A canonical Huffman code: how many codes of each length, and the symbols in code order.
struct Huffman {
    count: [u16; MAX_BITS + 1],
    symbol: Vec<u16>,
}

impl Huffman {
    /// The code of these lengths (0: the symbol is not used). Err when the lengths over-subscribe the code; an
    /// incomplete code is taken (deflate allows one for a single distance code).
    fn new(lengths: &[u8]) -> Result<Huffman, Error> {
        let mut count = [0u16; MAX_BITS + 1];
        for &l in lengths {
            count[l as usize] += 1;
        }
        let mut left: i32 = 1;
        for &c in &count[1..] {
            left <<= 1;
            left -= c as i32;
            if left < 0 {
                return Err(Error::Invalid);
            }
        }
        let mut offs = [0u16; MAX_BITS + 2];
        for len in 1..=MAX_BITS {
            offs[len + 1] = offs[len] + count[len];
        }
        let mut symbol = vec![0u16; lengths.len()];
        for (sym, &l) in lengths.iter().enumerate() {
            if l != 0 {
                symbol[offs[l as usize] as usize] = sym as u16;
                offs[l as usize] += 1;
            }
        }
        Ok(Huffman { count, symbol })
    }

    fn decode(&self, s: &mut Bits<'_>) -> Result<u16, Error> {
        let (mut code, mut first, mut index) = (0i32, 0i32, 0i32);
        for len in 1..=MAX_BITS {
            code |= s.bits(1)? as i32;
            let count = self.count[len] as i32;
            if code - count < first {
                return self.symbol.get((index + (code - first)) as usize).copied().ok_or(Error::Invalid);
            }
            index += count;
            first += count;
            first <<= 1;
            code <<= 1;
        }
        Err(Error::Invalid)
    }
}

const LEN_BASE: [u16; 29] =
    [3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115, 131, 163, 195, 227, 258];
const LEN_EXTRA: [u8; 29] = [0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0];
const DIST_BASE: [u16; 30] = [
    1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537, 2049, 3073, 4097, 6145,
    8193, 12289, 16385, 24577,
];
const DIST_EXTRA: [u8; 30] =
    [0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13, 13];

fn codes(s: &mut Bits<'_>, out: &mut Vec<u8>, max: usize, lit: &Huffman, dist: &Huffman) -> Result<(), Error> {
    loop {
        let sym = lit.decode(s)? as usize;
        if sym < 256 {
            if out.len() >= max {
                return Err(Error::TooLarge);
            }
            out.push(sym as u8);
        } else if sym == 256 {
            return Ok(());
        } else {
            let i = sym - 257;
            if i >= 29 {
                return Err(Error::Invalid);
            }
            let len = LEN_BASE[i] as usize + s.bits(LEN_EXTRA[i] as u32)? as usize;
            let d = dist.decode(s)? as usize;
            if d >= 30 {
                return Err(Error::Invalid);
            }
            let back = DIST_BASE[d] as usize + s.bits(DIST_EXTRA[d] as u32)? as usize;
            if back > out.len() {
                return Err(Error::Distance);
            }
            if out.len() + len > max {
                return Err(Error::TooLarge);
            }
            let from = out.len() - back;
            for k in 0..len {
                let b = out[from + k];
                out.push(b);
            }
        }
    }
}

fn fixed() -> (Huffman, Huffman) {
    let mut l = [0u8; 288];
    l[..144].fill(8);
    l[144..256].fill(9);
    l[256..280].fill(7);
    l[280..].fill(8);
    let lit = Huffman::new(&l).expect("the fixed code");
    let dist = Huffman::new(&[5u8; 30]).expect("the fixed code");
    (lit, dist)
}

fn dynamic(s: &mut Bits<'_>) -> Result<(Huffman, Huffman), Error> {
    const ORDER: [usize; 19] = [16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15];
    let nlen = s.bits(5)? as usize + 257;
    let ndist = s.bits(5)? as usize + 1;
    let ncode = s.bits(4)? as usize + 4;
    if nlen > 286 || ndist > 30 {
        return Err(Error::Invalid);
    }
    let mut lengths = [0u8; 320];
    for &o in &ORDER[..ncode] {
        lengths[o] = s.bits(3)? as u8;
    }
    let lencode = Huffman::new(&lengths[..19])?;
    let mut index = 0;
    while index < nlen + ndist {
        let sym = lencode.decode(s)?;
        if sym < 16 {
            lengths[index] = sym as u8;
            index += 1;
            continue;
        }
        let (value, repeat) = match sym {
            16 => {
                let prev = *lengths.get(index.wrapping_sub(1)).filter(|_| index > 0).ok_or(Error::Invalid)?;
                (prev, 3 + s.bits(2)? as usize)
            }
            17 => (0, 3 + s.bits(3)? as usize),
            _ => (0, 11 + s.bits(7)? as usize),
        };
        if index + repeat > nlen + ndist {
            return Err(Error::Invalid);
        }
        lengths[index..index + repeat].fill(value);
        index += repeat;
    }
    if lengths[256] == 0 {
        return Err(Error::Invalid); // no end-of-block code
    }
    Ok((Huffman::new(&lengths[..nlen])?, Huffman::new(&lengths[nlen..nlen + ndist])?))
}

/// The raw deflate data at the start of `data`, at most `max` bytes of output: the output and the bytes read.
pub fn deflate(data: &[u8], max: usize) -> Result<(Vec<u8>, usize), Error> {
    let mut s = Bits { data, at: 0, buf: 0, count: 0 };
    let mut out = Vec::new();
    loop {
        let last = s.bits(1)?;
        match s.bits(2)? {
            0 => {
                s.align();
                let at = s.at;
                let head = data.get(at..at + 4).ok_or(Error::Truncated)?;
                let len = u16::from_le_bytes([head[0], head[1]]);
                if len != !u16::from_le_bytes([head[2], head[3]]) {
                    return Err(Error::Invalid);
                }
                let body = data.get(at + 4..at + 4 + len as usize).ok_or(Error::Truncated)?;
                if out.len() + body.len() > max {
                    return Err(Error::TooLarge);
                }
                out.extend_from_slice(body);
                s.at = at + 4 + len as usize;
            }
            1 => {
                let (lit, dist) = fixed();
                codes(&mut s, &mut out, max, &lit, &dist)?;
            }
            2 => {
                let (lit, dist) = dynamic(&mut s)?;
                codes(&mut s, &mut out, max, &lit, &dist)?;
            }
            _ => return Err(Error::Invalid),
        }
        if last == 1 {
            return Ok((out, s.at));
        }
    }
}

/// The Adler-32 of `data` (RFC 1950 8.2).
pub fn adler32(data: &[u8]) -> u32 {
    let (mut a, mut b) = (1u32, 0u32);
    for chunk in data.chunks(5552) {
        for &x in chunk {
            a += x as u32;
            b += a;
        }
        a %= 65521;
        b %= 65521;
    }
    b << 16 | a
}

/// A zlib stream (its header, the deflate data, the Adler-32), at most `max` bytes of output.
pub fn zlib(data: &[u8], max: usize) -> Result<Vec<u8>, Error> {
    let (cmf, flg) = match data {
        [c, f, ..] => (*c, *f),
        _ => return Err(Error::Truncated),
    };
    if cmf & 0x0f != 8 || cmf >> 4 > 7 || ((cmf as u16) << 8 | flg as u16) % 31 != 0 || flg & 0x20 != 0 {
        return Err(Error::Header);
    }
    let (out, used) = deflate(&data[2..], max)?;
    let tail = data.get(2 + used..2 + used + 4).ok_or(Error::Truncated)?;
    if u32::from_be_bytes([tail[0], tail[1], tail[2], tail[3]]) != adler32(&out) {
        return Err(Error::Checksum);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    // `printf 'iPhone10,3 20F66\n' | zlib-flate -compress=9` (a fixed-code block) and `-compress=0` (a stored one).
    const FIXED: [u8; 25] = [
        0x78, 0xda, 0xcb, 0x0c, 0xc8, 0xc8, 0xcf, 0x4b, 0x35, 0x34, 0xd0, 0x31, 0x56, 0x30, 0x32, 0x70, 0x33, 0x33,
        0xe3, 0x02, 0x00, 0x2e, 0xd1, 0x04, 0x62,
    ];
    const STORED: [u8; 28] = [
        0x78, 0x01, 0x01, 0x11, 0x00, 0xee, 0xff, 0x69, 0x50, 0x68, 0x6f, 0x6e, 0x65, 0x31, 0x30, 0x2c, 0x33, 0x20,
        0x32, 0x30, 0x46, 0x36, 0x36, 0x0a, 0x2e, 0xd1, 0x04, 0x62,
    ];

    #[test]
    fn a_fixed_and_a_stored_block() {
        assert_eq!(zlib(&FIXED, 64).unwrap(), b"iPhone10,3 20F66\n");
        assert_eq!(zlib(&STORED, 64).unwrap(), b"iPhone10,3 20F66\n");
    }

    #[test]
    fn dynamic_codes_and_back_references() {
        // lines.z: `seq 0 199 | awk '{printf "line %d: the plug-in OneiOSRadeonMetal, build 20F66, ring %d\n", $1, $1 % 7}'
        // | zlib-flate -compress=9` (673 bytes, dynamic Huffman blocks with long matches).
        let want: String = (0..200)
            .map(|n| format!("line {n}: the plug-in OneiOSRadeonMetal, build 20F66, ring {}\n", n % 7))
            .collect();
        assert_eq!(zlib(include_bytes!("inflate/lines.z"), 1 << 20).unwrap(), want.as_bytes());
    }

    #[test]
    fn refusals() {
        assert_eq!(zlib(&FIXED[..10], 64), Err(Error::Truncated));
        assert_eq!(zlib(&FIXED, 8), Err(Error::TooLarge));
        let mut bad = FIXED;
        bad[24] ^= 1;
        assert_eq!(zlib(&bad, 64), Err(Error::Checksum));
        bad = FIXED;
        bad[1] ^= 1;
        assert_eq!(zlib(&bad, 64), Err(Error::Header));
        let mut stored = STORED;
        stored[5] ^= 1; // the length's complement
        assert_eq!(zlib(&stored, 64), Err(Error::Invalid));
        // a block of type 3
        assert_eq!(deflate(&[0x07], 64), Err(Error::Invalid));
        assert_eq!(adler32(b"Wikipedia"), 0x11e6_0398);
    }
}
