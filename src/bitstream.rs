#![allow(
    clippy::if_not_else,
    clippy::similar_names,
    clippy::inline_always,
    clippy::doc_markdown,
    clippy::cast_sign_loss,
    clippy::cast_possible_truncation
)]

//! This file exposes a single struct that can decode a huffman encoded
//! Bitstream in a JPEG file
//!
//! This code is optimized for speed.
//! It's meant to be super duper super fast, because everyone else depends on this being fast.
//! It's (annoyingly) serial hence we cant use parallel bitstreams(it's variable length coding.)
//!
//! Furthermore, on the case of refills, we have to do bytewise processing because the standard decided
//! that we want to support markers in the middle of streams(seriously few people use RST markers).
//!
//! So we pull in all optimization steps:
//! - use `inline[always]`? ✅ ,
//! - pre-execute most common cases ✅,
//! - add random comments ✅
//! -  fast paths ✅.
//!
//! Speed-wise: It is probably the fastest JPEG BitStream decoder to ever sail the seven seas because of
//! a couple of optimization tricks.
//! 1. Fast refills from libjpeg-turbo
//! 2. As few as possible branches in decoder fast paths.
//! 3. Accelerated AC table decoding borrowed from stb_image.h written by Fabian Gissen (@ rygorous),
//! improved by me to handle more cases.
//! 4. Safe and extensible routines(e.g. cool ways to eliminate bounds check)
//! 5. No unsafe here
//!
//! Readability comes as a second priority(I tried with variable names this time, and we are wayy better than libjpeg).
//!
//! Anyway if you are reading this it means your cool and I hope you get whatever part of the code you are looking for
//! (or learn something cool)
//!
//! Knock yourself out.
use std::cmp::min;
use std::io::Cursor;

use crate::errors::DecodeErrors;
use crate::huffman::{HuffmanTable, HUFF_LOOKAHEAD};
use crate::marker::Marker;
use crate::misc::UN_ZIGZAG;

macro_rules! decode_huff {
    ($stream:tt,$symbol:tt,$table:tt) => {
        let mut code_length = $symbol >> HUFF_LOOKAHEAD;

        ($symbol) &= (1 << HUFF_LOOKAHEAD) - 1;

        if code_length > i32::from(HUFF_LOOKAHEAD)
        {
            // if the symbol cannot be resolved in the first HUFF_LOOKAHEAD bits,
            // we know it lies somewhere between HUFF_LOOKAHEAD and 16 bits since jpeg imposes 16 bit
            // limit, we can therefore look 16 bits ahead and try to resolve the symbol
            // starting from 1+HUFF_LOOKAHEAD bits.
            $symbol = ($stream).peek_bits::<16>() as i32;

            // (Credits to Sean T. Barrett stb library for this optimization)
            // maxcode is pre-shifted 16 bytes long so that it has (16-code_length)
            // zeroes at the end hence we do not need to shift in the inner loop.
            while code_length < 17{
                if $symbol < $table.maxcode[code_length as usize]  {
                    break;
                }
                code_length += 1;
            }

            if code_length == 17{
                // symbol could not be decoded.
                //
                // We may think, lets fake zeroes, noo
                // panic, because Huffman codes are sensitive, probably everything
                // after this will be corrupt, so no need to continue.
                return Err(DecodeErrors::HuffmanDecode(format!("Bad Huffman Code 0x{:X}, corrupt JPEG",$symbol)))
            }

            $symbol >>= (16-code_length);
            ($symbol) = i32::from(
                ($table).values
                    [(($symbol + ($table).offset[code_length as usize]) & 0xFF) as usize],
            );
        }
        // drop bits read
        ($stream).drop_bits(code_length as u8);
    };
}

