//! Known SpaceMouse raw report layouts. A shared collection usage or vendor
//! alone cannot identify the wire format of a receiver, keyboard or numpad.

use screen_fix_protocol::{BoundedList, ipc::{NdofData, NdofReport}};

#[derive(Clone, Copy)]
pub(super) enum Profile {
    SplitAxes,
    CombinedAxes,
}

impl Profile {
    pub fn identify(vendor: u32, product: u32) -> Option<Self> {
        match (vendor, product) {
            (0x046D, 0xC623 | 0xC625..=0xC629 | 0xC62B) => Some(Self::SplitAxes),
            (0x256F, 0xC62E | 0xC631 | 0xC633 | 0xC635) => Some(Self::CombinedAxes),
            _ => None,
        }
    }

    pub fn decode(self, bytes: &[u8]) -> Option<NdofReport> {
        let (&id, data) = bytes.split_first()?;
        let decoded = match (self, id, data.len()) {
            (Self::SplitAxes, 1, 6) => NdofData::Motion { translation: Some(axes(data)?), rotation: None },
            (Self::SplitAxes, 2, 6) => NdofData::Motion { translation: None, rotation: Some(axes(data)?) },
            (Self::CombinedAxes, 1, 12) => NdofData::Motion {
                translation: Some(axes(&data[..6])?), rotation: Some(axes(&data[6..])?),
            },
            (_, 3, 1..=8) => {
                let mut mask = [0; 8];
                mask[..data.len()].copy_from_slice(data);
                NdofData::ButtonMask(u64::from_le_bytes(mask))
            }
            (Self::CombinedAxes, 0x1C | 0x1D, 2..=64) if data.len() % 2 == 0 => {
                let values = data.chunks_exact(2).map(|code| u16::from_le_bytes([code[0], code[1]])).collect();
                let codes = BoundedList::new(values).ok()?;
                if id == 0x1C { NdofData::ShortPress(codes) } else { NdofData::LongPress(codes) }
            }
            _ => return None,
        };
        Some(NdofReport { id, data: decoded })
    }
}

fn axes(bytes: &[u8]) -> Option<[i16; 3]> {
    let bytes = <[u8; 6]>::try_from(bytes).ok()?;
    Some([
        i16::from_le_bytes([bytes[0], bytes[1]]),
        i16::from_le_bytes([bytes[2], bytes[3]]),
        i16::from_le_bytes([bytes[4], bytes[5]]),
    ])
}
