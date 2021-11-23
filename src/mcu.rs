//! Implements routines to decode a MCU
//!
//! # Side notes
//! Yes, I pull in some dubious tricks, like really dubious here, they're not hard to come up
//! but I know they're hard to understand(e.g how I don't allocate space for Cb and Cr
//! channels if output colorspace is grayscale) but bear with me, it's the search for fast software
//! that got me here.
//!
//! # Multithreading
//!
//!This isn't exposed so I can dump all the info here
//!
//! To make multithreading work, we want to break dependency chains but in cool ways.
//! i.e we want to find out where we can forward one section as another one does something.
//!
//! # The algorithm
//!  Simply do it per MCU width taking into account sub-sampling ratios
//!
//! 1. Decode an MCU width taking into account how many image channels we have(either Y only or Y,Cb and Cr)
//!
//! 2. After successfully decoding, copy pixels decoded and spawn a thread to handle post processing(IDCT,
//! upsampling and color conversion)
//!
//! 3. After successfully decoding all pixels, join threads.
//!
//! 4. Call it a day,
//!
//!But as easy as this sounds in theory, in practice, it sucks...
//!
//! We essentially have to consider that down-sampled images have weird MCU arrangement and for such cases
//! ! choose the path of decoding 2 whole MCU heights for horizontal/vertical upsampling and
//! 4 whole MCU heights for horizontal and vertical upsampling, which when expressed in code doesn't look nice.
//!
//! There is also the overhead of synchronization which makes some things annoying.
//!
//! Also there is the overhead of `cloning` and allocating intermediate memory to ensure multithreading is safe.
//! This may make this library almost 3X slower if someone chooses to disable `threadpool` (please don't) feature because
//! we are optimized for the multithreading path.
//!
//! # Scoped ThreadPools
//! Things you don't want to do in the fast path. **Lock da mutex**
//! Things you don't want to have in your code. **Mutex**
//!
//! Multithreading is not everyone's cake because synchronization is like battling with the devil
//! The default way is a mutex for when threads may write to the same memory location. But in our case we
//! don't write to the same, location, so why pay for something not used.
//!
//! In C/C++ land we can just pass mutable chunks to different threads but in Rust don't you know about
//! the borrow checker?...
//!
//! To send different mutable chunks  to threads, we use scoped threads which guarantee that the thread
//! won't outlive the data and finally let it compile.
//! This allows us to not use locks during decoding avoiding that overhead. and allowing more cleaner
//! faster code in post processing..

use std::cmp::min;
use std::io::Cursor;
use std::sync::Arc;

use crate::bitstream::BitStream;
use crate::components::{ComponentID, SubSampRatios};
use crate::errors::DecodeErrors;
use crate::marker::Marker;
use crate::worker::post_process;
use crate::Decoder;

/// The size of a DC block for a MCU.

pub const DCT_BLOCK: usize = 64;

impl Decoder
{
    /// Check for existence of DC and AC Huffman Tables
    fn check_tables(&self) -> Result<(), DecodeErrors>
    {
        // check that dc and AC tables exist outside the hot path
        for i in 0..self.input_colorspace.num_components()
        {
            let _ = &self
                .dc_huffman_tables
                .get(self.components[i].dc_huff_table)
                .as_ref()
                .ok_or_else(|| {
                    DecodeErrors::HuffmanDecode(format!(
                        "No Huffman DC table for component {:?} ",
                        self.components[i].component_id
                    ))
                })?
                .as_ref()
                .ok_or_else(|| {
                    DecodeErrors::HuffmanDecode(format!(
                        "No DC table for component {:?}",
                        self.components[i].component_id
                    ))
                })?;

            let _ = &self
                .ac_huffman_tables
                .get(self.components[i].ac_huff_table)
                .as_ref()
                .ok_or_else(|| {
                    DecodeErrors::HuffmanDecode(format!(
                        "No Huffman AC table for component {:?} ",
                        self.components[i].component_id
                    ))
                })?
                .as_ref()
                .ok_or_else(|| {
                    DecodeErrors::HuffmanDecode(format!(
                        "No AC table for component {:?}",
                        self.components[i].component_id
                    ))
                })?;
        }
        Ok(())
    }

