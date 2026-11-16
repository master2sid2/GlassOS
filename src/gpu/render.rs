//! The Intel Gen7 render engine: a textured rectangle through the 3D pipeline.
//!
//! One and the same short pipeline gives both a copy and composition with transparency: the pixel
//! shader takes a texture (window) pixel and outputs it, and the blend unit either
//! just writes it, or puts it over the target as "premultiplied alpha"
//! (result = source + target × (1 − α)). Bilinear sampling is used for blurring by
//! downscaling and upscaling.
//!
//! The command order, the state layout and the shader binary are taken from
//! intel-gpu-tools, lib/rendercopy_gen7.c (MIT license):
//!   Copyright © 2013-2020 Intel Corporation and the intel-gpu-tools authors.
//!   Permission is hereby granted, free of charge, to any person obtaining a copy of
//!   this software and associated documentation files (the "Software"), to deal in the
//!   Software without restriction, including without limitation the rights to use,
//!   copy, modify, merge, publish, distribute, sublicense, and/or sell copies of the
//!   Software, subject to the following conditions: the above copyright notice and this
//!   permission notice shall be included in all copies or substantial portions of the
//!   Software. THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND.

use super::{Ring, MI_NOOP};
use alloc::string::String;

/// A 3D pipeline command: type 3, pipeline, opcode, sub-opcode.
const fn cmd3d(pipeline: u32, opcode: u32, sub: u32) -> u32 {
    3 << 29 | pipeline << 27 | opcode << 24 | sub << 16
}

const PIPELINE_SELECT_3D: u32 = cmd3d(1, 1, 4);
const STATE_BASE_ADDRESS: u32 = cmd3d(0, 1, 1);
const BASE_ADDRESS_MODIFY: u32 = 1;
const MULTISAMPLE: u32 = cmd3d(3, 1, 0x0D);
const SAMPLE_MASK: u32 = cmd3d(3, 0, 0x18);
const PUSH_CONSTANT_ALLOC_PS: u32 = cmd3d(3, 1, 0x16);
const URB_VS: u32 = cmd3d(3, 0, 0x30);
const URB_HS: u32 = cmd3d(3, 0, 0x31);
const URB_DS: u32 = cmd3d(3, 0, 0x32);
const URB_GS: u32 = cmd3d(3, 0, 0x33);
const STATE_VS: u32 = cmd3d(3, 0, 0x10);
const STATE_HS: u32 = cmd3d(3, 0, 0x1B);
const STATE_TE: u32 = cmd3d(3, 0, 0x1C);
const STATE_DS: u32 = cmd3d(3, 0, 0x1D);
const STATE_GS: u32 = cmd3d(3, 0, 0x11);
const STATE_CLIP: u32 = cmd3d(3, 0, 0x12);
const VIEWPORT_SF_CL: u32 = cmd3d(3, 0, 0x21);
const STATE_SF: u32 = cmd3d(3, 0, 0x13);
const STATE_WM: u32 = cmd3d(3, 0, 0x14);
const STREAMOUT: u32 = cmd3d(3, 0, 0x1E);
const DEPTH_BUFFER: u32 = cmd3d(3, 0, 0x05);
const CLEAR_PARAMS: u32 = cmd3d(3, 0, 0x04);
const BLEND_STATE_POINTERS: u32 = cmd3d(3, 0, 0x24);
const VIEWPORT_CC: u32 = cmd3d(3, 0, 0x23);
const SAMPLER_POINTERS_PS: u32 = cmd3d(3, 0, 0x2F);
const STATE_SBE: u32 = cmd3d(3, 0, 0x1F);
const STATE_PS: u32 = cmd3d(3, 0, 0x20);
const VERTEX_ELEMENTS: u32 = cmd3d(3, 0, 9);
const VERTEX_BUFFERS: u32 = cmd3d(3, 0, 8);
const BINDING_TABLE_PS: u32 = cmd3d(3, 0, 0x2A);
const DRAWING_RECTANGLE: u32 = cmd3d(3, 1, 0);
const PRIMITIVE: u32 = cmd3d(3, 3, 0);
const PRIM_RECTLIST: u32 = 0x0F;

