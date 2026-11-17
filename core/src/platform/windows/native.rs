use std::{collections::VecDeque, ops::Deref, rc::{Rc, Weak}, sync::Arc};

use windows::Win32::Graphics::{
    Direct3D11::*,
    Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM,
};

use crate::{capture::frame::{FrameExtent, FrameMetadata}, protocol::FrameId};

use super::{ProbeError, hdr::ToneMap, pixels::Painter, readback::{Budget, Bytes, MAX_BYTES, image_bytes}, stage, unavailable};

const HISTORY: usize = 4;
// Four circulating frames, one copy destination and three distinct
// out-of-history holds. Lease capacity must leave the copy destination free.
const EXTRA_HOLDS: usize = 3;
const BASE_SLOTS: usize = HISTORY + 1;

#[derive(Clone)]
pub(super) struct NativeFrame {
    pub metadata: FrameMetadata,
    pub change: Option<f64>,
    pub change_sampled: bool,
    texture: Arc<ID3D11Texture2D>,
}

impl NativeFrame {
    pub fn copy_into(&self, context: &ID3D11DeviceContext, staging: &ID3D11Texture2D) {
        // Staging owns this copy. Later reuse of native history is ordered behind it.
        unsafe {
            context.CopyResource(staging, self.texture.as_ref());
            context.Flush();
        }
    }
}

pub(super) struct NativeImages {
    context: ID3D11DeviceContext,
    extent: FrameExtent,
    allocation_bytes: u64,
    pool: Vec<Arc<ID3D11Texture2D>>,
    history: VecDeque<NativeFrame>,
    staged: VecDeque<(Arc<ID3D11Texture2D>, FrameMetadata)>,
    evicted: Option<FrameMetadata>,
}

impl NativeImages {
    pub fn new(device: &ID3D11Device, extent: FrameExtent) -> Result<Self, ProbeError> {
        let frame_bytes = image_bytes(extent).ok_or_else(|| unavailable("The native frame extent overflows retention"))?;
        let slots = BASE_SLOTS + (MAX_BYTES / frame_bytes).min(EXTRA_HOLDS);
        let allocation_bytes = (frame_bytes as u64).checked_mul(slots as u64)
            .ok_or_else(|| unavailable("The native frame extent overflows the retention calculation"))?;
        let mut pool = Vec::with_capacity(slots);
        for _ in 0..slots {
            let texture = super::pixels::texture(
                device, extent.width.get(), extent.height.get(), DXGI_FORMAT_B8G8R8A8_UNORM,
                D3D11_BIND_SHADER_RESOURCE.0 as u32 | D3D11_BIND_RENDER_TARGET.0 as u32, false,
            )?;
            pool.push(Arc::new(texture));
        }
        let context = stage(unsafe { device.GetImmediateContext() }, "Get the native capture GPU context")?;
        Ok(Self { context, extent, allocation_bytes, pool, history: VecDeque::with_capacity(HISTORY), staged: VecDeque::with_capacity(HISTORY), evicted: None })
    }

    pub fn allocation_bytes(&self) -> u64 {
        self.allocation_bytes
    }

    pub fn context(&self) -> &ID3D11DeviceContext {
        &self.context
    }

    pub fn copy(&self, source: &ID3D11Texture2D, tone_map: Option<&ToneMap>) -> Result<Option<Arc<ID3D11Texture2D>>, ProbeError> {
        let Some(texture) = self.pool.iter().find(|texture| Arc::strong_count(texture) == 1) else {
            return Ok(None);
        };
        let region = D3D11_BOX {
            right: self.extent.width.get(), bottom: self.extent.height.get(), back: 1,
            ..Default::default()
        };
        // The source rectangle was checked against the WGC texture bounds. The copy is
        // ordered on the capture context before the frame lease is returned to WGC.
        let destination = tone_map.map_or(texture.as_ref(), |tone_map| &tone_map.scratch);
        unsafe {
            self.context.CopySubresourceRegion(destination, 0, 0, 0, 0, source, 0, Some(&region));
            self.context.Flush();
        }
        Ok(Some(Arc::clone(texture)))
    }

