// SPDX-License-Identifier: GPL-3.0-or-later
//! DST (Direct Stream Transfer) lossless decoder — a faithful port of
//! `libdstdec` (`dst_decoder.cpp`, `frame_reader.cpp`, `ac_data.cpp`,
//! `coded_table.cpp`). Decodes one DST frame to **interleaved, MSB-first** DSD
//! (the same clustered layout as raw SACD DSD frames), so it drops straight into
//! the DoP pipeline.
//!
//! DST is MPEG-4 (ISO/IEC 14496-3) arithmetic-coded DSD with adaptive FIR
//! prediction. Integer widths mirror the C exactly (i16 prediction wrap, u32
//! arithmetic-coder registers) so the output is bit-identical.

// A faithful port: index-based loops (often with neighbour indexing) and explicit
// range checks mirror the reference and read more clearly than iterator forms.
#![allow(clippy::needless_range_loop, clippy::manual_range_contains)]

mod bitstream;

use bitstream::{get_bit, log2_round_up, StrData};

use crate::error::{Error, Result};

// --- constants (dst_consts.h) ---------------------------------------------
const MAX_CHANNELS: usize = 6;
const MAX2CH: usize = 2 * MAX_CHANNELS;
const MAXNROF_FSEGS: i32 = 4;
const MAXNROF_PSEGS: i32 = 8;
const MAXNROF_SEGS: usize = 8;
const MIN_FSEG_LEN: i32 = 1024;
const MIN_PSEG_LEN: i32 = 32;
const SIZE_CODEDPREDORDER: i32 = 7;
const SIZE_PREDCOEF: i32 = 9;
const AC_BITS: i32 = 8;
const AC_HISBITS: i32 = 6;
const AC_HISMAX: usize = 1 << AC_HISBITS; // 64
const AC_QSTEP: i32 = SIZE_PREDCOEF - AC_HISBITS; // 3
const AC_PROBS: i32 = 1 << AC_BITS; // 256
const SIZE_RICEMETHOD: i32 = 2;
const SIZE_RICEM: i32 = 3;
const NROFFRICEMETHODS: usize = 3;
const MAXCPREDORDER: usize = 3;

// --- arithmetic decoder (ac_data.cpp) -------------------------------------
const PBITS: u32 = AC_BITS as u32; // 8
const ABITS: i32 = PBITS as i32 + 4; // 12
const ONE: u32 = 1 << ABITS; // 4096
const HALF: u32 = 1 << (ABITS - 1); // 2048

struct AcData {
    c: u32,
    a: u32,
    cbptr: i32,
}

impl AcData {
    fn new() -> Self {
        Self { c: 0, a: 0, cbptr: 0 }
    }

    fn ptable_index(predic_val: i32, ptable_len: i32) -> i32 {
        let mut j = predic_val.abs() >> AC_QSTEP;
        if j >= ptable_len {
            j = ptable_len - 1;
        }
        j
    }

    fn init(&mut self, cb: &[u8], fs: i32) {
        self.a = ONE - 1;
        self.c = 0;
        self.cbptr = 1;
        while self.cbptr <= ABITS {
            self.c <<= 1;
            if self.cbptr < fs {
                self.c |= get_bit(cb, self.cbptr as usize);
            }
            self.cbptr += 1;
        }
    }

    fn decode(&mut self, p: i32, cb: &[u8], fs: i32) -> u8 {
        let ap = ((self.a >> PBITS) | ((self.a >> (PBITS - 1)) & 1)) * p as u32;
        let h = self.a - ap;
        let b = if self.c >= h {
            self.c -= h;
            self.a = ap;
            0
        } else {
            self.a = h;
            1
        };
        while self.a < HALF {
            self.a <<= 1;
            self.c <<= 1;
            if self.cbptr < fs {
                self.c |= get_bit(cb, self.cbptr as usize);
            }
            self.cbptr += 1;
        }
        b
    }

    fn flush(&mut self, cb: &[u8], fs: i32) -> u8 {
        if self.cbptr < fs - 7 {
            0
        } else {
            let mut b = 1u8;
            while self.cbptr < fs && b == 1 {
                if get_bit(cb, self.cbptr as usize) != 0 {
                    b = 1;
                }
                self.cbptr += 1;
            }
            b
        }
    }
}

