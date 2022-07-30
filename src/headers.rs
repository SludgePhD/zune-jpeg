//! Decode Decoder markers/segments
//!
//! This file deals with decoding header information in a jpeg file
//!

use std::cmp::max;
use std::io::{BufRead, Read};

use crate::components::Components;
use crate::decoder::{Decoder, ImageInfo, MAX_COMPONENTS};
use crate::errors::DecodeErrors;
use crate::huffman::HuffmanTable;
use crate::marker::Marker;
use crate::misc::{read_byte, read_u16_be, Aligned32, ColorSpace, SOFMarkers, UN_ZIGZAG};

///**B.2.4.2 Huffman table-specification syntax**
#[allow(clippy::similar_names)]
pub(crate) fn parse_huffman<R>(decoder: &mut Decoder, mut buf: &mut R) -> Result<(), DecodeErrors>
    where
        R: Read,
{
    // Read the length of the Huffman table
    let mut dht_length = read_u16_be(&mut buf)
        .map_err(|_| {
            DecodeErrors::HuffmanDecode("Could not read Huffman length from image".to_string())
        })?
        .checked_sub(2)
        .ok_or(DecodeErrors::HuffmanDecode(
            "Invalid Huffman length in image".to_string(),
        ))? as i32;

    while dht_length > 16
    {
        // HT information
        let ht_info = read_byte(&mut buf)?;

        // third bit indicates whether the huffman encoding is DC or AC type
        let dc_or_ac = (ht_info >> 4) & 0xF;

        // Indicate the position of this table, should be less than 4;
        let index = (ht_info & 0xF) as usize;

        if index >= MAX_COMPONENTS
        {
            return Err(DecodeErrors::HuffmanDecode(format!(
                "Invalid DHT index {}, expected between 0 and 3",
                index
            )));
        }
        if dc_or_ac > 1
        {
            return Err(DecodeErrors::HuffmanDecode(format!(
                "Invalid DHT postioon {}, should be 0 or 1",
                dc_or_ac
            )));
        }
        // read the number of symbols
        let mut num_symbols: [u8; 17] = [0; 17];

        buf.read_exact(&mut num_symbols[1..17]).map_err(|_| {
            DecodeErrors::HuffmanDecode("Could not read bytes into the buffer".to_string())
        })?;

        dht_length -= 1 + 16;

        let symbols_sum: i32 = num_symbols.iter().map(|f| i32::from(*f)).sum();

        // The sum of the number of symbols cannot be greater than 256;
        if symbols_sum > 256
        {
            return Err(DecodeErrors::HuffmanDecode(
                "Encountered Huffman table with excessive length in DHT".to_string(),
            ));
        }
        if symbols_sum > dht_length
        {
            return Err(DecodeErrors::HuffmanDecode(format!(
                "Excessive Huffman table of length {} found when header length is {}",
                symbols_sum, dht_length
            )));
        }
        dht_length -= symbols_sum;

        // A table containing symbols in increasing code length
        let mut symbols = [0; 256];

        buf.read_exact(&mut symbols[0..(symbols_sum as usize)])
            .map_err(|x| {
                DecodeErrors::Format(format!("Could not read symbols into the buffer\n{}", x))
            })?;
        // store
        match dc_or_ac
        {
            0 =>
                {
                    decoder.dc_huffman_tables[index] = Some(HuffmanTable::new(
                        &num_symbols,
                        symbols,
                        true,
                        decoder.is_progressive,
                    )?);
                }
            _ =>
                {
                    decoder.ac_huffman_tables[index] = Some(HuffmanTable::new(
                        &num_symbols,
                        symbols,
                        false,
                        decoder.is_progressive,
                    )?);
                }
        }
    }
    if dht_length > 0
    {
        return Err(DecodeErrors::HuffmanDecode(format!(
            "Bogus Huffman table definition"
        )));
    }
    Ok(())
}