const MI_BATCH_BUFFER_START: u32 = 0x31 << 23;
const MI_BATCH_BUFFER_END: u32 = 0x0A << 23;

const FORMAT_B8G8R8A8_UNORM: u32 = 0x0C0;
const FORMAT_R32G32B32A32_FLOAT: u32 = 0x000;
const FORMAT_R16G16_SSCALED: u32 = 0x0F6;
const FORMAT_R32G32_FLOAT: u32 = 0x085;
const SURFACE_2D: u32 = 1;
const SURFACE_NULL: u32 = 7;
/// Valleyview: the surface is cached in L3.
const VLV_MOCS_L3: u32 = 1;

const VFCOMP_STORE_SRC: u32 = 1;
const VFCOMP_STORE_0: u32 = 2;
const VFCOMP_STORE_1_FLT: u32 = 3;

const BLEND_ONE: u32 = 0x01;
const BLEND_CONST_ALPHA: u32 = 0x08;
const BLEND_ZERO: u32 = 0x11;
const BLEND_INV_SRC_ALPHA: u32 = 0x13;
const BLEND_INV_CONST_ALPHA: u32 = 0x18;
const CC_STATE_POINTERS: u32 = cmd3d(3, 0, 0x0E);

/// The shader (SIMD16): interpolate the texture coordinates, fetch a pixel from binding table
/// entry 1 with sampler 0, write it to render target 0 and finish.
const PS_KERNEL: [u32; 32] = [
    0x0080005a, 0x2e2077bd, 0x000000c0, 0x008d0040, //
    0x0080005a, 0x2e6077bd, 0x000000d0, 0x008d0040, //
    0x02800031, 0x21801fa9, 0x008d0e20, 0x08840001, //
    0x00800001, 0x2e2003bd, 0x008d0180, 0x00000000, //
    0x00800001, 0x2e6003bd, 0x008d01c0, 0x00000000, //
    0x00800001, 0x2ea003bd, 0x008d0200, 0x00000000, //
    0x00800001, 0x2ee003bd, 0x008d0240, 0x00000000, //
    0x05800031, 0x20001fa8, 0x008d0e20, 0x90031000, //
];

/// A batch of commands and its states (a shared buffer: commands from the start, states from the middle).
pub(super) const BATCH_BYTES: usize = 16 * 1024;
const STATE_AT: usize = 4096;

/// A picture as the GPU sees it: address, dimensions, line stride in bytes (32 bits per pixel).
#[derive(Clone, Copy)]
pub struct Image {
    pub addr: u32,
    pub w: u32,
    pub h: u32,
    pub pitch: u32,
}

#[derive(Clone, Copy, PartialEq)]
pub enum Blend {
    /// Just write.
    Copy,
    /// Over the target, premultiplied alpha.
    Over,
    /// A mix: result = source × share + target × (1 − share) (share 0..1).
    Mix(f32),
}

/// The batch buffer: kernel memory and its GPU address.
pub(super) struct Batch {
    pub cpu: *mut u32,
    pub gpu: u32,
}

unsafe impl Send for Batch {}

/// Builds a batch in memory: commands from the start, states from `STATE_AT`.
struct Builder<'a> {
    b: &'a Batch,
    cmd: usize,
    state: usize,
}

impl Builder<'_> {
    fn out(&mut self, words: &[u32]) {
        for &w in words {
            unsafe { self.b.cpu.add(self.cmd / 4).write_volatile(w) };
            self.cmd += 4;
        }
    }

    /// Puts a state with alignment; returns its offset from the start of the batch.
    fn state(&mut self, align: usize, words: &[u32]) -> u32 {
        self.state = self.state.next_multiple_of(align);
        let at = self.state;
        for (i, &w) in words.iter().enumerate() {
            unsafe { self.b.cpu.add(at / 4 + i).write_volatile(w) };
        }
        self.state += words.len() * 4;
        at as u32
    }
}