// --- frame structures (dst_defs.h / coded_table.h) ------------------------
#[derive(Clone)]
struct Segment {
    resolution: i32,
    segment_len: [[i32; MAXNROF_SEGS]; MAX_CHANNELS],
    nr_of_segments: [i32; MAX_CHANNELS],
    table4_segment: [[i32; MAXNROF_SEGS]; MAX_CHANNELS],
}

impl Segment {
    fn new() -> Self {
        Self {
            resolution: 0,
            segment_len: [[0; MAXNROF_SEGS]; MAX_CHANNELS],
            nr_of_segments: [0; MAX_CHANNELS],
            table4_segment: [[0; MAXNROF_SEGS]; MAX_CHANNELS],
        }
    }
}

/// Fixed Rice-prediction methods for filter coefficients / Ptable entries.
struct CodedTable {
    cpred_order: [i32; NROFFRICEMETHODS],
    cpred_coef: [[i32; MAXCPREDORDER]; NROFFRICEMETHODS],
    coded: [i32; MAX2CH],
    best_method: [i32; MAX2CH],
    m: [[i32; NROFFRICEMETHODS]; MAX2CH],
}

impl CodedTable {
    fn filter() -> Self {
        // CCodedTableF: orders/coefs for filter-coefficient prediction.
        Self {
            cpred_order: [1, 2, 3],
            cpred_coef: [[-8, 0, 0], [-16, 8, 0], [-9, -5, 6]],
            coded: [0; MAX2CH],
            best_method: [0; MAX2CH],
            m: [[0; NROFFRICEMETHODS]; MAX2CH],
        }
    }
    fn ptable() -> Self {
        // CCodedTableP: orders/coefs for Ptable-entry prediction.
        Self {
            cpred_order: [1, 2, 3],
            cpred_coef: [[-8, 0, 0], [-16, 8, 0], [-24, 24, -8]],
            coded: [0; MAX2CH],
            best_method: [0; MAX2CH],
            m: [[0; NROFFRICEMETHODS]; MAX2CH],
        }
    }
}

struct FrameHeader {
    nr_of_channels: i32,
    nr_of_filters: i32,
    nr_of_ptables: i32,
    max_frame_len: i32,
    nr_of_bits_per_ch: i32,
    max_nr_of_filters: i32,
    max_nr_of_ptables: i32,
    frame_nr: i32,
    pred_order: [i32; MAX2CH],
    ptable_len: [i32; MAX2CH],
    icoef_a: [[i16; 1 << SIZE_CODEDPREDORDER]; MAX2CH],
    dst_coded: i32,
    calc_nr_of_bits: i32,
    half_prob: [i32; MAX_CHANNELS],
    nr_of_half_bits: [i32; MAX_CHANNELS],
    fseg: Segment,
    pseg: Segment,
    p_same_seg_as_f: i32,
    p_same_map_as_f: i32,
    f_same_seg_all_ch: i32,
    f_same_map_all_ch: i32,
    p_same_seg_all_ch: i32,
    p_same_map_all_ch: i32,
}

impl FrameHeader {
    fn new() -> Self {
        Self {
            nr_of_channels: 0,
            nr_of_filters: 0,
            nr_of_ptables: 0,
            max_frame_len: 0,
            nr_of_bits_per_ch: 0,
            max_nr_of_filters: 0,
            max_nr_of_ptables: 0,
            frame_nr: 0,
            pred_order: [0; MAX2CH],
            ptable_len: [0; MAX2CH],
            icoef_a: [[0; 1 << SIZE_CODEDPREDORDER]; MAX2CH],
            dst_coded: 0,
            calc_nr_of_bits: 0,
            half_prob: [0; MAX_CHANNELS],
            nr_of_half_bits: [0; MAX_CHANNELS],
            fseg: Segment::new(),
            pseg: Segment::new(),
            p_same_seg_as_f: 0,
            p_same_map_as_f: 0,
            f_same_seg_all_ch: 0,
            f_same_map_all_ch: 0,
            p_same_seg_all_ch: 0,
            p_same_map_all_ch: 0,
        }
    }
}

fn err(msg: &str) -> Error {
    Error::Malformed(format!("DST: {msg}"))
}

// --- frame-header reading (frame_reader.cpp) ------------------------------

