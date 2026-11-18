//! Read the H.264 evidence needed to qualify an encoder and open a segment.

use std::collections::BTreeMap;

use windows::Win32::Media::MediaFoundation::{
    IMFMediaType, IMFSample, MF_MT_MPEG_SEQUENCE_HEADER,
};

use super::{ProbeError, stage, unavailable};

const MAX_HEADER_BYTES: u32 = 64 * 1024;
const MAX_SAMPLE_BYTES: u32 = 32 * 1024 * 1024;

#[derive(Default)]
pub(super) struct Headers {
    /// SPS NAL units by id.
    sps: BTreeMap<u32, Vec<u8>>,
    /// PPS NAL units by id, with the SPS id each refers to.
    pps: BTreeMap<u32, (u32, Vec<u8>)>,
    length_width: Option<usize>,
}

pub(super) struct Picture {
    pub idr: bool,
    pub b_slice: bool,
    pps: u32,
}

impl Headers {
    pub(super) fn read_media_type(&mut self, media_type: &IMFMediaType) -> Result<(), ProbeError> {
        let size = match unsafe { media_type.GetBlobSize(&MF_MT_MPEG_SEQUENCE_HEADER) } {
            Ok(size) => size,
            Err(error) if error.code() == windows::Win32::Media::MediaFoundation::MF_E_ATTRIBUTENOTFOUND => return Ok(()),
            Err(error) => return stage(Err(error), "Read the H.264 sequence header size"),
        };
        if size == 0 || size > MAX_HEADER_BYTES {
            return Err(invalid());
        }
        let mut bytes = vec![0; size as usize];
        stage(unsafe { media_type.GetBlob(&MF_MT_MPEG_SEQUENCE_HEADER, &mut bytes, None) },
            "Read the H.264 sequence header")?;
        if bytes.first() == Some(&1) && bytes.len() >= 7 {
            let width = usize::from((bytes[4] & 3) + 1);
            let count = bytes[5] & 31;
            let remaining = self.read_configuration_nals(&bytes[6..], count, 7)?;
            let (&count, remaining) = remaining.split_first().ok_or_else(invalid)?;
            self.read_configuration_nals(remaining, count, 8)?;
            self.length_width = Some(width);
        } else {
            annex_b(&bytes, |nal| match nal[0] & 31 {
                7 | 8 => self.parameter_set(nal),
                _ => Ok(()),
            })?;
        }
        Ok(())
    }