///**B.2.4.1 Quantization table-specification syntax**
#[allow(clippy::cast_possible_truncation)]
pub(crate) fn parse_dqt<R>(decoder: &mut Decoder, buf: &mut R) -> Result<(), DecodeErrors>
    where
        R: Read,
{
    let mut buf = buf;

    // read length
    let mut qt_length = read_u16_be(&mut buf)
        .map_err(|c| DecodeErrors::Format(format!("Could not read  DQT length {}", c)))?
        .checked_sub(2)
        .ok_or(DecodeErrors::DqtError(format!(
            "Invalid DQT length. Length should be greater than 2"
        )))?;
    // A single DQT header may have multiple QT's
    while qt_length > 0
    {
        let qt_info = read_byte(&mut buf)?;

        // 0 = 8 bit otherwise 16 bit dqt
        let precision = (qt_info >> 4) as usize;

        // last 4 bits give us position
        let table_position = (qt_info & 0x0f) as usize;

        let precision_value = 64 * (precision + 1);

        if (precision_value + 1) as u16 > qt_length
        {
            return Err(DecodeErrors::DqtError(format!("Invalid QT table bytes left :{}. Too small to construct a valid qt table which should be {} long", qt_length, precision_value + 1)));
        }

        let dct_table = match precision
        {
            0 =>
                {
                    let mut qt_values = [0; 64];

                    buf.read_exact(&mut qt_values).map_err(|x| {
                        DecodeErrors::Format(format!("Could not read symbols into the buffer\n{}", x))
                    })?;
                    qt_length -= (precision_value as u16) + 1 /*QT BIT*/;
                    // carry out un zig-zag here
                    un_zig_zag(&qt_values)
                }
            1 =>
                {
                    // 16 bit quantization tables
                    //(cae) Before we enable this. Should 16 bit QT cause any other lib changes
                    return Err(DecodeErrors::DqtError(
                        "Support for 16 bit quantization table is not complete".to_string(),
                    ));
                }
            _ =>
                {
                    return Err(DecodeErrors::DqtError(format!(
                        "Expected QT precision value of either 0 or 1, found {:?}",
                        precision
                    )));
                }
        };
        if table_position >= MAX_COMPONENTS
        {
            return Err(DecodeErrors::DqtError(format!(
                "Too large table position for QT :{}, expected between 0 and 3",
                table_position
            )));
        }

        decoder.qt_tables[table_position] = Some(dct_table);
    }

    return Ok(());
}

/// Section:`B.2.2 Frame header syntax`