fn read_table_segment_data(
    sd: &mut StrData,
    nr_of_channels: i32,
    frame_len: i32,
    max_nr_of_segs: i32,
    min_seg_len: i32,
    s: &mut Segment,
    same_seg_all_ch: &mut i32,
) -> Result<()> {
    let mut defined_bits = 0i32;
    let mut resol_read = false;
    let mut max_seg_size = frame_len - min_seg_len / 8;
    *same_seg_all_ch = sd.get_uint(1) as i32;

    if *same_seg_all_ch != 0 {
        let mut seg_nr = 0usize;
        let mut end_of_channel = sd.get_uint(1) as i32;
        while end_of_channel == 0 {
            if seg_nr as i32 >= max_nr_of_segs {
                return Err(err("too many segments for this channel"));
            }
            if !resol_read {
                let nr_of_bits = log2_round_up(frame_len - min_seg_len / 8);
                s.resolution = sd.get_uint(nr_of_bits) as i32;
                if s.resolution == 0 || s.resolution > frame_len - min_seg_len / 8 {
                    return Err(err("invalid segment resolution"));
                }
                resol_read = true;
            }
            let nr_of_bits = log2_round_up(max_seg_size / s.resolution);
            s.segment_len[0][seg_nr] = sd.get_uint(nr_of_bits) as i32;
            let seg_bits = s.resolution * 8 * s.segment_len[0][seg_nr];
            if seg_bits < min_seg_len || seg_bits > frame_len * 8 - defined_bits - min_seg_len {
                return Err(err("invalid segment length"));
            }
            defined_bits += seg_bits;
            max_seg_size -= s.resolution * s.segment_len[0][seg_nr];
            seg_nr += 1;
            end_of_channel = sd.get_uint(1) as i32;
        }
        s.nr_of_segments[0] = seg_nr as i32 + 1;
        s.segment_len[0][seg_nr] = 0;
        for ch in 1..nr_of_channels as usize {
            s.nr_of_segments[ch] = s.nr_of_segments[0];
            for seg in 0..s.nr_of_segments[0] as usize {
                s.segment_len[ch][seg] = s.segment_len[0][seg];
            }
        }
    } else {
        let mut ch = 0usize;
        let mut seg_nr = 0usize;
        while (ch as i32) < nr_of_channels {
            if seg_nr as i32 >= max_nr_of_segs {
                return Err(err("too many segments for this channel"));
            }
            let end_of_channel = sd.get_uint(1) as i32;
            if end_of_channel == 0 {
                if !resol_read {
                    let nr_of_bits = log2_round_up(frame_len - min_seg_len / 8);
                    s.resolution = sd.get_uint(nr_of_bits) as i32;
                    if s.resolution == 0 || s.resolution > frame_len - min_seg_len / 8 {
                        return Err(err("invalid segment resolution"));
                    }
                    resol_read = true;
                }
                let nr_of_bits = log2_round_up(max_seg_size / s.resolution);
                s.segment_len[ch][seg_nr] = sd.get_uint(nr_of_bits) as i32;
                let seg_bits = s.resolution * 8 * s.segment_len[ch][seg_nr];
                if seg_bits < min_seg_len || seg_bits > frame_len * 8 - defined_bits - min_seg_len {
                    return Err(err("invalid segment length"));
                }
                defined_bits += seg_bits;
                max_seg_size -= s.resolution * s.segment_len[ch][seg_nr];
                seg_nr += 1;
            } else {
                s.nr_of_segments[ch] = seg_nr as i32 + 1;
                s.segment_len[ch][seg_nr] = 0;
                seg_nr = 0;
                defined_bits = 0;
                max_seg_size = frame_len - min_seg_len / 8;
                ch += 1;
            }
        }
    }
    if !resol_read {
        s.resolution = 1;
    }
    Ok(())
}