/// A `BitStream` struct, a bit by bit reader with super powers
///
pub(crate) struct BitStream
{
    /// A MSB type buffer that is used for some certain operations
    pub buffer: u64,
    /// A TOP  aligned MSB type buffer that is used to accelerate some operations like
    /// peek_bits and get_bits.
    ///
    /// By top aligned, I mean the top bit (63) represents the top bit in the buffer.
    aligned_buffer: u64,
    /// Tell us the bits left the two buffer
    pub(crate) bits_left: u8,
    /// Did we find a marker(RST/EOF) during decoding?
    pub marker: Option<Marker>,

    /// Progressive decoding
    pub successive_high: u8,
    pub successive_low: u8,
    spec_start: u8,
    spec_end: u8,
    pub eob_run: i32,
}

impl BitStream
{
    /// Create a new BitStream
    pub(crate) const fn new() -> BitStream
    {
        BitStream {
            buffer: 0,
            aligned_buffer: 0,
            bits_left: 0,
            marker: None,
            successive_high: 0,
            successive_low: 0,
            spec_start: 0,
            spec_end: 0,
            eob_run: 0,
        }
    }

    /// Create a new Bitstream for progressive decoding

    pub(crate) fn new_progressive(ah: u8, al: u8, spec_start: u8, spec_end: u8) -> BitStream
    {
        BitStream {
            buffer: 0,
            aligned_buffer: 0,
            bits_left: 0,
            marker: None,
            successive_high: ah,
            successive_low: al,
            spec_start,
            spec_end,
            eob_run: 0,
        }
    }