pub(crate) fn parse_start_of_frame<R>(
    buf: &mut R, sof: SOFMarkers, img: &mut Decoder,
) -> Result<(), DecodeErrors>
    where
        R: Read,
{
    let mut buf = buf;

    // Get length of the frame header
    let length = read_u16_be(&mut buf)
        .map_err(|_| DecodeErrors::Format("Cannot read SOF length, exhausted data".to_string()))?;

    // usually 8, but can be 12 and 16, we currently support only 8
    // so sorry about that 12 bit images
    let dt_precision = read_byte(&mut buf)?;

    if dt_precision != 8
    {
        return Err(DecodeErrors::SofError(format!(
            "The library can only parse 8-bit images, the image has {} bits of precision",
            dt_precision
        )));
    }

    img.info.set_density(dt_precision);

    // read  and set the image height.
    let img_height = read_u16_be(&mut buf).map_err(|_| {
        DecodeErrors::Format("Cannot read image height, exhausted data".to_string())
    })?;

    img.info.set_height(img_height);

    // read and set the image width
    let img_width = read_u16_be(&mut buf)
        .map_err(|_| DecodeErrors::Format("Cannot read image width, exhausted data".to_string()))?;

    img.info.set_width(img_width);

    info!("Image width  :{}", img_width);
    info!("Image height :{}", img_height);

    if img_width > img.max_width {
        return Err(DecodeErrors::Format(format!("Image width {} greater than width limit {}. If use `set_limits` if you want to support huge images", img_width, img.max_width)));
    }

    if img_height > img.max_height {
        return Err(DecodeErrors::Format(format!("Image height {} greater than height limit {}. If use `set_limits` if you want to support huge images", img_height, img.max_height)));
    }

    // Check image width or height is zero
    if img_width == 0 || img_height == 0
    {
        return Err(DecodeErrors::ZeroError);
    }

    // Number of components for the image.
    let num_components = read_byte(&mut buf)?;

    if num_components == 0
    {
        return Err(DecodeErrors::SofError(format!(
            "Number of components cannot be zero."
        )));
    }

    let expected = 8 + 3 * u16::from(num_components);
    // length should be equal to num components
    if length != expected
    {
        return Err(DecodeErrors::SofError(format!(
            "Length of start of frame differs from expected {},value is {}",
            expected, length
        )));
    }
    info!("Image components : {}", num_components);

    if num_components == 1
    {
        // SOF sets the number of image components
        // and that to us translates to setting input and output
        // colorspaces to zero
        img.input_colorspace = ColorSpace::GRAYSCALE;
        img.output_colorspace = ColorSpace::GRAYSCALE;
    }

    // set number of components
    img.info.components = num_components;

    let mut components = Vec::with_capacity(num_components as usize);

    let mut temp = [0; 3];

    for _ in 0..num_components
    {
        // read 3 bytes for each component
        buf.read_exact(&mut temp)
            .map_err(|x| DecodeErrors::Format(format!("Could not read component data\n{}", x)))?;
        // create a component.
        let component = Components::from(temp)?;

        components.push(component);
    }

    img.info.set_sof_marker(sof);

    for component in &mut components
    {
        // compute interleaved image info

        // h_max contains the maximum horizontal component
        img.h_max = max(img.h_max, component.horizontal_sample);

        // v_max contains the maximum vertical component
        img.v_max = max(img.v_max, component.vertical_sample);

        img.mcu_width = img.h_max * 8;

        img.mcu_height = img.v_max * 8;

        // Number of MCU's per width
        img.mcu_x = (usize::from(img.info.width) + img.mcu_width - 1) / img.mcu_width;

        // Number of MCU's per height
        img.mcu_y = (usize::from(img.info.height) + img.mcu_height - 1) / img.mcu_height;
        if img.h_max != 1 || img.v_max != 1
        {
            // interleaved images have horizontal and vertical sampling factors
            // not equal to 1.
            img.interleaved = true;
        }
        // Extract quantization tables from the arrays into components
        let qt_table = *img.qt_tables[component.quantization_table_number as usize]
            .as_ref()
            .ok_or_else(|| {
                DecodeErrors::DqtError(format!(
                    "No quantization table for component {:?}",
                    component.component_id
                ))
            })?;

        component.quantization_table = Aligned32(qt_table);
        // initially stride contains its horizontal sub-sampling
        component.width_stride *= img.mcu_x * 8;
    }

    // delete quantization tables, we'll extract them from the components when
    // needed
    img.qt_tables = [None, None, None, None];

    img.components = components;

    Ok(())
}