fn copy_segment_data(fh: &mut FrameHeader) -> Result<()> {
    fh.pseg.resolution = fh.fseg.resolution;
    fh.p_same_seg_all_ch = 1;
    for ch in 0..fh.nr_of_channels as usize {
        fh.pseg.nr_of_segments[ch] = fh.fseg.nr_of_segments[ch];
        if fh.pseg.nr_of_segments[ch] > MAXNROF_PSEGS {
            return Err(err("too many segments"));
        }
        if fh.pseg.nr_of_segments[ch] != fh.pseg.nr_of_segments[0] {
            fh.p_same_seg_all_ch = 0;
        }
        for seg in 0..fh.fseg.nr_of_segments[ch] as usize {
            fh.pseg.segment_len[ch][seg] = fh.fseg.segment_len[ch][seg];
            if fh.pseg.segment_len[ch][seg] != 0
                && fh.pseg.resolution * 8 * fh.pseg.segment_len[ch][seg] < MIN_PSEG_LEN
            {
                return Err(err("invalid segment length"));
            }
            if fh.pseg.segment_len[ch][seg] != fh.pseg.segment_len[0][seg] {
                fh.p_same_seg_all_ch = 0;
            }
        }
    }
    Ok(())
}

fn read_segment_data(sd: &mut StrData, fh: &mut FrameHeader) -> Result<()> {
    fh.p_same_seg_as_f = sd.get_uint(1) as i32;
    let mut same = fh.f_same_seg_all_ch;
    let nch = fh.nr_of_channels;
    let mfl = fh.max_frame_len;
    read_table_segment_data(sd, nch, mfl, MAXNROF_FSEGS, MIN_FSEG_LEN, &mut fh.fseg, &mut same)?;
    fh.f_same_seg_all_ch = same;
    if fh.p_same_seg_as_f == 1 {
        copy_segment_data(fh)?;
    } else {
        let mut psame = fh.p_same_seg_all_ch;
        read_table_segment_data(sd, nch, mfl, MAXNROF_PSEGS, MIN_PSEG_LEN, &mut fh.pseg, &mut psame)?;
        fh.p_same_seg_all_ch = psame;
    }
    Ok(())
}

fn read_table_mapping_data(
    sd: &mut StrData,
    nr_of_channels: i32,
    max_nr_of_tables: i32,
    s: &mut Segment,
    nr_of_tables: &mut i32,
    same_map_all_ch: &mut i32,
) -> Result<()> {
    let mut count_tables = 1i32;
    s.table4_segment[0][0] = 0;
    *same_map_all_ch = sd.get_uint(1) as i32;

    if *same_map_all_ch != 0 {
        for seg in 1..s.nr_of_segments[0] as usize {
            let nr_of_bits = log2_round_up(count_tables);
            s.table4_segment[0][seg] = sd.get_uint(nr_of_bits) as i32;
            if s.table4_segment[0][seg] == count_tables {
                count_tables += 1;
            } else if s.table4_segment[0][seg] > count_tables {
                return Err(err("invalid table number for segment"));
            }
        }
        for ch in 1..nr_of_channels as usize {
            if s.nr_of_segments[ch] != s.nr_of_segments[0] {
                return Err(err("mapping can't be the same for all channels"));
            }
            for seg in 0..s.nr_of_segments[0] as usize {
                s.table4_segment[ch][seg] = s.table4_segment[0][seg];
            }
        }
    } else {
        for ch in 0..nr_of_channels as usize {
            for seg in 0..s.nr_of_segments[ch] as usize {
                if ch != 0 || seg != 0 {
                    let nr_of_bits = log2_round_up(count_tables);
                    s.table4_segment[ch][seg] = sd.get_uint(nr_of_bits) as i32;
                    if s.table4_segment[ch][seg] == count_tables {
                        count_tables += 1;
                    } else if s.table4_segment[ch][seg] > count_tables {
                        return Err(err("invalid table number for segment"));
                    }
                }
            }
        }
    }
    if count_tables > max_nr_of_tables {
        return Err(err("too many tables for this frame"));
    }
    *nr_of_tables = count_tables;
    Ok(())
}

fn copy_mapping_data(fh: &mut FrameHeader) -> Result<()> {
    fh.p_same_map_all_ch = 1;
    for ch in 0..fh.nr_of_channels as usize {
        if fh.pseg.nr_of_segments[ch] == fh.fseg.nr_of_segments[ch] {
            for seg in 0..fh.fseg.nr_of_segments[ch] as usize {
                fh.pseg.table4_segment[ch][seg] = fh.fseg.table4_segment[ch][seg];
                if fh.pseg.table4_segment[ch][seg] != fh.pseg.table4_segment[0][seg] {
                    fh.p_same_map_all_ch = 0;
                }
            }
        } else {
            return Err(err("not the same number of segments for filters and Ptables"));
        }
    }
    fh.nr_of_ptables = fh.nr_of_filters;
    if fh.nr_of_ptables > fh.max_nr_of_ptables {
        return Err(err("too many tables for this frame"));
    }
    Ok(())
}