    /// Decode MCUs and carry out post processing.
    ///
    /// This is the main decoder loop for the library, the hot path.
    ///
    /// Because of this, we pull in some very crazy optimization tricks hence readability is a pinch
    /// here.
    #[allow(clippy::similar_names)]
    #[inline(never)]
    #[rustfmt::skip]
    pub(crate) fn decode_mcu_ycbcr_baseline(
        &mut self, reader: &mut Cursor<Vec<u8>>,
    ) -> Result<Vec<u8>, DecodeErrors>
    {
        let mut scoped_pools = scoped_threadpool::Pool::new(num_cpus::get() as u32);
        info!("Created {} worker threads", scoped_pools.thread_count());

        let (mcu_width, mcu_height);
        let mut bias = 1;

        if self.interleaved
        {
            // set upsampling functions
            self.set_upsampling()?;

            if self.sub_sample_ratio == SubSampRatios::H
            {
                // horizontal sub-sampling.

                // Values for horizontal samples end halfway the image and do not complete an MCU width.
                // To make it complete we multiply width by 2 and divide mcu_height by 2
                mcu_width = self.mcu_x * 2;

                mcu_height = self.mcu_y / 2;
            } else if self.sub_sample_ratio == SubSampRatios::HV
            {
                mcu_width = self.mcu_x;

                mcu_height = self.mcu_y / 2;
                bias = 2;
                // V;
            } else {
                mcu_width = self.mcu_x;

                mcu_height = self.mcu_y;
            }
        } else {
            // For non-interleaved images( (1*1) subsampling)
            // number of MCU's are the widths (+7 to account for paddings) divided bu 8.
            mcu_width = ((self.info.width + 7) / 8) as usize;

            mcu_height = ((self.info.height + 7) / 8) as usize;
        }

        let mut stream = BitStream::new();
        // Size of our output image(width*height)
        let capacity = usize::from(self.info.width + 7) * usize::from(self.info.height + 7);

        let component_capacity = mcu_width * DCT_BLOCK;
        // for those pointers storing unprocessed items, zero them out here
        for (pos, comp) in self.components.iter().enumerate()
        {
            // multiply capacity with sampling factor, it  should be 1*1 for un-sampled images

            //NOTE: We only allocate a block if we need it, so e.g for grayscale
            // we don't allocate for CB and Cr channels
            if min(self.output_colorspace.num_components() - 1, pos) == pos
            {
                let len = component_capacity * comp.vertical_sample * comp.horizontal_sample * bias;
                // For 4:2:0 upsampling we need to do some tweaks, reason explained in bias

                self.mcu_block[pos] = vec![0; len];
            }
        }

        // Create an Arc of components to prevent cloning on every MCU width
        let global_component = Arc::new(self.components.clone());

        // Storage for decoded pixels
        let mut global_channel = vec![0; capacity * self.output_colorspace.num_components()];

        // things needed for post processing that we can remove out of the loop
        let input = self.input_colorspace;

        let output = self.output_colorspace;

        let idct_func = self.idct_func;

        let color_convert = self.color_convert;

        let color_convert_16 = self.color_convert_16;

        let width = usize::from(self.width());

        let h_max = self.h_max;

        let v_max = self.v_max;
        // Halfway width size, used for vertical sub-sampling to write |Y2| in the right position.
        let width_stride = (self.mcu_block[0].len()) >> 1;

        let hv_width_stride = (self.mcu_block[0].len()) >> 2;
        // check dc and AC tables
        self.check_tables()?;

        let is_hv = self.sub_sample_ratio == SubSampRatios::HV;

        // Split output into different blocks each containing enough space for an MCU width
        let mut chunks =
            global_channel.chunks_exact_mut(width * output.num_components() * 8 * h_max * v_max);

        // Argument for scoped threadpools, see file docs.
        scoped_pools.scoped::<_, Result<(), DecodeErrors>>(|scope| {
            for _ in 0..mcu_height
            {
                // Bias only affects 4:2:0(chroma quartered) sub-sampled images. So let me explain
                for v in 0..bias
                {
                    // Ideally this should be one loop but I'm parallelizing per MCU width boys
                    for j in 0..mcu_width
                    {
                         // iterate over components

                        'rst: for pos in 0..self.input_colorspace.num_components()
                        {
                            let component = &mut self.components[pos];
                            // Safety:The tables were confirmed to exist in self.check_tables();
                            // Reason.
                            // - These were 4 branch checks per component, for a 1080 * 1080 *3 component image
                            //   that becomes(1080*1080*3)/(16)-> 218700 branches in the hot path. And I'm not
                            //   paying that penalty
                            let dc_table = unsafe {
                                self.dc_huffman_tables
                                    .get_unchecked(component.dc_huff_table)
                                    .as_ref()
                                    .unwrap_or_else(|| std::hint::unreachable_unchecked())
                            };
                            let ac_table = unsafe {
                                self.ac_huffman_tables
                                    .get_unchecked(component.ac_huff_table)
                                    .as_ref()
                                    .unwrap_or_else(|| std::hint::unreachable_unchecked())
                            };
                            // If image is interleaved iterate over scan  components,
                            // otherwise if it-s non-interleaved, these routines iterate in
                            // trivial scanline order(Y,Cb,Cr)
                            for v_samp in 0..component.vertical_sample
                            {
                                for h_samp in 0..component.horizontal_sample
                                {
                                    let mut tmp = [0; DCT_BLOCK];
                                    stream.decode_mcu_block(reader, dc_table, ac_table, &mut tmp, &mut component.dc_pred)?;

                                    // Store only needed components (i.e for YCbCr->Grayscale don't store Cb and Cr channels)
                                    // improves speed when we do a clone(less items to clone)
                                    if min(self.output_colorspace.num_components() - 1, pos) == pos
                                    {

                                        // The spec  https://www.w3.org/Graphics/JPEG/itu-t81.pdf page 26

                                        let is_y =
                                            usize::from(component.component_id == ComponentID::Y);

                                        // This only affects 4:2:0 images.
                                        let y_offset = is_y
                                            * v
                                            * (hv_width_stride
                                            + (hv_width_stride * (component.vertical_sample - 1)));

                                        let another_stride =
                                            (width_stride * v_samp * usize::from(!is_hv))
                                                + hv_width_stride * v_samp * usize::from(is_hv);

                                        let yet_another_stride = usize::from(is_hv)
                                            * (width_stride >> 2)
                                            * v
                                            * usize::from(component.component_id != ComponentID::Y);

                                        // offset calculator.
                                        let start = (j * 64 * component.horizontal_sample)
                                            + (h_samp * 64)
                                            + another_stride
                                            + y_offset
                                            + yet_another_stride;

                                        self.mcu_block[pos][start..start + 64].copy_from_slice(&tmp);

                                    }
                                }
                            }
                            self.todo -= 1;
                            // after every interleaved MCU that's a mcu, count down restart markers.
                            if self.todo == 0 {
                                self.todo = self.restart_interval;

                                // decode the MCU
                                if let Some(marker) = stream.marker
                                {   // Found a marker
                                    // Read stream and see what marker is stored there
                                    match marker
                                    {
                                        Marker::RST(_) =>
                                            {
                                                // reset stream
                                                stream.reset();
                                                // Initialize dc predictions to zero for all components
                                                self.components.iter_mut().for_each(|x| x.dc_pred = 0);
                                                // Start iterating again. from position.
                                                break 'rst;
                                            }
                                        Marker::EOI =>
                                            {
                                                // silent pass
                                            }
                                        _ =>
                                            {
                                                return Err(DecodeErrors::MCUError(format!(
                                                    "Marker {:?} found in bitstream, possibly corrupt jpeg",
                                                    marker
                                                )));
                                            }
                                    }
                                }
                            }
                        }
                    }
                }
                // Clone things, to make multithreading safe
                let component = global_component.clone();

                let mut block = self.mcu_block.clone();

                let next_chunk = chunks.next().unwrap();

                scope.execute(move || {
                    post_process(&mut block, &component,
                                 idct_func, color_convert_16, color_convert,
                                 input, output, next_chunk,
                                 mcu_width, width);
                });
            }
            //everything is okay
            Ok(())
        })?;
        info!("Finished decoding image");
        // remove excess allocation for images.
        global_channel.truncate(
            usize::from(self.width())
                * usize::from(self.height())
                * self.output_colorspace.num_components(),
        );
        return Ok(global_channel);
    }
}