/// Parse a start of scan data
pub(crate) fn parse_sos<R>(buf: &mut R, image: &mut Decoder) -> Result<(), DecodeErrors>
    where
        R: Read + BufRead,
{
    let mut buf = buf;

    let mut seen = [false; MAX_COMPONENTS];

    // Scan header length
    let ls = read_u16_be(&mut buf)?;

    // Number of image components in scan
    let ns = read_byte(&mut buf)?;
    image.num_scans = ns;

    if ls != 6 + 2 * u16::from(ns)
    {
        return Err(DecodeErrors::SosError(
            "Bad SOS length,corrupt jpeg".to_string(),
        ));
    }

    // Check number of components.
    // Currently ths library doesn't support images with more than 4 components
    if !(1..4).contains(&ns)
    {
        return Err(DecodeErrors::SosError(format!(
            "Number of components in start of scan should be less than 3 but more than 0. Found {}",
            ns
        )));
    }

    if image.info.components == 0
    {
        return Err(DecodeErrors::SofError(format!(
            "Number of components cannot be zero."
        )));
    }

    // consume spec parameters
    for i in 0..ns
    {
        // CS_i parameter, I don't need it so I might as well delete it
        let id = read_byte(&mut buf)?;

        if usize::from(id) > image.components.len()
        {
            return Err(DecodeErrors::SofError(format!(
                "Too large component ID {}, expected value between 0 and {}",
                id,
                image.components.len()
            )));
        }
        if seen[usize::from(id)]
        {
            return Err(DecodeErrors::SofError(format!(
                "Duplicate ID {} seen twice in the same component",
                id
            )));
        }
        seen[usize::from(id)] = true;

        // DC and AC huffman table position
        // top 4 bits contain dc huffman destination table
        // lower four bits contain ac huffman destination table
        let y = read_byte(&mut buf)?;
        let mut j = 0;
        while j < image.info.components
        {
            if image.components[j as usize].id == id
            {
                break;
            }
            j += 1;
        }
        if j == image.info.components
        {
            return Err(DecodeErrors::SofError(format!(
                "Invalid component id {}, expected a value between 0 and {}",
                id,
                image.components.len()
            )));
        }

        image.components[usize::from(j)].dc_huff_table = usize::from((y >> 4) & 0xF);

        image.components[usize::from(j)].ac_huff_table = usize::from(y & 0xF);
        image.z_order[i as usize] = j as usize;
    }

    // Collect the component spec parameters
    // This is only needed for progressive images but I'll read
    // them in order to ensure they are correct according to the spec

    // Extract progressive information

    // https://www.w3.org/Graphics/JPEG/itu-t81.pdf
    // Page 42

    // Start of spectral / predictor selection. (between 0 and 63)
    image.spec_start = read_byte(&mut buf)? & 63;

    // End of spectral selection
    image.spec_end = read_byte(&mut buf)? & 63;

    let bit_approx = read_byte(&mut buf)?;

    // successive approximation bit position high
    image.succ_high = bit_approx >> 4;

    if image.succ_high > 13
    {
        return Err(DecodeErrors::SofError(format!(
            "Invalid Ah parameter {}, range should be 0-13",
            image.succ_low
        )));
    }
    // successive approximation bit position low
    image.succ_low = bit_approx & 0xF;

    if image.succ_low > 13
    {
        return Err(DecodeErrors::SofError(format!(
            "Invalid Al parameter {}, range should be 0-13",
            image.succ_low
        )));
    }

    Ok(())
}

pub(crate) fn _parse_app<R>(
    buf: &mut R, marker: Marker, _info: &mut ImageInfo,
) -> Result<(), DecodeErrors>
    where
        R: BufRead + Read,
{
    let length = read_u16_be(buf)?
        .checked_sub(2)
        .ok_or(DecodeErrors::Format(format!(
            "Invalid APP0 length. Length should be greater than 2"
        )))?;

    let mut bytes_read = 0;
    match marker
    {
        Marker::APP(0) =>
            {
                if length != 14
                {
                    warn!("Incorrect length of APP0 ,{}, should be 14", length);
                }
                // Don't handle APP0 as of now
                buf.consume(length as usize);
            }
        Marker::APP(1) =>
            {
                if length >= 6
                {
                    let mut buffer = [0_u8; 6];

                    buf.read_exact(&mut buffer).map_err(|x| {
                        DecodeErrors::Format(format!("Could not read Exif data\n{}", x))
                    })?;

                    bytes_read += 6;

                    // https://web.archive.org/web/20190624045241if_/http://www.cipa.jp:80/std/documents/e/DC-008-Translation-2019-E.pdf
                    // 4.5.4 Basic Structure of Decoder Compressed Data
                    if &buffer == b"Exif\x00\x00"
                    {
                        buf.consume(length as usize - bytes_read);
                    }
                }
            }
        _ =>
            {}
    }

    Ok(())
}

/// Small utility function to print Un-zig-zagged quantization tables

fn un_zig_zag(a: &[u8]) -> [i32; 64]
{
    let mut output = [0; 64];

    for i in 0..64
    {
        output[UN_ZIGZAG[i]] = i32::from(a[i]);
    }

    output
}
