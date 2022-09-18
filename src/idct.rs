//! Routines for IDCT
//!
//! Essentially we provide 2 routines for IDCT, a scalar implementation and a not super optimized
//! AVX2 one, i'll talk about them here.
//!
//! There are 2 reasons why we have the avx one
//! 1. No one compiles with -C target-features=avx2 hence binaries won't probably take advantage(even
//! if it exists).
//! 2. AVX employs zero short circuit in a way the scalar code cannot employ it.
//!     - AVX does this by checking for MCU's whose 63 AC coefficients are zero and if true, it writes
//!        values directly, if false, it goes the long way of calculating.
//!     -   Although this can be trivially implemented in the scalar version, it  generates code
//!         I'm not happy width(scalar version that basically loops and that is too many branches for me)
//!         The avx one does a better job of using bitwise or's with (`_mm256_or_si256`) which is magnitudes of faster
//!         than anything I could come up with
//!
//! The AVX code also has some cool transpose instructions which look so complicated to be cool
//! (spoiler alert, i barely understand how it works, that's why I credited the owner).
//!
#![allow(
    clippy::excessive_precision,
    clippy::unreadable_literal,
    clippy::module_name_repetitions,
    unused_parens,
    clippy::wildcard_imports
)]

use crate::decoder::IDCTPtr;
#[cfg(feature = "X86")]
use crate::idct::avx2::dequantize_and_idct_avx2;
use crate::idct::scalar::dequantize_and_idct_int;

#[cfg(feature = "x86")]
mod avx2;

mod scalar;

/// Choose an appropriate IDCT function

pub fn choose_idct_func(use_unsafe: bool) -> IDCTPtr
{
    if use_unsafe
    {
        #[cfg(all(feature = "x86", any(target_arch = "x86_64", target_arch = "x86")))]
        {
            if is_x86_feature_detected!("avx2")
            {
                debug!("Using AVX optimized integer IDCT");
                // use avx one
                return crate::idct::avx2::dequantize_and_idct_avx2;
            }
        }
    }
    debug!("Using scalar integer IDCT");
    // Fun fact, when compiling this with -C target-feature=+avx2, Rust won't
    // use CPUID instructions for run-time detection and this function will boil down
    // to a return statement above.

    // use generic one
    return dequantize_and_idct_int;
}

//------------------------------------------------------
// TEST CODE
// -----------------------------------------------------
#[test]
#[cfg(feature = "x86")]
#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
fn test_zeroes()
{
    use crate::misc::Aligned32;

    let qt_table = Aligned32([1; 64]);
    let stride = 8;
    let coeff = vec![0; 64];
    let output_scalar = dequantize_and_idct_int(&coeff, &qt_table, stride, 1, 1);
    let output_avx = crate::idct::avx2::dequantize_and_idct_avx2(&coeff, &qt_table, stride, 1, 1);
    assert_eq!(output_scalar, output_avx, "AVX and scalar do not match");
    // output should be 128 because IDCT does level shifting too..
    assert_eq!(output_scalar, &[128; 64], "Test for zeroes failed");
}

#[test]
#[cfg(feature = "x86")]
#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
// disable this because rust will bounds check wrapping additions which won't make sense as IDCT depends on wrapping arithmetic
#[cfg(not(debug_assertions))]
fn test_max()
{
    use crate::misc::Aligned32;

    let qt_table = Aligned32([1; 64]);
    let stride = 8;
    let coeff = vec![i16::MAX; 64];
    let output = [
        0, 255, 0, 255, 0, 0, 255, 255, 255, 0, 0, 255, 0, 255, 0, 0, 0, 0, 255, 0, 255, 0, 255,
        255, 255, 255, 0, 255, 0, 255, 0, 0, 0, 0, 255, 0, 255, 0, 255, 255, 0, 255, 0, 255, 0,
        158, 0, 49, 255, 0, 255, 0, 255, 0, 255, 255, 255, 0, 255, 0, 255, 49, 255, 255,
    ];
    let output_scalar = dequantize_and_idct_int(&coeff, &qt_table, stride, 1, 1);
    let output_avx = crate::idct::avx2::dequantize_and_idct_avx2(&coeff, &qt_table, stride, 1, 1);
    assert_eq!(output_scalar, output_avx, "AVX and scalar do not match");

    assert_eq!(output_avx, &output, "Test for max IDCT failed");
}

#[test]
#[cfg(feature = "x86")]
#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
#[cfg(not(debug_assertions))] // disable this because rust will bounds check wrapping additions which won't work for debug builds
fn test_min()
{
    use crate::misc::Aligned32;

    let qt_table = Aligned32([1; 64]);
    let stride = 8;
    let coeff = vec![i16::MIN; 64];
    let output = [
        255, 0, 255, 0, 255, 255, 0, 0, 0, 255, 255, 0, 255, 0, 255, 255, 255, 255, 0, 255, 0, 255,
        0, 0, 0, 0, 255, 0, 255, 0, 255, 255, 255, 255, 0, 255, 0, 255, 0, 0, 255, 0, 255, 0, 255,
        98, 255, 207, 0, 255, 0, 255, 0, 255, 0, 0, 0, 255, 0, 255, 0, 207, 0, 0,
    ];
    let output_scalar = dequantize_and_idct_int(&coeff, &qt_table, stride, 1, 1);
    let output_avx = crate::idct::avx2::dequantize_and_idct_avx2(&coeff, &qt_table, stride, 1, 1);
    assert_eq!(output_scalar, output_avx, "AVX and scalar do not match");
    assert_eq!(output_avx, &output, "Test for min IDCT fails");
}