    /// Refill the bit buffer by (a maximum of) 32 bits
    ///
    /// # Arguments
    ///  - `reader`:`&mut BufReader<R>`: A mutable reference to an underlying
    ///    File/Memory buffer containing a valid JPEG stream
    ///
    /// This function will only refill if `self.count` is less than 32
    #[inline(never)] // to many call sites?
    fn refill(&mut self, reader: &mut Cursor<Vec<u8>>) -> Result<bool, DecodeErrors>
    {
        /// Macro version of a single byte refill.
        /// Arguments
        /// buffer-> our io buffer, because rust macros cannot get values from
        /// the surrounding environment bits_left-> number of bits left
        /// to full refill
        macro_rules! refill {
            ($buffer:expr,$byte:expr,$bits_left:expr) => {
                // read a byte from the stream
                $byte = read_u8(reader);

                // append to the buffer
                // JPEG is a MSB type buffer so that means we append this
                // to the lower end (0..8) of the buffer and push the rest bits above..
                $buffer = ($buffer << 8) | $byte;

                // Increment bits left
                $bits_left += 8;

                // Check for special case  of OxFF, to see if it's a stream or a marker
                if $byte == 0xff
                {
                    // read next byte
                    let mut next_byte = read_u8(reader);

                    // Byte snuffing, if we encounter byte snuff, we skip the byte
                    if next_byte != 0x00
                    {
                        // skip that byte we read
                        while next_byte == 0xFF
                        {
                            next_byte = read_u8(reader);
                        }

                        if next_byte != 0x00
                        {
                            // Undo the byte append and return
                            self.buffer >>= 8;

                            $bits_left -= 8;
                            if $bits_left != 0
                            {
                                self.aligned_buffer = $buffer << (64 - $bits_left);
                            }
                            self.marker =
                                Some(Marker::from_u8(next_byte as u8).ok_or_else(|| {
                                    DecodeErrors::Format(format!(
                                        "Unknown marker 0xFF{:X}",
                                        next_byte
                                    ))
                                })?);
                            return Ok(false);
                        }
                    }
                }
            };
        }

        // 32 bits is enough for a decode(16 bits) and receive_extend(max 16 bits)
        // If we have less than 32 bits we refill
        if self.bits_left <= 32 && self.marker.is_none()
        {
            // So before we do anything, check if we have a 0xFF byte

            if ((reader.position() + 4) as usize) < (reader.get_ref().len())
            {
                let pos = reader.position() as usize;
                // we have 4 bytes to spare, read the 4 bytes into a temporary buffer
                let mut buf = [0; 4];
                buf.copy_from_slice(reader.get_ref().get(pos..pos + 4).unwrap());
                // create buffer
                let msb_buf = u32::from_be_bytes(buf);
                // check if we have 0xff
                if !has_byte(msb_buf, 255)
                {
                    // Move cursor 4 bytes ahead.
                    reader.set_position((pos + 4) as u64);
                    // indicate we have 32 bits incoming
                    self.bits_left += 32;
                    // make room
                    self.buffer <<= 32;
                    // add
                    self.buffer |= u64::from(msb_buf);
                    // set them correctly
                    self.aligned_buffer = self.buffer << (64 - self.bits_left);
                    // done.
                    return Ok(true);
                }
            }

            // This serves two reasons,
            // 1: Make clippy shut up
            // 2: Favour register reuse
            let mut byte;

            // 4 refills, if all succeed the stream should contain enough bits to decode a
            // value
            refill!(self.buffer, byte, self.bits_left);

            refill!(self.buffer, byte, self.bits_left);

            refill!(self.buffer, byte, self.bits_left);

            refill!(self.buffer, byte, self.bits_left);

            // Construct an MSB buffer whose top bits are the bitstream we are currently
            // holding.
            self.aligned_buffer = self.buffer << (64 - self.bits_left);
        }
        else if self.marker.is_some()
        {
            // fill with zeroes
            self.bits_left = 63;
        }

        return Ok(true);
    }
    /// Decode the DC coefficient in a MCU block.
    ///
    /// The decoded coefficient is written to `dc_prediction`
    ///
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::unwrap_used
    )]
    #[inline(always)]
    fn decode_dc(
        &mut self, reader: &mut Cursor<Vec<u8>>, dc_table: &HuffmanTable, dc_prediction: &mut i32,
    ) -> Result<bool, DecodeErrors>
    {
        let (mut symbol, r);

        if self.bits_left < 16
        {
            self.refill(reader)?;
        };
        // look a head HUFF_LOOKAHEAD bits into the bitstream
        symbol = self.peek_bits::<HUFF_LOOKAHEAD>();

        symbol = dc_table.lookup[symbol as usize];

        decode_huff!(self, symbol, dc_table);

        if symbol != 0
        {
            r = self.get_bits(symbol as u8);

            symbol = huff_extend(r, symbol);
        }
        // Update DC prediction
        *dc_prediction = dc_prediction.wrapping_add(symbol);

        return Ok(true);
    }

    /// Decode a Minimum Code Unit(MCU) as quickly as possible
    ///
    /// # Arguments
    /// - reader: The bitstream from where we read more bits.
    /// - dc_table: The Huffman table used to decode the DC coefficient
    /// - ac_table: The Huffman table used to decode AC values
    /// - block: A memory region where we will write out the decoded values
    /// - DC prediction: Last DC value for this component
    ///
    #[allow(
    clippy::many_single_char_names,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
    )]
    #[rustfmt::skip]
    #[inline(always)]
    pub fn decode_mcu_block(
        &mut self,
        reader: &mut Cursor<Vec<u8>>,
        dc_table: &HuffmanTable,
        ac_table: &HuffmanTable,
        block: &mut [i16; 64],
        dc_prediction: &mut i32,
    ) -> Result<(), DecodeErrors>
    {
        // decode DC, dc prediction will contain the value
        self.decode_dc(reader, dc_table, dc_prediction)?;

        // set dc to be the dc prediction.
        block[0] = *dc_prediction as i16;

        let (mut symbol, mut r);
        // Decode AC coefficients
        let mut pos: usize = 1;
        // Get fast AC table as a reference before we enter the hot path
        let ac_lookup = ac_table.ac_lookup.as_ref().unwrap();

        while pos < 64
        {
            self.refill(reader)?;

            symbol = self.peek_bits::<HUFF_LOOKAHEAD>();

            let fast_ac = ac_lookup[symbol as usize];

            symbol = ac_table.lookup[symbol as usize];

            if fast_ac != 0
            {
                //  FAST AC path

                // run
                pos += ((fast_ac >> 4) & 63) as usize;

                // Value

                // The `& 63` is to remove a  branch, i.e keep it between 0 and 63 because Rust can't
                // see that un-zig-zag returns values less than 63
                // See https://godbolt.org/z/zrbe6qcPf
                block[UN_ZIGZAG[min(pos, 63)] & 63] = fast_ac >> 10;

                // combined length
                self.drop_bits((fast_ac & 15) as u8);

                pos += 1;
            } else {
                decode_huff!(self,symbol,ac_table);

                r = symbol >> 4;

                symbol &= 15;

                if symbol != 0
                {
                    pos += r as usize;

                    r = self.get_bits(symbol as u8);

                    symbol = huff_extend(r, symbol);

                    block[UN_ZIGZAG[pos as usize & 63] & 63] = symbol as i16;

                    pos += 1;
                } else {
                    if r != 15
                    {
                        return Ok(());
                    }
                    pos += 16;
                }
            }
        }
        return Ok(());
    }

    /// Peek `look_ahead` bits ahead without discarding them from the buffer
    #[inline(always)]
    #[allow(clippy::cast_possible_truncation)]
    const fn peek_bits<const LOOKAHEAD: u8>(&self) -> i32
    {
        (self.aligned_buffer >> (64 - LOOKAHEAD)) as i32
    }

    /// Discard the next `N` bits without checking
    #[inline]
    fn drop_bits(&mut self, n: u8)
    {
        // prevent under flowing subtraction.
        // The best situation should be panicking out but that has
        // a performance impact
        self.bits_left = self.bits_left.saturating_sub(n);

        // remove top n bits  in lsb buffer
        self.aligned_buffer <<= n;
    }

    /// Read `n_bits` from the buffer  and discard them
    #[inline(always)]
    #[allow(clippy::cast_possible_truncation)]
    fn get_bits(&mut self, n_bits: u8) -> i32
    {
        let mask = (1_u64 << n_bits) - 1;
        // Place the needed bits in the lower part of our bit-buffer
        // using rotate instructions
        self.aligned_buffer = self.aligned_buffer.rotate_left(u32::from(n_bits));
        // Mask lower bits
        let bits = (self.aligned_buffer & mask) as i32;

        // Reduce the bits left, this influences the MSB buffer
        self.bits_left = self.bits_left.saturating_sub(n_bits);

        // shift out bits read in the LSB buffer
        bits
    }

    /// Decode a DC block
    #[allow(clippy::cast_possible_truncation)]
    #[inline]
    pub(crate) fn decode_prog_dc_first(
        &mut self, reader: &mut Cursor<Vec<u8>>, dc_table: &HuffmanTable, block: &mut i16,
        dc_prediction: &mut i32,
    ) -> Result<(), DecodeErrors>
    {
        self.decode_dc(reader, dc_table, dc_prediction)?;

        *block = (*dc_prediction as i16).wrapping_mul(1_i16 << self.successive_low);

        return Ok(());
    }
    #[inline]
    pub(crate) fn decode_prog_dc_refine(
        &mut self, reader: &mut Cursor<Vec<u8>>, block: &mut i16,
    ) -> Result<(), DecodeErrors>
    {
        // refinement scan
        if self.bits_left < 1
        {
            self.refill(reader)?;
        }
        if self.get_bit() == 1
        {
            *block += 1 << self.successive_low;
        }
        Ok(())
    }

    /// Get a single bit from the bitstream
    fn get_bit(&mut self) -> u8
    {
        let k = (self.aligned_buffer >> 63) as u8;

        // discard a bit
        self.drop_bits(1);

        return k;
    }
    pub(crate) fn decode_mcu_ac_first(
        &mut self, reader: &mut Cursor<Vec<u8>>, ac_table: &HuffmanTable, block: &mut [i16; 64],
    ) -> Result<bool, DecodeErrors>
    {
        let shift = self.successive_low;
        // EOB runs are handled in mcu_prog.rs
        // see the comment there

        let mut k = self.spec_start as usize;
        // same as the AC part for decode block , with a twist
        let fast_ac = ac_table.ac_lookup.as_ref().unwrap();
        // emulate a do while loop
        'block: loop
        {
            // don't check what refill returns,
            // but then we have to put refills in a lot of placed
            // because of this
            self.refill(reader)?;

            let (mut symbol, mut r);
            symbol = self.peek_bits::<HUFF_LOOKAHEAD>();

            let fac = fast_ac[symbol as usize];

            symbol = ac_table.lookup[symbol as usize];

            if fac != 0
            {
                // fast ac path

                // run
                k += ((fac >> 4) & 63) as usize;
                // value
                block[UN_ZIGZAG[min(k, 63)] & 63] = (fac >> 10) * (1 << shift);

                self.drop_bits((fac & 15) as u8);
                k += 1;
            }
            else
            {
                decode_huff!(self, symbol, ac_table);

                r = symbol >> 4;

                symbol &= 15;

                if symbol != 0
                {
                    k += r as usize;

                    r = self.get_bits(symbol as u8);

                    symbol = huff_extend(r, symbol);

                    block[UN_ZIGZAG[k as usize & 63] & 63] = symbol as i16 * (1 << shift);

                    k += 1;
                }
                else
                {
                    if r != 15
                    {
                        self.eob_run = 1 << r;

                        // we refilled earlier, hence we can assume we have enough bits
                        // for this.
                        self.eob_run += self.get_bits(r as u8);

                        self.eob_run -= 1;

                        break;
                    }
                    k += 16;
                }
            }

            if k > self.spec_end as usize
            {
                break 'block;
            }
        }
        return Ok(true);
    }
    pub(crate) fn decode_mcu_ac_refine(
        &mut self, reader: &mut Cursor<Vec<u8>>, table: &HuffmanTable, block: &mut [i16; 64],
    ) -> Result<bool, DecodeErrors>
    {
        let bit = (1 << self.successive_low) as i16;

        let mut k = self.spec_start;

        if self.eob_run == 0
        {
            'no_eob: loop
            {
                // Decode a coefficient from the bit stream
                self.refill(reader)?;

                let mut symbol = self.peek_bits::<HUFF_LOOKAHEAD>();

                symbol = table.lookup[symbol as usize];

                decode_huff!(self, symbol, table);

                let mut r = symbol >> 4;

                symbol &= 15;

                if symbol == 0
                {
                    if r != 15
                    {
                        // EOB run is 2^r + bits
                        self.eob_run = 1 << r;

                        self.eob_run += self.get_bits(r as u8);
                        // EOB runs are handled by the eob logic
                        break 'no_eob;
                    }
                }
                else
                {
                    if symbol != 1
                    {
                        return Err(DecodeErrors::HuffmanDecode(
                            "Bad Huffman code, corrupt JPEG?".to_string(),
                        ));
                    }
                    // get sign bit
                    // We assume we have enough bits, which should be correct for sane images
                    // since we refill by 32 above
                    if self.get_bit() == 1
                    {
                        // new non-zero coefficient is positive
                        symbol = i32::from(bit);
                    }
                    else
                    {
                        // the new non zero coefficient is negative
                        symbol = i32::from(-bit);
                    }
                }

                // Advance over already nonzero coefficients  appending
                // correction bits to the non-zeroes.
                // A correction bit is 1 if the absolute value of the coefficient must be increased

                'advance_nonzero: while k <= self.spec_end
                {
                    let coefficient = &mut block[UN_ZIGZAG[k as usize & 63] & 63];

                    if *coefficient != 0
                    {
                        if self.get_bit() == 1 && (*coefficient & bit as i16) == 0
                        {
                            if *coefficient >= 0
                            {
                                *coefficient += bit;
                            }
                            else
                            {
                                *coefficient -= bit;
                            }
                        }
                        if self.bits_left < 1
                        {
                            self.refill(reader)?;
                        }
                    }
                    else
                    {
                        r -= 1;

                        if r < 0
                        {
                            // reached target zero coefficient.
                            break 'advance_nonzero;
                        }
                    };
                    k += 1;
                }
                if symbol != 0
                {
                    let pos = UN_ZIGZAG[k as usize & 63];
                    // output new non-zero coefficient.
                    block[pos & 63] = symbol as i16;
                }

                k += 1;

                if k > self.spec_end
                {
                    break 'no_eob;
                }
            }
        }
        if self.eob_run > 0
        {
            // only run if block does not consists of purely zeroes
            if &block[1..] != &[0; 63]
            {
                self.refill(reader)?;

                while k <= self.spec_end
                {
                    let coefficient = &mut block[UN_ZIGZAG[k as usize & 63] & 63];

                    if *coefficient != 0 && self.get_bit() == 1
                    {
                        // check if we already modified it, if so do nothing, otherwise
                        // append the correction bit.
                        if (*coefficient & bit) == 0
                        {
                            if *coefficient >= 0
                            {
                                *coefficient += bit;
                            }
                            else
                            {
                                *coefficient -= bit;
                            }
                        }
                    }
                    if self.bits_left < 1
                    {
                        // refill at the last possible moment
                        self.refill(reader)?;
                    }
                    k += 1;
                }
            }
            // count a block completed in EOB run
            self.eob_run -= 1;
        }
        return Ok(true);
    }

    pub fn update_progressive_params(&mut self, ah: u8, al: u8, spec_start: u8, spec_end: u8)
    {
        self.successive_high = ah;
        self.successive_low = al;

        self.spec_start = spec_start;
        self.spec_end = spec_end;
    }

    /// Reset the stream if we have a restart marker
    ///
    /// Restart markers indicate drop those bits in the stream and zero out
    /// everything
    #[cold]
    pub fn reset(&mut self)
    {
        self.bits_left = 0;

        self.marker = None;

        self.buffer = 0;

        self.aligned_buffer = 0;

        self.eob_run = 0;
    }
}