fn read_mapping_data(sd: &mut StrData, fh: &mut FrameHeader) -> Result<()> {
    fh.p_same_map_as_f = sd.get_uint(1) as i32;
    let nch = fh.nr_of_channels;
    let maxf = fh.max_nr_of_filters;
    let mut fsame = fh.f_same_map_all_ch;
    let mut nf = fh.nr_of_filters;
    read_table_mapping_data(sd, nch, maxf, &mut fh.fseg, &mut nf, &mut fsame)?;
    fh.nr_of_filters = nf;
    fh.f_same_map_all_ch = fsame;
    if fh.p_same_map_as_f == 1 {
        copy_mapping_data(fh)?;
    } else {
        let maxp = fh.max_nr_of_ptables;
        let mut psame = fh.p_same_map_all_ch;
        let mut np = fh.nr_of_ptables;
        read_table_mapping_data(sd, nch, maxp, &mut fh.pseg, &mut np, &mut psame)?;
        fh.nr_of_ptables = np;
        fh.p_same_map_all_ch = psame;
    }
    for i in 0..fh.nr_of_channels as usize {
        fh.half_prob[i] = sd.get_uint(1) as i32;
    }
    Ok(())
}

fn read_filter_coef_sets(sd: &mut StrData, fh: &mut FrameHeader, cf: &mut CodedTable) -> Result<()> {
    for filter in 0..fh.nr_of_filters as usize {
        fh.pred_order[filter] = sd.get_uint(SIZE_CODEDPREDORDER) as i32 + 1;
        cf.coded[filter] = sd.get_uint(1) as i32;
        if cf.coded[filter] == 0 {
            cf.best_method[filter] = -1;
            for coef in 0..fh.pred_order[filter] as usize {
                fh.icoef_a[filter][coef] = sd.get_short_signed(SIZE_PREDCOEF);
            }
        } else {
            cf.best_method[filter] = sd.get_uint(SIZE_RICEMETHOD) as i32;
            let bm = cf.best_method[filter] as usize;
            if cf.cpred_order[bm] >= fh.pred_order[filter] {
                return Err(err("invalid coefficient coding method"));
            }
            for coef in 0..cf.cpred_order[bm] as usize {
                fh.icoef_a[filter][coef] = sd.get_short_signed(SIZE_PREDCOEF);
            }
            cf.m[filter][bm] = sd.get_uint(SIZE_RICEM) as i32;
            for coef in cf.cpred_order[bm] as usize..fh.pred_order[filter] as usize {
                let mut x = 0i32;
                for tap in 0..cf.cpred_order[bm] as usize {
                    x += cf.cpred_coef[bm][tap] * fh.icoef_a[filter][coef - tap - 1] as i32;
                }
                let r = sd.rice_decode(cf.m[filter][bm]);
                let c = if x >= 0 {
                    r - (x + 4) / 8
                } else {
                    r + (-x + 3) / 8
                };
                if c < -(1 << (SIZE_PREDCOEF - 1)) || c >= (1 << (SIZE_PREDCOEF - 1)) {
                    return Err(err("filter coefficient out of range"));
                }
                fh.icoef_a[filter][coef] = c as i16;
            }
        }
    }
    for ch in 0..fh.nr_of_channels as usize {
        fh.nr_of_half_bits[ch] = fh.pred_order[fh.fseg.table4_segment[ch][0] as usize];
    }
    Ok(())
}

