// Vorbis decoder written in Rust
//
// Copyright (c) 2016 est31 <MTest31@outlook.com>
// and contributors. All rights reserved.
// Licensed under MIT license, or Apache 2 license,
// at your option. Please see the LICENSE file
// attached to this source distribution for details.

#![forbid(unsafe_code)]
// GlassOS: without the standard library.
#![no_std]

/*!
A `vorbis` decoder, written in Rust.

If you "just" want to decode `ogg/vorbis` files, take a look into
the `inside_ogg` module (make sure you haven't disabled the `ogg` feature).

For lower level, per-packet usage, you can have a look at the `audio` and `header`
modules.
*/

#[macro_use]
extern crate alloc;
extern crate libm;
extern crate tinyvec;

/// GlassOS: a replacement for `std` for no_std. In the 2015 edition `::std::…` paths in `use` are resolved
/// from the crate root, so the modules below take everything they need from here.
mod std {
	pub use core::{cmp, convert, f32, fmt, iter, mem, ops, result, slice, u16, u32, u8};
	pub use alloc::{boxed, string, vec};
	pub mod error {
		pub trait Error: ::core::fmt::Debug + ::core::fmt::Display {}
	}
	/// Only what the comment header needs: reading from a slice.
	pub mod io {
		#[derive(Debug)]
		pub enum ErrorKind { UnexpectedEof }
		#[derive(Debug)]
		pub struct Error;
		impl Error {
			pub fn kind(&self) -> ErrorKind { ErrorKind::UnexpectedEof }
		}
		impl ::core::fmt::Display for Error {
			fn fmt(&self, f: &mut ::core::fmt::Formatter) -> ::core::fmt::Result { f.write_str("unexpected end of packet") }
		}
		pub trait Read {}
		pub struct Cursor<T> { data: T, pos: usize }
		impl<'a> Cursor<&'a [u8]> {
			pub fn new(data: &'a [u8]) -> Self { Cursor { data, pos: 0 } }
			pub fn read_u8(&mut self) -> Result<u8, Error> {
				let b = *self.data.get(self.pos).ok_or(Error)?;
				self.pos += 1;
				Ok(b)
			}
			pub fn read_u32le(&mut self) -> Result<u32, Error> {
				let mut b = [0u8; 4];
				self.read_exact(&mut b)?;
				Ok(u32::from_le_bytes(b))
			}
			pub fn read_exact(&mut self, buf: &mut [u8]) -> Result<(), Error> {
				let end = self.pos.checked_add(buf.len()).ok_or(Error)?;
				let src = self.data.get(self.pos..end).ok_or(Error)?;
				buf.copy_from_slice(src);
				self.pos = end;
				Ok(())
			}
		}
	}
	/// Floating-point math through libm (core does not have it).
	pub trait F32Ext {
		fn sin(self) -> f32;
		fn cos(self) -> f32;
		fn exp(self) -> f32;
		fn sqrt(self) -> f32;
		fn floor(self) -> f32;
		fn atan(self) -> f32;
		fn exp2(self) -> f32;
	}
	impl F32Ext for f32 {
		fn sin(self) -> f32 { ::libm::sinf(self) }
		fn cos(self) -> f32 { ::libm::cosf(self) }
		fn exp(self) -> f32 { ::libm::expf(self) }
		fn sqrt(self) -> f32 { ::libm::sqrtf(self) }
		fn floor(self) -> f32 { ::libm::floorf(self) }
		fn atan(self) -> f32 { ::libm::atanf(self) }
		fn exp2(self) -> f32 { ::libm::exp2f(self) }
	}
	pub mod prelude {
		pub use alloc::borrow::ToOwned;
		pub use alloc::boxed::Box;
		pub use alloc::string::{String, ToString};
		pub use alloc::vec::Vec;
		pub use super::F32Ext;
	}
}
#[allow(unused_imports)]
use std::prelude::*;

macro_rules! try {
	($expr:expr) => (match $expr {
		$crate::std::result::Result::Ok(val) => val,
		$crate::std::result::Result::Err(err) => {
			return Err($crate::std::convert::From::from(err));
		}
	})
}

/*
// This little thing is very useful.
macro_rules! try {
	($expr:expr) => (match $expr {
		$crate::std::result::Result::Ok(val) => val,
		$crate::std::result::Result::Err(err) => {
			panic!("Panic on Err turned on for debug reasons. Encountered Err: {:?}", err)
		}
	})
}
// */

// The following macros are super useful for debugging

macro_rules! record_residue_pre_inverse {
	($residue_vectors:expr) => {
// 		for v in $residue_vectors.iter() {
// 			for &re in v {
// 				println!("{}", re);
// 			}
// 		}
	}
}

macro_rules! record_residue_post_inverse {
	($residue_vectors:expr) => {
// 		for v in $residue_vectors.iter() {
// 			for &re in v {
// 				println!("{}", re);
// 			}
// 		}
	}
}

macro_rules! record_pre_mdct {
	($audio_spectri:expr) => {
// 		for v in $audio_spectri.iter() {
// 			for &s in v {
// 				println!("{:.5}", s);
// 			}
// 		}
	}
}

macro_rules! record_post_mdct {
	($audio_spectri:expr) => {
// 		for v in $audio_spectri.iter() {
// 			for &s in v {
// 				println!("{:.4}", s);
// 			}
// 		}
	}
}

pub mod header;
mod header_cached;
mod huffman_tree;
mod imdct;
pub mod audio;
mod bitpacking;
pub mod samples;


/// Errors that can occur during decoding
#[derive(Debug)]
pub enum VorbisError {
	BadAudio(audio::AudioReadError),
	BadHeader(header::HeaderReadError),
	#[cfg(feature = "ogg")]
	OggError(OggReadError),
}

impl ::std::error::Error for VorbisError {}

impl ::std::fmt::Display for VorbisError {
	fn fmt(&self, fmt: &mut ::std::fmt::Formatter) -> Result<(), ::std::fmt::Error> {
		write!(fmt, "{}", match self {
			VorbisError::BadAudio(_) => "Vorbis bitstream audio decode problem",
			VorbisError::BadHeader(_) => "Vorbis bitstream header decode problem",
			#[cfg(feature = "ogg")]
			VorbisError::OggError(_) => "Ogg decode problem",
		})
	}
}

impl From<audio::AudioReadError> for VorbisError {
	fn from(err :audio::AudioReadError) -> VorbisError {
		VorbisError::BadAudio(err)
	}
}

impl From<header::HeaderReadError> for VorbisError {
	fn from(err :header::HeaderReadError) -> VorbisError {
		VorbisError::BadHeader(err)
	}
}

#[cfg(feature = "ogg")]
impl From<OggReadError> for VorbisError {
	fn from(err :OggReadError) -> VorbisError {
		VorbisError::OggError(err)
	}
}

fn ilog(val :u64) -> u8 {
	64 - val.leading_zeros() as u8
}

#[test]
fn test_ilog() {
	// Uses the test vectors from the Vorbis I spec
	assert_eq!(ilog(0), 0);
	assert_eq!(ilog(1), 1);
	assert_eq!(ilog(2), 2);
	assert_eq!(ilog(3), 2);
	assert_eq!(ilog(4), 3);
	assert_eq!(ilog(7), 3);
}

fn bit_reverse(n :u32) -> u32 {
	// From the stb_vorbis implementation
	let mut nn = n;
	nn = ((nn & 0xAAAAAAAA) >> 1) | ((nn & 0x55555555) << 1);
	nn = ((nn & 0xCCCCCCCC) >> 2) | ((nn & 0x33333333) << 2);
	nn = ((nn & 0xF0F0F0F0) >> 4) | ((nn & 0x0F0F0F0F) << 4);
	nn = ((nn & 0xFF00FF00) >> 8) | ((nn & 0x00FF00FF) << 8);
	return (nn >> 16) | (nn << 16);
}

// GlassOS: the debug print_*_slice functions were removed (there is no println! in no_std).