/// Do the equivalent of JPEG HUFF_EXTEND
#[inline(always)]
fn huff_extend(x: i32, s: i32) -> i32
{
    // if x<s return x else return x+offset[s] where offset[s] = ( (-1<<s)+1)

    (x) + ((((x) - (1 << ((s) - 1))) >> 31) & (((-1) << (s)) + 1))
}

/// Read a byte from underlying file
///
/// Function is inlined (as always)
#[inline(always)]
#[allow(clippy::cast_possible_truncation)]
fn read_u8(reader: &mut Cursor<Vec<u8>>) -> u64
{
    let pos = reader.position();

    reader.set_position(pos + 1);
    // if we have nothing left fill buffer with zeroes
    u64::from(*reader.get_ref().get(pos as usize).unwrap_or(&0))
}

fn has_zero(v: u32) -> bool
{
    // Retrieved from Stanford bithacks
    // @ https://graphics.stanford.edu/~seander/bithacks.html#ZeroInWord
    return !((((v & 0x7F7F_7F7F) + 0x7F7F_7F7F) | v) | 0x7F7F_7F7F) != 0;
}
fn has_byte(b: u32, val: u8) -> bool
{
    // Retrieved from Stanford bithacks
    // @ https://graphics.stanford.edu/~seander/bithacks.html#ZeroInWord
    has_zero(b ^ ((!0_u32 / 255) * u32::from(val)))
}
