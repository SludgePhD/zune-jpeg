//!This crate provides a library for decoding valid
//! ITU-T Rec. T.851 (09/2005) ITU-T T.81 (JPEG-1) or JPEG images.
//!
//!
//!
//! # Features
//!  - SSE and AVX accelerated functions to speed up certain decoding operations
//!  - Really fast and accurate 32 bit IDCT algorithm
//!  - Fast color convert functions
//!  - RGBA and RGBX (4-Channel) color conversion functions
//!  - YCbCr to GrayScale conversion.
//!
//! # Usage
//! Add zune-jpeg to the dependencies in the project Cargo.toml
//!
//! ```toml
//! [dependencies]
//! zune_jpeg = "0.2.0"
//! ```
//! # Examples
//!
//! ## Decode a JPEG file with default arguments.
//! ```no_run
//! use zune_jpeg::Decoder;
//! //will contain pixels
//! let mut pixels = Decoder::new().decode_file("a_jpeg_file").unwrap();
//!
//! ```
//!
//! ## Decode a JPEG file to RGBA format
//! ```no_run
//! use zune_jpeg::{ColorSpace, Decoder, ZuneJpegOptions};
//! let mut decoder = Decoder::new_with_options(ZuneJpegOptions::new().set_out_colorspace(ColorSpace::RGBA));
//! decoder.decode_file("a_jpeg_file");
//! ```
//!
//! ## Decode an image and get it's width and height.
//! ```no_run
//! use zune_jpeg::{Decoder, ZuneJpegOptions};
//! use zune_jpeg::ColorSpace::GRAYSCALE;
//! let mut decoder = Decoder::new_with_options(ZuneJpegOptions::new()
//! .set_out_colorspace(GRAYSCALE));
//! let img_data = std::fs::read("a_valid.jpeg").unwrap();
//! decoder.read_headers(&img_data);
//! let image_info = decoder.info().unwrap();
//! println!("{},{}",image_info.width,image_info.height)
//! ```
//!
//!
//! # Crate features.
//! This crate tries to be as minimal as possible while being extensible
//! enough to handle the complexities arising from parsing different types
//! of jpeg images.
//!
//! Safety is a top concern that is why we provide both static ways to disable unsafe code,
//! disabling x86 feature, and dynamic ,by using `ZuneJpegOptions::set_unsafe(false)`,
//! both of these disable platform specific optimizations, which reduce the speed of decompression.
//!
//! Please do note that careful consideration has been taken to ensure that the unsafe paths
//! are only unsafe because they depend on platform specific intrinsics, hence no need to disable them
//!
//! The crate tries to decode as many images as possible, as a best effort, even those violating the standard
//! , this means a lot of images may  get silent warnings and wrong output, but if you are sure you will be handling
//! images that follow the spec, set `ZuneJpegOptions::set_strict` to true.

#![warn(
    clippy::correctness,
    clippy::perf,
    clippy::pedantic,
    clippy::inline_always,
    clippy::missing_errors_doc,
    clippy::panic
)]
#![allow(
    clippy::needless_return,
    clippy::similar_names,
    clippy::inline_always,
    clippy::similar_names,
    clippy::doc_markdown,
    clippy::module_name_repetitions,
    clippy::missing_panics_doc,
    clippy::missing_errors_doc
)]
#![cfg_attr(not(feature = "x86"), forbid(unsafe_code))]

#[macro_use]
extern crate log;

pub use crate::decoder::{Decoder, ImageInfo};
pub use crate::misc::ColorSpace;
pub use crate::options::ZuneJpegOptions;
pub use crate::probe::probe;

mod bitstream;
mod color_convert;
mod components;
mod decoder;
pub mod errors;
mod headers;
mod huffman;
mod idct;
mod marker;
mod mcu;
mod mcu_prog;
mod misc;
mod options;
mod probe;
mod unsafe_utils;
mod upsampler;
mod worker;