fn read_probability_tables(
    sd: &mut StrData,
    fh: &mut FrameHeader,
    cp: &mut CodedTable,
    p_one: &mut [[i32; AC_HISMAX]; MAX2CH],
) -> Result<()> {
    for pt in 0..fh.nr_of_ptables as usize {
        fh.ptable_len[pt] = sd.get_uint(AC_HISBITS) as i32 + 1;
        if fh.ptable_len[pt] > 1 {
            cp.coded[pt] = sd.get_uint(1) as i32;
            if cp.coded[pt] == 0 {
                cp.best_method[pt] = -1;
                for entry in 0..fh.ptable_len[pt] as usize {
                    p_one[pt][entry] = sd.get_uint(AC_BITS - 1) as i32 + 1;
                }
            } else {
                cp.best_method[pt] = sd.get_uint(SIZE_RICEMETHOD) as i32;
                let bm = cp.best_method[pt] as usize;
                if cp.cpred_order[bm] >= fh.ptable_len[pt] {
                    return Err(err("invalid Ptable coding method"));
                }
                for entry in 0..cp.cpred_order[bm] as usize {
                    p_one[pt][entry] = sd.get_uint(AC_BITS - 1) as i32 + 1;
                }
                cp.m[pt][bm] = sd.get_uint(SIZE_RICEM) as i32;
                for entry in cp.cpred_order[bm] as usize..fh.ptable_len[pt] as usize {
                    let mut x = 0i32;
                    for tap in 0..cp.cpred_order[bm] as usize {
                        x += cp.cpred_coef[bm][tap] * p_one[pt][entry - tap - 1];
                    }
                    let r = sd.rice_decode(cp.m[pt][bm]);
                    let c = if x >= 0 {
                        r - (x + 4) / 8
                    } else {
                        r + (-x + 3) / 8
                    };
                    if c < 1 || c > (1 << (AC_BITS - 1)) {
                        return Err(err("Ptable entry out of range"));
                    }
                    p_one[pt][entry] = c;
                }
            }
        } else {
            p_one[pt][0] = 128;
            cp.best_method[pt] = -1;
        }
    }
    Ok(())
}

/// `reverse7LSBs`: 7-bit reverse + 1, used to prime the arithmetic decoder.
fn reverse7_lsbs(c: i16) -> i32 {
    #[rustfmt::skip]
    const REVERSE: [i32; 128] = [
        1, 65, 33, 97, 17, 81, 49, 113, 9, 73, 41, 105, 25, 89, 57, 121,
        5, 69, 37, 101, 21, 85, 53, 117, 13, 77, 45, 109, 29, 93, 61, 125,
        3, 67, 35, 99, 19, 83, 51, 115, 11, 75, 43, 107, 27, 91, 59, 123,
        7, 71, 39, 103, 23, 87, 55, 119, 15, 79, 47, 111, 31, 95, 63, 127,
        2, 66, 34, 98, 18, 82, 50, 114, 10, 74, 42, 106, 26, 90, 58, 122,
        6, 70, 38, 102, 22, 86, 54, 118, 14, 78, 46, 110, 30, 94, 62, 126,
        4, 68, 36, 100, 20, 84, 52, 116, 12, 76, 44, 108, 28, 92, 60, 124,
        8, 72, 40, 104, 24, 88, 56, 120, 16, 80, 48, 112, 32, 96, 64, 128,
    ];
    REVERSE[((c as i32 + (1 << SIZE_PREDCOEF)) & 127) as usize]
}

// --- the decoder ----------------------------------------------------------

pub(crate) struct Decoder {
    fh: FrameHeader,
    str_filter: CodedTable,
    str_ptable: CodedTable,
    p_one: [[i32; AC_HISMAX]; MAX2CH],
    adata: Vec<u8>,
    adata_len: i32,
    sd: StrData,
    /// Per-bit filter index (low/high nibble per byte), `channels * stride`.
    filter4bit: Vec<u8>,
    ptable4bit: Vec<u8>,
    /// Bytes per channel in `filter4bit`/`ptable4bit` (`ceil(bits_per_ch / 2)`).
    stride: usize,
    /// Precomputed FIR lookup `[2*ch][16][256]`, flat: `f*16*256 + t*256 + s`.
    lt_icoef: Vec<i16>,
    out_bytes: usize,
}

impl Decoder {
    pub(crate) fn new(channels: usize) -> Self {
        // SACD audio is always DSD64 (fs44 = 64).
        let fs44 = 64;
        let max_frame_len = 588 * fs44 / 8;
        let nr_of_bits_per_ch = max_frame_len * 8;
        let stride = (nr_of_bits_per_ch as usize).div_ceil(2);

        let mut fh = FrameHeader::new();
        fh.nr_of_channels = channels as i32;
        fh.max_frame_len = max_frame_len;
        fh.nr_of_bits_per_ch = nr_of_bits_per_ch;
        fh.max_nr_of_filters = 2 * channels as i32;
        fh.max_nr_of_ptables = 2 * channels as i32;

        Self {
            fh,
            str_filter: CodedTable::filter(),
            str_ptable: CodedTable::ptable(),
            p_one: [[0; AC_HISMAX]; MAX2CH],
            adata: Vec::new(),
            adata_len: 0,
            sd: StrData::new(),
            filter4bit: vec![0u8; channels * stride],
            ptable4bit: vec![0u8; channels * stride],
            stride,
            lt_icoef: vec![0i16; 2 * channels * 16 * 256],
            out_bytes: max_frame_len as usize * channels,
        }
    }

