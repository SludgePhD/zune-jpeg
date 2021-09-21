#![allow(clippy::similar_names, clippy::module_name_repetitions)]
use std::convert::TryFrom;

use crate::errors::DecodeErrors;

// # bits of lookahead
pub const HUFF_LOOKAHEAD: u8 = 9;

pub struct HuffmanTable {
    /// element[0] of each array is unused
    /// largest code of length k
    pub(crate) maxcode: [i32; 18],
    /// offset for codes of length k
    /// Answers the question, where do code-lengths of length k end
    /// Element 0 is unused
    pub(crate) offset: [i32; 18],
    /// lookup table for fast decoding
    ///
    /// top  bits above HUFF_LOOKAHEAD (8) contain the code length.
    ///
    /// Lower (8) bits contain the symbol in order of increasing code length.
    pub(crate) lookup: [i32; 1 << HUFF_LOOKAHEAD],

    /// A table which can be used to decode small AC coefficients and
    /// do an equivalent of receive_extend
    pub(crate) ac_lookup: Option<[i16; 1 << HUFF_LOOKAHEAD]>,

    /// Directly represent contents of a JPEG DHT marker
    ///
    /// \# number of symbols with codes of length `k` bits
    /// bits[0] is unused
    pub(crate) bits: [u8; 17],
    /// Symbols in order of increasing code length
    pub(crate) values: Vec<u8>,
}

impl HuffmanTable {
    pub fn new(codes: &[u8; 17], values: Vec<u8>, is_dc: bool) -> HuffmanTable {
        let mut p = HuffmanTable {
            maxcode: [0; 18],
            offset: [0; 18],
            lookup: [0; 1 << HUFF_LOOKAHEAD],
            bits: codes.to_owned(),
            values,
            ac_lookup: None,
        };
        p.make_derived_table(is_dc)
            .expect("Could not create a Huffman table");
        p
    }
    /// Compute derived values for a Huffman table
    ///
    /// This routine performs some validation checks on the table
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_possible_wrap,
        clippy::cast_sign_loss
    )]
    fn make_derived_table(&mut self, is_dc: bool) -> Result<(), DecodeErrors> {
        // build a list of code size
        let mut size = [0; 257];
        // Huffman code lengths
        let mut huff_code: [u32; 257] = [0; 257];

        // figure C.1 make table of Huffman code length for each symbol
        let mut p = 0;
        for l in 1..=16 {
            let mut i = i32::from(self.bits[l]);
            // table overrun is checked before ,so we dont need to check
            while i != 0 {
                size[p] = l as u8;
                p += 1;
                i -= 1;
            }
        }
        size[p] = 0;
        let num_symbols = p;

        // Generate the codes themselves
        // We also validate that the counts represent a legal Huffman code tree
        let mut code = 0;
        let mut si = i32::from(size[0]);
        p = 0;

        while size[p] != 0 {
            while i32::from(size[p]) == si {
                huff_code[p] = code;
                code += 1;
                p += 1;
            }
            // code is now 1 more than the last code used for code-length si; but
            // it must still fit in si bits, since no code is allowed to be all ones.
            if i32::try_from(code).expect("Overflow, weee") >= (1 << si) {
                return Err(DecodeErrors::HuffmanDecode("Bad Huffman Table".to_string()));
            }
            code <<= 1;
            si += 1;
        }
        // Figure F.15 generate decoding tables for bit-sequential decoding
        p = 0;
        for l in 0..=16 {
            if self.bits[l] == 0 {
                // -1 if no codes of this length
                self.maxcode[l] = -1;
            } else {
                // offset[l]=codes[index of 1st symbol of code length l
                // minus minimum code of length l]
                self.offset[l] =
                    i32::try_from(p).expect("Overflow error wee") - (huff_code[p]) as i32;
                p += usize::from(self.bits[l]);
                // maximum code of length l
                self.maxcode[l] = huff_code[p - 1] as i32;
            }
        }
        self.offset[17] = 0;
        // we ensure that decode terminates
        self.maxcode[17] = 0x000F_FFFF;

        /*
         * Compute lookahead tables to speed up decoding.
         * First we set all the table entries to 0(left justified), indicating "too long";
         * then we iterate through the Huffman codes that are short enough and
         * fill in all the entries that correspond to bit sequences starting
         * with that code.
         */
        for i in 0..(1 << HUFF_LOOKAHEAD) {
            self.lookup[i] = (i32::from(HUFF_LOOKAHEAD) + 1) << HUFF_LOOKAHEAD;
        }
        p = 0;
        for l in 1..=HUFF_LOOKAHEAD {
            for _ in 1..=i32::from(self.bits[usize::from(l)]) {
                // l -> Current code length,
                // p => Its index in self.code and self.values
                // Generate left justified code followed by all possible bit sequences
                let mut look_bits = (huff_code[p] as usize) << (HUFF_LOOKAHEAD - l);
                for _ in 0..1 << (HUFF_LOOKAHEAD - l) {
                    self.lookup[look_bits] =
                        (i32::from(l) << HUFF_LOOKAHEAD) | i32::from(self.values[p]);
                    look_bits += 1;
                }
                p += 1;
            }
        }

        // build an ac table that does an equivalent of decode and receive_extend
        if !is_dc {
            let mut fast = [255; 1 << HUFF_LOOKAHEAD];
            for i in 0..num_symbols {
                let s = size[i];
                if s <= HUFF_LOOKAHEAD {
                    let c = (huff_code[i] << (HUFF_LOOKAHEAD - s)) as usize;
                    let m = (1 << (HUFF_LOOKAHEAD - s)) as usize;
                    for j in 0..m {
                        fast[c + j] = i as i16;
                    }
                }
            }

            // build a table that decodes both magnitude and value of small ACs in
            // one go.
            let mut fast_ac = [0; 1 << HUFF_LOOKAHEAD];

            for i in 0..(1 << HUFF_LOOKAHEAD) {
                let fast = fast[i];
                if fast < 255 {
                    let rs = self.values[fast as usize];
                    let run = i16::from((rs >> 4) & 15);
                    let mag_bits = i16::from(rs & 15);
                    let len = i16::from(size[fast as usize]);
                    if mag_bits != 0 && len + mag_bits <= i16::from(HUFF_LOOKAHEAD) {
                        // magnitude code followed by receive_extend code
                        let mut k = (((i as i16) << len) & ((1 << HUFF_LOOKAHEAD) - 1))
                            >> (i16::from(HUFF_LOOKAHEAD) - mag_bits);
                        let m = 1 << (mag_bits - 1);
                        if k < m {
                            k += (!0_i16 << mag_bits) + 1;
                        };
                        // if result is small enough fit into fast ac table
                        if k >= -128 && k <= 127 {
                            fast_ac[i] = (k << 8) + (run << 4) + (len + mag_bits);
                        }
                    }
                }
            }

            self.ac_lookup = Some(fast_ac);
        }

        // Validate symbols as being reasonable
        // For AC tables, we make no check, but accept all byte values 0..255
        // For DC tables, we require symbols to be in range 0..15
        if is_dc {
            for i in 0..num_symbols {
                let sym = self.values[i];
                if sym > 15 {
                    return Err(DecodeErrors::HuffmanDecode("Bad Huffman Table".to_string()));
                }
            }
        }
        Ok(())
    }
}
