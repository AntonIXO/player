// SPDX-License-Identifier: GPL-3.0-or-later
//! MSB-first bit reader over the DST frame, plus Rice decoding — a port of
//! `str_data.cpp` (`CStrData`) and `CFrameReader::RiceDecode` / `log2RoundUp`.

/// Reads big-endian (MSB-first) bit fields from a byte buffer.
pub(crate) struct StrData {
    data: Vec<u8>,
    total_bytes: usize,
    byte_counter: usize,
    bit_position: i32,
    data_byte: u8,
}

impl StrData {
    pub(crate) fn new() -> Self {
        Self {
            data: Vec::new(),
            total_bytes: 0,
            byte_counter: 0,
            bit_position: 0,
            data_byte: 0,
        }
    }

    /// Load a DST frame and reset the read cursor.
    pub(crate) fn fill(&mut self, buf: &[u8]) {
        self.data.clear();
        self.data.extend_from_slice(buf);
        self.total_bytes = buf.len();
        self.byte_counter = 0;
        self.bit_position = 0;
        self.data_byte = 0;
    }

    fn next_byte(&mut self) -> u8 {
        // Past the buffer reads as 0 (well-formed frames never rely on it); the
        // byte counter still advances so `in_bitcount` stays exact.
        let b = self.data.get(self.byte_counter).copied().unwrap_or(0);
        self.byte_counter += 1;
        b
    }

    /// Read `nbits` (0..=32) MSB-first into an unsigned value.
    fn get_bits(&mut self, mut nbits: i32) -> u32 {
        const MASKS: [u32; 9] = [0, 1, 3, 7, 0xf, 0x1f, 0x3f, 0x7f, 0xff];
        let mut outword: u32 = 0;
        while nbits > 0 {
            if self.bit_position == 0 {
                self.data_byte = self.next_byte();
                self.bit_position = 8;
            }
            let thisbits = self.bit_position.min(nbits);
            let shift = self.bit_position - thisbits;
            let mask = MASKS[thisbits as usize] << shift;
            let shift2 = (nbits - thisbits) - shift;
            if shift2 <= 0 {
                outword |= (self.data_byte as u32 & mask) >> (-shift2);
            } else {
                outword |= (self.data_byte as u32 & mask) << shift2;
            }
            nbits -= thisbits;
            self.bit_position -= thisbits;
        }
        outword
    }

    pub(crate) fn get_uint(&mut self, nbits: i32) -> u32 {
        if nbits > 0 {
            self.get_bits(nbits)
        } else {
            0
        }
    }

    pub(crate) fn get_chr(&mut self, nbits: i32) -> u8 {
        self.get_uint(nbits) as u8
    }

    /// `nbits`-bit two's-complement signed value.
    pub(crate) fn get_int_signed(&mut self, nbits: i32) -> i32 {
        if nbits <= 0 {
            return 0;
        }
        let mut x = self.get_bits(nbits) as i32;
        if x >= (1 << (nbits - 1)) {
            x -= 1 << nbits;
        }
        x
    }

    pub(crate) fn get_short_signed(&mut self, nbits: i32) -> i16 {
        self.get_int_signed(nbits) as i16
    }

    /// Bits consumed so far (`ByteCounter * 8 - BitPosition`).
    pub(crate) fn in_bitcount(&self) -> i32 {
        self.byte_counter as i32 * 8 - self.bit_position
    }

    /// Read one Rice code with parameter `m` (`CFrameReader::RiceDecode`).
    pub(crate) fn rice_decode(&mut self, m: i32) -> i32 {
        // Unary run length: count zero bits up to the terminating one bit.
        let mut run_length = 0i32;
        loop {
            let rl_bit = self.get_uint(1);
            run_length += 1 - rl_bit as i32;
            if rl_bit != 0 {
                break;
            }
        }
        let lsbs = self.get_uint(m) as i32;
        let mut nr = (run_length << m) + lsbs;
        if nr != 0 && self.get_uint(1) != 0 {
            nr = -nr;
        }
        nr
    }
}

/// `log2RoundUp`: smallest `y` with `x < (1 << y)`.
pub(crate) fn log2_round_up(x: i32) -> i32 {
    let mut y = 0;
    while x >= (1 << y) {
        y += 1;
    }
    y
}

/// MSB-first single-bit access into a packed byte array (the `GET_BIT` macro),
/// used by the arithmetic decoder over the AData buffer.
#[inline]
pub(crate) fn get_bit(data: &[u8], idx: usize) -> u32 {
    ((data[idx >> 3] >> (7 - (idx & 7))) & 1) as u32
}