    fn read_configuration_nals<'a>(&mut self, mut bytes: &'a [u8], count: u8, kind: u8) -> Result<&'a [u8], ProbeError> {
        for _ in 0..count {
            let (nal, remaining) = take_nal(bytes, 2)?;
            if nal.first().copied().map(|header| header & 31) != Some(kind) {
                return Err(invalid());
            }
            self.parameter_set(nal)?;
            bytes = remaining;
        }
        Ok(bytes)
    }

    /// Verify all VCL NALs in an access unit, even if the MFT supplied a sample flag.
    pub(super) fn inspect(&mut self, sample: &IMFSample) -> Result<Picture, ProbeError> {
        let buffer = stage(unsafe { sample.ConvertToContiguousBuffer() }, "Get the H.264 sample bytes")?;
        let mut data = std::ptr::null_mut();
        let mut len = 0;
        stage(unsafe { buffer.Lock(&mut data, None, Some(&mut len)) }, "Lock the H.264 sample bytes")?;
        let inspected = if !data.is_null() && len != 0 && len <= MAX_SAMPLE_BYTES {
            // The locked IMFMediaBuffer supplies len valid bytes until Unlock.
            self.inspect_bytes(unsafe { std::slice::from_raw_parts(data, len as usize) })
        } else {
            Err(invalid())
        };
        stage(unsafe { buffer.Unlock() }, "Unlock the H.264 sample bytes")?;
        inspected
    }

    fn inspect_bytes(&mut self, bytes: &[u8]) -> Result<Picture, ProbeError> {
        let mut picture = None::<Picture>;
        let width = self.length_width;
        let visit = |nal: &[u8]| {
            let (&header, payload) = nal.split_first().ok_or_else(invalid)?;
            if header & 0x80 != 0 {
                return Err(invalid());
            }
            match header & 31 {
                kind @ (1 | 5) => {
                    let mut bits = Bits::new(payload);
                    bits.ue().ok_or_else(invalid)?; // first_mb_in_slice
                    let slice = bits.ue().filter(|slice| *slice <= 9).ok_or_else(invalid)?;
                    let pps = bits.ue().filter(|id| *id <= 255).ok_or_else(invalid)?;
                    let idr = kind == 5;
                    let b_slice = slice % 5 == 1;
                    if idr && slice % 5 != 2 && slice % 5 != 4 {
                        return Err(invalid());
                    }
                    if let Some(first) = &mut picture {
                        if first.idr != idr || first.pps != pps {
                            return Err(invalid());
                        }
                        first.b_slice |= b_slice;
                    } else {
                        picture = Some(Picture { idr, b_slice, pps });
                    }
                }
                2..=4 | 19..=21 => return Err(invalid()), // unsupported VCL arrangements
                7 | 8 => self.parameter_set(nal)?,
                _ => {}
            }
            Ok(())
        };
        if let Some(width) = width {
            length_prefixed(bytes, width, visit)?;
        } else if bytes.starts_with(&[0, 0, 1]) || bytes.starts_with(&[0, 0, 0, 1]) {
            annex_b(bytes, visit)?;
        } else {
            length_prefixed(bytes, 4, visit)?;
        }
        picture.ok_or_else(invalid)
    }

    fn parameter_set(&mut self, nal: &[u8]) -> Result<(), ProbeError> {
        let (&header, payload) = nal.split_first().ok_or_else(invalid)?;
        if nal.len() > 4096 || header & 0x80 != 0 {
            return Err(invalid());
        }
        let mut bits = Bits::new(payload);
        match header & 31 {
            7 => {
                let profile = bits.read(8).ok_or_else(invalid)?;
                bits.read(16).ok_or_else(invalid)?; // constraint flags, level
                let id = bits.ue().filter(|id| *id <= 31).ok_or_else(invalid)?;
                if profile != 100 || bits.ue() != Some(1) || bits.ue() != Some(0) || bits.ue() != Some(0) {
                    return Err(unavailable("The H.264 SPS does not signal High profile with 8-bit 4:2:0"));
                }
                match check_vui(&mut bits).ok_or_else(invalid)? {
                    Some(false) => return Err(unavailable("The H.264 SPS color signal conflicts with studio-range BT.709 gamma 2.2")),
                    Some(true) | None => {}
                }
                self.sps.insert(id, nal.to_vec());
            }
            8 => {
                let id = bits.ue().filter(|id| *id <= 255).ok_or_else(invalid)?;
                let sps = bits.ue().filter(|id| *id <= 31).ok_or_else(invalid)?;
                self.pps.insert(id, (sps, nal.to_vec()));
            }
            _ => {}
        }
        Ok(())
    }

    /// MPEG sequence headers use Annex B start codes regardless of sample framing.
    pub(super) fn segment_header(&self, picture: &Picture) -> Result<Vec<u8>, ProbeError> {
        if !picture.idr || picture.b_slice {
            return Err(unavailable("The H.264 segment boundary is not an IDR"));
        }
        let (sps_id, pps) = self.pps.get(&picture.pps).ok_or_else(invalid)?;
        let sps = self.sps.get(sps_id).ok_or_else(invalid)?;
        let mut sequence = Vec::with_capacity(8 + sps.len() + pps.len());
        for nal in [sps, pps] {
            sequence.extend_from_slice(&[0, 0, 0, 1]);
            sequence.extend_from_slice(nal);
        }
        Ok(sequence)
    }
}

// Walk the SPS fields after bit depth. Absent VUI color is left to the
// negotiated stream metadata. Explicitly contradictory VUI is rejected.
fn check_vui(bits: &mut Bits<'_>) -> Option<Option<bool>> {
    bits.read(1)?; // qpprime_y_zero_transform_bypass_flag
    if bits.read(1)? != 0 {
        for list in 0..8 {
            if bits.read(1)? == 0 {
                continue;
            }
            let mut previous = 8_i64;
            let mut next = 8_i64;
            for _ in 0..if list < 6 { 16 } else { 64 } {
                if next != 0 {
                    next = (previous + bits.se()?).rem_euclid(256);
                }
                if next != 0 {
                    previous = next;
                }
            }
        }
    }
    bits.ue()?; // log2_max_frame_num_minus4
    match bits.ue()? {
        0 => { bits.ue()?; }
        1 => {
            bits.read(1)?;
            bits.se()?;
            bits.se()?;
            let cycle = bits.ue()?;
            if cycle > 255 { return None; }
            for _ in 0..cycle { bits.se()?; }
        }
        2 => {}
        _ => return None,
    }
    bits.ue()?; // max_num_ref_frames
    bits.read(1)?;
    bits.ue()?; // pic_width_in_mbs_minus1
    bits.ue()?; // pic_height_in_map_units_minus1
    if bits.read(1)? == 0 {
        bits.read(1)?; // mb_adaptive_frame_field_flag
    }
    bits.read(1)?; // direct_8x8_inference_flag
    if bits.read(1)? != 0 {
        for _ in 0..4 { bits.ue()?; }
    }
    if bits.read(1)? == 0 {
        return Some(None);
    }
    if bits.read(1)? != 0 && bits.read(8)? == 255 {
        bits.read(16)?;
        bits.read(16)?;
    }
    if bits.read(1)? != 0 {
        bits.read(1)?; // overscan_appropriate_flag
    }
    if bits.read(1)? == 0 {
        return Some(None);
    }
    bits.read(3)?; // video_format
    let limited = bits.read(1)? == 0;
    if bits.read(1)? == 0 {
        return Some((!limited).then_some(false));
    }
    let primaries = bits.read(8)?;
    let transfer = bits.read(8)?;
    let matrix = bits.read(8)?;
    Some(Some(limited && (primaries == 1 || primaries == 2)
        && (transfer == 4 || transfer == 2) && (matrix == 1 || matrix == 2)))
}