    /// Decode one DST frame, appending interleaved MSB-first DSD to `out`.
    pub(crate) fn decode(&mut self, dst_frame: &[u8], out: &mut Vec<u8>) -> Result<()> {
        self.fh.frame_nr += 1;
        self.fh.calc_nr_of_bits = dst_frame.len() as i32 * 8;

        self.sd.fill(dst_frame);
        self.fh.dst_coded = self.sd.get_uint(1) as i32;

        if self.fh.dst_coded == 0 {
            let _xbit = self.sd.get_uint(1);
            if self.sd.get_uint(6) != 0 {
                return Err(err("illegal stuffing pattern"));
            }
            // Uncompressed DSD carried in the DST stream: copy straight out.
            let byte_max = (self.fh.max_frame_len * self.fh.nr_of_channels) as usize;
            for _ in 0..byte_max {
                out.push(self.sd.get_chr(8));
            }
            return Ok(());
        }

        read_segment_data(&mut self.sd, &mut self.fh)?;
        read_mapping_data(&mut self.sd, &mut self.fh)?;
        read_filter_coef_sets(&mut self.sd, &mut self.fh, &mut self.str_filter)?;
        read_probability_tables(&mut self.sd, &mut self.fh, &mut self.str_ptable, &mut self.p_one)?;
        self.adata_len = self.fh.calc_nr_of_bits - self.sd.in_bitcount();
        self.read_arithmetic_coded_data();
        if self.adata_len > 0 && get_bit(&self.adata, 0) != 0 {
            return Err(err("illegal arithmetic code"));
        }

        self.run_decode(out)
    }

    fn read_arithmetic_coded_data(&mut self) {
        let len = self.adata_len.max(0) as usize;
        // ceil(len/8) bytes + a little zero padding so `get_bit` never reads OOB.
        self.adata.clear();
        self.adata.resize(len.div_ceil(8) + 2, 0);
        for j in 0..(len >> 3) {
            self.adata[j] = self.sd.get_chr(8);
        }
        let mut val = 0u8;
        for j in (len & !7)..len {
            let v = self.sd.get_chr(1);
            val |= v << (7 - (j & 7));
            if j == len - 1 {
                self.adata[j >> 3] = val;
                val = 0;
            }
        }
        let _ = val;
    }

    /// Fill `dst[ch*stride..]` with the per-bit nibble table for `seg`.
    fn fill_table4bit(seg: &Segment, nr_of_channels: i32, nr_of_bits_per_ch: i32, stride: usize, dst: &mut [u8]) {
        for item in dst.iter_mut() {
            *item = 0;
        }
        for ch in 0..nr_of_channels as usize {
            let base = ch * stride;
            let mut start = 0i32;
            let mut seg_nr = 0usize;
            while (seg_nr as i32) < seg.nr_of_segments[ch] - 1 {
                let val = seg.table4_segment[ch][seg_nr] as u8;
                let end = start + seg.resolution * 8 * seg.segment_len[ch][seg_nr];
                for bitnr in start..end {
                    let s = (bitnr & 1) * 4;
                    let idx = base + (bitnr as usize >> 1);
                    dst[idx] = (val << s) | (dst[idx] & (0xf0u8 >> s));
                }
                start = end;
                seg_nr += 1;
            }
            let val = seg.table4_segment[ch][seg_nr] as u8;
            for bitnr in start..nr_of_bits_per_ch {
                let s = (bitnr & 1) * 4;
                let idx = base + (bitnr as usize >> 1);
                dst[idx] = (val << s) | (dst[idx] & (0xf0u8 >> s));
            }
        }
    }