fn surface(img: Image) -> [u32; 8] {
    [
        SURFACE_2D << 29 | FORMAT_B8G8R8A8_UNORM << 18,
        img.addr,
        (img.h - 1) << 16 | (img.w - 1),
        img.pitch - 1,
        0,
        VLV_MOCS_L3 << 16,
        0,
        0,
    ]
}

/// The target rectangle: x, y, width, height.
pub type Area = (u32, u32, u32, u32);
/// The source rectangle — in pixels, possibly fractional (blur levels: half pixels).
pub type From = (f32, f32, f32, f32);

pub fn whole(a: Area) -> From {
    (a.0 as f32, a.1 as f32, a.2 as f32, a.3 as f32)
}

/// Draws piece `from` of picture `src` into piece `to` of picture `dst` (the sizes may
/// differ — then it stretches or shrinks): builds a batch and puts it into the ring.
/// The caller does the fence and the waiting.
pub(super) fn draw(ring: &mut Ring, batch: &Batch, dst: Image, to: Area, src: Image, from: From, blend: Blend, linear: bool) -> Result<(), String> {
    let ((dx, dy, w, h), (sx, sy, sw, sh)) = (to, from);
    let inside = w > 0 && h > 0 && dx + w <= dst.w && dy + h <= dst.h;
    // The source may extend beyond the edge: sampling is clamped to the edge (the edge pixels there),
    // it never reads outside the picture.
    let limit = 32768.0;
    let from_inside = sw > 0.0 && sh > 0.0 && sx > -limit && sy > -limit && sx + sw < limit && sy + sh < limit;
    if !inside || !from_inside || dst.w > 16384 || dst.h > 16384 || src.w > 16384 || src.h > 16384 {
        return Err("rectangle out of bounds".into());
    }
    let mut b = Builder { b: batch, cmd: 0, state: STATE_AT };
    // --- states (offsets from the start of the batch: all base addresses = the start of the batch)
    let blend0 = match blend {
        Blend::Copy => BLEND_ZERO | BLEND_ONE << 5,
        Blend::Over => BLEND_INV_SRC_ALPHA | BLEND_ONE << 5 | 1 << 31,
        Blend::Mix(_) => BLEND_INV_CONST_ALPHA | BLEND_CONST_ALPHA << 5 | 1 << 31,
    };
    let blend_state = b.state(64, &[blend0, 0b11]);
    // The "colour calc" state: the constant blend colour is dwords 2..5 (R, G, B, A).
    let share = if let Blend::Mix(t) = blend { t.clamp(0.0, 1.0) } else { 1.0 };
    let cc_state = b.state(64, &[0, 0, 0, 0, 0, share.to_bits()]);
    let cc_viewport = b.state(32, &[(-1.0e35f32).to_bits(), 1.0e35f32.to_bits()]);
    let filter = if linear { 1 << 14 | 1 << 17 } else { 0 };
    let clamp = 2;
    let sampler = b.state(32, &[filter, 0, 0, clamp | clamp << 3 | clamp << 6 | 1 << 10]);
    let kernel = b.state(64, &PS_KERNEL);
    // The rectangle vertices (three corners; the pipeline builds the fourth itself):
    // x, y — 16-bit integers, s, t — float (12 bytes per vertex).
    let v = |x: u32, y: u32, s: f32, t: f32| [x & 0xFFFF | y << 16, s.to_bits(), t.to_bits()];
    let mut corners = [0u32; 9];
    corners[0..3].copy_from_slice(&v(dx + w, dy + h, sx + sw, sy + sh));
    corners[3..6].copy_from_slice(&v(dx, dy + h, sx, sy + sh));
    corners[6..9].copy_from_slice(&v(dx, dy, sx, sy));
    let vertices = b.state(8, &corners);
    let dst_ss = b.state(32, &surface(dst));
    let src_ss = b.state(32, &surface(src));
    let table = b.state(32, &[dst_ss, src_ss]);
    if b.state > BATCH_BYTES {
        return Err("batch overflow".into());
    }
    let base = batch.gpu | BASE_ADDRESS_MODIFY;
    // --- commands
    b.out(&[PIPELINE_SELECT_3D]);
    b.out(&[STATE_BASE_ADDRESS | (10 - 2), 0, base, base, 0, base, 0, BASE_ADDRESS_MODIFY, 0, BASE_ADDRESS_MODIFY]);
    b.out(&[MULTISAMPLE | (4 - 2), 0, 0, 0, SAMPLE_MASK, 1]);
    b.out(&[PUSH_CONSTANT_ALLOC_PS, 8]);
    b.out(&[URB_VS, 64 | (2 - 1) << 16 | 1 << 25, URB_HS, 2 << 25, URB_DS, 2 << 25, URB_GS, 1 << 25]);
    b.out(&[STATE_VS | (6 - 2), 0, 0, 0, 0, 0]);
    b.out(&[STATE_HS | (7 - 2), 0, 0, 0, 0, 0, 0]);
    b.out(&[STATE_TE | (4 - 2), 0, 0, 0]);
    b.out(&[STATE_DS | (6 - 2), 0, 0, 0, 0, 0]);
    b.out(&[STATE_GS | (7 - 2), 0, 0, 0, 0, 0, 0]);
    b.out(&[STATE_CLIP | (4 - 2), 0, 0, 0, VIEWPORT_SF_CL, 0]);
    b.out(&[STATE_SF | (7 - 2), 0, 1 << 29, 2 << 25, 0, 0, 0]);
    b.out(&[STATE_WM | (3 - 2), 1 << 29 | 1 << 11, 0]);
    b.out(&[STREAMOUT | (3 - 2), 0, 0]);
    b.out(&[DEPTH_BUFFER | (7 - 2), SURFACE_NULL << 29 | 1 << 18, 0, 0, 0, 0, 0]);
    b.out(&[CLEAR_PARAMS | (3 - 2), 0, 0]);
    b.out(&[BLEND_STATE_POINTERS, blend_state, VIEWPORT_CC, cc_viewport]);
    b.out(&[CC_STATE_POINTERS, cc_state | 1]);
    b.out(&[SAMPLER_POINTERS_PS, sampler]);
    b.out(&[STATE_SBE | (14 - 2), 1 << 22 | 1 << 11 | 1 << 4, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
    b.out(&[STATE_PS | (8 - 2), kernel, 1 << 27 | 2 << 18, 0, 40 << 24 | 1 << 1 | 1 << 10, 6 << 16, 0, 0]);
    let element = |format: u32, offset: u32, comps: [u32; 4]| [1 << 25 | format << 16 | offset, comps[0] << 28 | comps[1] << 24 | comps[2] << 20 | comps[3] << 16];
    b.out(&[VERTEX_ELEMENTS | (2 * 3 + 1 - 2)]);
    b.out(&element(FORMAT_R32G32B32A32_FLOAT, 0, [VFCOMP_STORE_0; 4]));
    b.out(&element(FORMAT_R16G16_SSCALED, 0, [VFCOMP_STORE_SRC, VFCOMP_STORE_SRC, VFCOMP_STORE_0, VFCOMP_STORE_1_FLT]));
    b.out(&element(FORMAT_R32G32_FLOAT, 4, [VFCOMP_STORE_SRC, VFCOMP_STORE_SRC, VFCOMP_STORE_0, VFCOMP_STORE_1_FLT]));
    b.out(&[VERTEX_BUFFERS | (5 - 2), 1 << 14 | 12, batch.gpu + vertices, !0, 0]);
    b.out(&[BINDING_TABLE_PS, table]);
    b.out(&[DRAWING_RECTANGLE | (4 - 2), 0, (dst.h - 1) << 16 | (dst.w - 1), 0]);
    b.out(&[PRIMITIVE | (7 - 2), PRIM_RECTLIST, 3, 0, 1, 0, 0]);
    b.out(&[MI_BATCH_BUFFER_END, MI_NOOP]);
    if b.cmd > STATE_AT {
        return Err("command area overflow".into());
    }
    core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);
    ring.invalidate()?;
    ring.submit(&[MI_BATCH_BUFFER_START, batch.gpu])
}
