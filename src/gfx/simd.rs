//! Hot graphics loops on SSE: four pixels per instruction (window composition, shadows).
//! Blur is not here: the compiler vectorizes its loops with SSE by itself just as well (measured). The result is pixel-exact
//! the same as that of the simple versions in `gfx.rs` (`bench` compares them at startup).
//! The kernel is built for x86-64-v2, so SSE2…SSE4.1 are always there.

use core::arch::x86_64::*;

/// A window surface (premultiplied alpha) over a row: like `over`.
pub fn over_row(d: &mut [u32], s: &[u32]) {
    let n = d.len().min(s.len());
    let mut i = 0;
    unsafe {
        let zero = _mm_setzero_si128();
        let opaque = _mm_set1_epi32(255);
        let full = _mm_set1_epi32(256);
        while i + 4 <= n {
            let sp = _mm_loadu_si128(s.as_ptr().add(i) as *const __m128i);
            let a = _mm_srli_epi32(sp, 24);
            let solid = _mm_movemask_epi8(_mm_cmpeq_epi32(a, opaque));
            if solid == 0xFFFF {
                _mm_storeu_si128(d.as_mut_ptr().add(i) as *mut __m128i, sp);
                i += 4;
                continue;
            }
            let clear = _mm_cmpeq_epi32(a, zero);
            if _mm_movemask_epi8(clear) == 0xFFFF {
                i += 4;
                continue;
            }
            let dp = _mm_loadu_si128(d.as_ptr().add(i) as *const __m128i);
            // The background multiplier: 256 − a − a/128, per pixel; spread over 4 channels.
            let f = _mm_sub_epi32(_mm_sub_epi32(full, a), _mm_srli_epi32(a, 7));
            let f16 = _mm_packs_epi32(f, f);
            let pairs = _mm_unpacklo_epi16(f16, f16);
            let (flo, fhi) = (_mm_unpacklo_epi32(pairs, pairs), _mm_unpackhi_epi32(pairs, pairs));
            let lo = _mm_srli_epi16(_mm_mullo_epi16(_mm_unpacklo_epi8(dp, zero), flo), 8);
            let hi = _mm_srli_epi16(_mm_mullo_epi16(_mm_unpackhi_epi8(dp, zero), fhi), 8);
            let mixed = _mm_add_epi32(sp, _mm_packus_epi16(lo, hi));
            // A fully transparent window pixel — the background as is.
            let out = _mm_or_si128(_mm_and_si128(clear, dp), _mm_andnot_si128(clear, mixed));
            _mm_storeu_si128(d.as_mut_ptr().add(i) as *mut __m128i, out);
            i += 4;
        }
    }
    for k in i..n {
        d[k] = super::over(s[k], d[k]);
    }
}

/// A shadow: darken each pixel by `mask` (0..=255) — like `scale(p, 256 − a)`.
pub fn shade_row(d: &mut [u32], mask: &[u8]) {
    let n = d.len().min(mask.len());
    let mut i = 0;
    unsafe {
        let zero = _mm_setzero_si128();
        let full = _mm_set1_epi32(256);
        while i + 4 <= n {
            let m = u32::from_le_bytes([mask[i], mask[i + 1], mask[i + 2], mask[i + 3]]);
            if m == 0 {
                i += 4;
                continue;
            }
            let a = _mm_cvtepu8_epi32(_mm_cvtsi32_si128(m as i32));
            let f = _mm_sub_epi32(full, a);
            let f16 = _mm_packs_epi32(f, f);
            let pairs = _mm_unpacklo_epi16(f16, f16);
            let (flo, fhi) = (_mm_unpacklo_epi32(pairs, pairs), _mm_unpackhi_epi32(pairs, pairs));
            let dp = _mm_loadu_si128(d.as_ptr().add(i) as *const __m128i);
            let lo = _mm_srli_epi16(_mm_mullo_epi16(_mm_unpacklo_epi8(dp, zero), flo), 8);
            let hi = _mm_srli_epi16(_mm_mullo_epi16(_mm_unpackhi_epi8(dp, zero), fhi), 8);
            _mm_storeu_si128(d.as_mut_ptr().add(i) as *mut __m128i, _mm_packus_epi16(lo, hi));
            i += 4;
        }
    }
    for k in i..n {
        if mask[k] != 0 {
            d[k] = super::scale(d[k], 256 - mask[k] as u32);
        }
    }
}