fn invalid() -> ProbeError {
    unavailable("The H.264 access unit or parameter sets are invalid or incomplete")
}

fn annex_b(mut bytes: &[u8], mut visit: impl FnMut(&[u8]) -> Result<(), ProbeError>) -> Result<(), ProbeError> {
    let first = start_code(bytes).ok_or_else(invalid)?;
    if bytes[..first].iter().any(|&byte| byte != 0) {
        return Err(invalid());
    }
    bytes = &bytes[first + 3..];
    loop {
        let next = start_code(bytes);
        let mut nal = &bytes[..next.unwrap_or(bytes.len())];
        while let Some((&0, rest)) = nal.split_last() {
            nal = rest;
        }
        if nal.is_empty() {
            return Err(invalid());
        }
        visit(nal)?;
        let Some(next) = next else { return Ok(()) };
        bytes = &bytes[next + 3..];
    }
}

fn start_code(bytes: &[u8]) -> Option<usize> {
    bytes.windows(3).position(|window| window == [0, 0, 1])
}

fn length_prefixed(mut bytes: &[u8], width: usize, mut visit: impl FnMut(&[u8]) -> Result<(), ProbeError>) -> Result<(), ProbeError> {
    while !bytes.is_empty() {
        let (nal, remaining) = take_nal(bytes, width)?;
        visit(nal)?;
        bytes = remaining;
    }
    Ok(())
}

fn take_nal(bytes: &[u8], width: usize) -> Result<(&[u8], &[u8]), ProbeError> {
    let (prefix, bytes) = bytes.split_at_checked(width).ok_or_else(invalid)?;
    let length = prefix.iter().fold(0_usize, |value, &byte| (value << 8) | usize::from(byte));
    if length == 0 {
        return Err(invalid());
    }
    bytes.split_at_checked(length).ok_or_else(invalid)
}

/// Incremental RBSP reader. It avoids copying a full compressed slice.
struct Bits<'a> {
    remaining: &'a [u8],
    zeros: u8,
    byte: u8,
    left: u8,
}

impl<'a> Bits<'a> {
    fn new(remaining: &'a [u8]) -> Self {
        Self { remaining, zeros: 0, byte: 0, left: 0 }
    }

    fn read(&mut self, count: u32) -> Option<u32> {
        let mut value = 0;
        for _ in 0..count {
            if self.left == 0 {
                let (&byte, remaining) = self.remaining.split_first()?;
                self.remaining = remaining;
                self.byte = if self.zeros == 2 && byte == 3 {
                    let (&unescaped, remaining) = self.remaining.split_first()?;
                    self.remaining = remaining;
                    self.zeros = 0;
                    unescaped
                } else {
                    byte
                };
                self.zeros = if self.byte == 0 { (self.zeros + 1).min(2) } else { 0 };
                self.left = 8;
            }
            self.left -= 1;
            value = (value << 1) | u32::from((self.byte >> self.left) & 1);
        }
        Some(value)
    }

    fn se(&mut self) -> Option<i64> {
        let value = i64::from(self.ue()?);
        Some(if value % 2 == 0 { -(value / 2) } else { (value + 1) / 2 })
    }

    fn ue(&mut self) -> Option<u32> {
        let mut zeros = 0;
        while self.read(1)? == 0 {
            zeros += 1;
            if zeros >= 31 {
                return None;
            }
        }
        Some((1_u32 << zeros) - 1 + self.read(zeros)?)
    }
}