    pub fn finish_copy(&self, texture: &ID3D11Texture2D, painter: &Painter, tone_map: Option<&ToneMap>) -> Result<(), ProbeError> {
        if let Some(tone_map) = tone_map {
            tone_map.convert(painter, &self.context, texture, self.extent)?;
        }
        Ok(())
    }

    // Both queues share the existing history capacity and leave the spare
    // GPU copy destination available even when all extra holds are leased.
    pub fn staging_full(&self) -> bool { self.staged.len() == HISTORY }

    pub fn stage(&mut self, texture: Arc<ID3D11Texture2D>, metadata: FrameMetadata) {
        debug_assert!(!self.staging_full());
        if self.history.len() + self.staged.len() == HISTORY {
            self.evicted = self.history.pop_front().map(|frame| frame.metadata);
        }
        self.staged.push_back((texture, metadata));
    }

    pub fn staged(&self) -> Option<FrameMetadata> {
        self.staged.front().map(|(_, metadata)| metadata.clone())
    }

    pub fn take_staged(&mut self) -> Option<(Arc<ID3D11Texture2D>, FrameMetadata)> {
        self.staged.pop_front()
    }

    pub fn clear_staged(&mut self) { self.staged.clear(); }

    pub fn remember(&mut self, texture: Arc<ID3D11Texture2D>, metadata: FrameMetadata) {
        if self.history.len() == HISTORY {
            self.evicted = self.history.pop_front().map(|frame| frame.metadata);
        }
        self.history.push_back(NativeFrame { texture, metadata, change: None, change_sampled: false });
    }

    pub fn clear_history(&mut self) {
        self.history.clear();
        self.clear_staged();
        self.evicted = None;
    }

    pub fn history(&self) -> &VecDeque<NativeFrame> {
        &self.history
    }

    pub fn last_evicted(&self) -> Option<FrameMetadata> {
        self.evicted.clone()
    }

    pub fn lease(&self, id: FrameId) -> Option<NativeFrame> {
        self.find(id).cloned()
    }

    fn find(&self, id: FrameId) -> Option<&NativeFrame> {
        self.history.iter().find(|frame| frame.metadata.id == id)
    }

    pub fn record_change(&mut self, id: FrameId, change: Option<f64>) {
        if let Some(frame) = self.history.iter_mut().find(|frame| frame.metadata.id == id) {
            frame.change = change;
            frame.change_sampled = true;
        }
    }

    pub fn change(&self, id: FrameId) -> Option<f64> {
        self.find(id).and_then(|frame| frame.change)
    }
}

/// One allocation is charged once, even if several logical observations
/// share the same retained frame.
pub(super) struct HeldFrame {
    frame: NativeFrame,
    _bytes: Bytes,
}

impl Deref for HeldFrame {
    type Target = NativeFrame;
    fn deref(&self) -> &Self::Target { &self.frame }
}

pub(super) struct Holds {
    live: Vec<(FrameId, Weak<HeldFrame>)>,
    budget: Arc<Budget>,
}

impl Holds {
    pub fn new(budget: Arc<Budget>) -> Self {
        Self { live: Vec::with_capacity(EXTRA_HOLDS), budget }
    }

    pub fn retain(&mut self, frame: &NativeFrame) -> Option<Rc<HeldFrame>> {
        self.live.retain(|(_, frame)| frame.strong_count() != 0);
        if let Some(held) = self.live.iter().find_map(|(id, held)| {
            (*id == frame.metadata.id).then(|| held.upgrade()).flatten()
        }) {
            return Some(held);
        }
        if self.live.len() >= EXTRA_HOLDS {
            return None;
        }
        let bytes = self.budget.reserve(image_bytes(frame.metadata.extent)?).ok()?;
        let held = Rc::new(HeldFrame { frame: frame.clone(), _bytes: bytes });
        self.live.push((frame.metadata.id, Rc::downgrade(&held)));
        Some(held)
    }

    pub fn fits_readback(&self, frame: &NativeFrame) -> bool {
        image_bytes(frame.metadata.extent).and_then(|bytes| bytes.checked_mul(2))
            .is_some_and(|bytes| self.budget.fits(bytes))
    }
}