    /// Precompute the per-filter FIR lookup tables (`LT_InitCoefTablesI`).
    fn init_coef_tables(&mut self) {
        for filter in 0..self.fh.nr_of_filters as usize {
            let filter_length = self.fh.pred_order[filter];
            for table in 0..16usize {
                let mut k = filter_length - table as i32 * 8;
                k = k.clamp(0, 8);
                for i in 0..256usize {
                    let mut cvalue = 0i32;
                    for j in 0..k as usize {
                        let sign = ((i >> j) & 1) as i32 * 2 - 1;
                        cvalue += sign * self.fh.icoef_a[filter][table * 8 + j] as i32;
                    }
                    self.lt_icoef[(filter * 16 + table) * 256 + i] = cvalue as i16;
                }
            }
        }
    }

    fn run_decode(&mut self, out: &mut Vec<u8>) -> Result<()> {
        let nch = self.fh.nr_of_channels as usize;
        let nbits = self.fh.nr_of_bits_per_ch;
        let stride = self.stride;

        // Filter/Ptable per-bit maps + FIR lookup tables.
        // (Borrow the buffers out so we can also borrow other fields.)
        let mut filter4bit = std::mem::take(&mut self.filter4bit);
        let mut ptable4bit = std::mem::take(&mut self.ptable4bit);
        Self::fill_table4bit(&self.fh.fseg, self.fh.nr_of_channels, nbits, stride, &mut filter4bit);
        Self::fill_table4bit(&self.fh.pseg, self.fh.nr_of_channels, nbits, stride, &mut ptable4bit);
        self.filter4bit = filter4bit;
        self.ptable4bit = ptable4bit;
        self.init_coef_tables();

        // FIR history per channel: 128 bits as 4 little-endian u32 words, 0xAA init.
        let mut status = vec![[0xAAAA_AAAAu32; 4]; nch];

        let mut ac = AcData::new();
        ac.init(&self.adata, self.adata_len);
        // Prime the decoder (result discarded), like the reference.
        let _ = ac.decode(reverse7_lsbs(self.fh.icoef_a[0][0]), &self.adata, self.adata_len);

        let start = out.len();
        out.resize(start + self.out_bytes, 0);

        for bitnr in 0..nbits {
            let byte_off = start + (bitnr as usize >> 3) * nch;
            let bit_shift = 7 - (bitnr & 7);
            for ch in 0..nch {
                let f_byte = self.filter4bit[ch * stride + (bitnr as usize >> 1)];
                let filter_nr = ((f_byte >> ((bitnr & 1) * 4)) & 0xf) as usize;

                // FIR prediction: sum the 16 table lookups (i16 wrap, as in C).
                let st = &status[ch];
                let base = filter_nr * 16 * 256;
                let mut predict = 0i32;
                for t in 0..16usize {
                    let sb = ((st[t >> 2] >> ((t & 3) * 8)) & 0xff) as usize;
                    predict += self.lt_icoef[base + t * 256 + sb] as i32;
                }
                let predict = predict as i16;

                let residual = if self.fh.half_prob[ch] != 0 && bitnr < self.fh.nr_of_half_bits[ch] {
                    ac.decode(AC_PROBS / 2, &self.adata, self.adata_len)
                } else {
                    let p_byte = self.ptable4bit[ch * stride + (bitnr as usize >> 1)];
                    let ptable_nr = ((p_byte >> ((bitnr & 1) * 4)) & 0xf) as usize;
                    let pidx = AcData::ptable_index(predict as i32, self.fh.ptable_len[ptable_nr]);
                    ac.decode(self.p_one[ptable_nr][pidx as usize], &self.adata, self.adata_len)
                };

                let bitval = (((predict as u16) >> 15) ^ residual as u16) & 1;
                out[byte_off + ch] |= (bitval as u8) << bit_shift;

                // Shift the 128-bit FIR history left by 1, inserting bitval.
                let st = &mut status[ch];
                st[3] = (st[3] << 1) | (st[2] >> 31);
                st[2] = (st[2] << 1) | (st[1] >> 31);
                st[1] = (st[1] << 1) | (st[0] >> 31);
                st[0] = (st[0] << 1) | bitval as u32;
            }
        }

        let ac_error = ac.flush(&self.adata, self.adata_len);
        if ac_error != 1 {
            return Err(err("arithmetic decoding error"));
        }
        Ok(())
    }
}
