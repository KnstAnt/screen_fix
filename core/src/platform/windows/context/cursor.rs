//! Samples Windows cursor state on the context owner. Custom cursor pixels are
//! copied out of native bitmaps, then a bounded PNG owner publishes assets.

use std::{
    fs::{self, OpenOptions},
    io,
    mem::size_of,
    path::Path,
    ptr::null_mut,
    sync::{
        Arc, atomic::{AtomicBool, Ordering::{Acquire, Release}},
        mpsc::{Receiver, SyncSender, sync_channel},
    },
    thread,
    time::{Duration, Instant},
};

use serde_json::{Value, json};
use windows_sys::Win32::{
    Graphics::Gdi::{BITMAP, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS, DeleteObject, GetDC, GetDIBits, GetObjectW, HBITMAP, ReleaseDC},
    UI::WindowsAndMessaging::{
        CURSOR_SHOWING, CURSOR_SUPPRESSED, CURSORINFO, GetCursorInfo, GetIconInfo, HCURSOR, ICONINFO,
        IDC_APPSTARTING, IDC_ARROW, IDC_CROSS, IDC_HAND, IDC_HELP, IDC_IBEAM, IDC_NO, IDC_SIZEALL,
        IDC_SIZENESW, IDC_SIZENS, IDC_SIZENWSE, IDC_SIZEWE, IDC_UPARROW, IDC_WAIT, LoadCursorW,
    },
};

use crate::{package::{ArtifactKind, ArtifactRef, ImageDigest, metadata::AttemptLayout},
    protocol::{ClockOrigin, PhysicalPoint, QpcTick}};
use super::super::{ProbeError, clock, mp4::io_error};

const POLL: Duration = Duration::from_millis(50);
const MAX_EDGE: usize = 256;
const QUEUE: usize = 4;

#[derive(Clone, Copy, PartialEq, Eq)]
struct State {
    handle: usize,
    visible: bool,
    suppressed: bool,
    approved: bool,
}

pub(super) struct Tracker {
    previous: Option<State>,
    standards: Vec<(usize, &'static str)>,
    next_poll: Instant,
    sender: Option<SyncSender<Job>>,
    owner: Option<thread::JoinHandle<()>>,
    returned: Receiver<Completion>,
    pending: Vec<Observed>,
    cancelled: Arc<AtomicBool>,
}

impl Tracker {
    pub fn start(layout: &AttemptLayout) -> Result<Self, ProbeError> {
        let layout = layout.clone();
        let (sender, jobs) = sync_channel::<Job>(QUEUE);
        let (returning, returned) = sync_channel(QUEUE);
        let cancelled = Arc::new(AtomicBool::new(false));
        let worker_cancelled = Arc::clone(&cancelled);
        let owner = thread::Builder::new().name("context-cursor-assets".into()).spawn(move || {
            while let Ok(job) = jobs.recv() {
                if worker_cancelled.load(Acquire) { break; }
                let resolved = write_assets(&layout, job.image, &worker_cancelled).ok();
                let _ = returning.try_send(Completion { observed: job.observed, resolved, kind: ResultKind::Asset });
            }
        }).map_err(|error| io_error("Start cursor asset owner", error))?;
        let standards = [
            (IDC_ARROW, "arrow"), (IDC_IBEAM, "ibeam"), (IDC_WAIT, "wait"), (IDC_CROSS, "crosshair"),
            (IDC_UPARROW, "up_arrow"), (IDC_SIZENWSE, "size_nwse"), (IDC_SIZENESW, "size_nesw"),
            (IDC_SIZEWE, "size_we"), (IDC_SIZENS, "size_ns"), (IDC_SIZEALL, "size_all"),
            (IDC_NO, "no"), (IDC_HAND, "hand"), (IDC_APPSTARTING, "app_starting"), (IDC_HELP, "help"),
        ].into_iter().filter_map(|(id, name)| {
            let cursor = unsafe { LoadCursorW(null_mut(), id) };
            (!cursor.is_null()).then_some((cursor as usize, name))
        }).collect();
        Ok(Self { previous: None, standards, next_poll: Instant::now(), sender: Some(sender), owner: Some(owner), returned, pending: Vec::with_capacity(QUEUE), cancelled })
    }

    pub fn reset_observation(&mut self) {
        self.previous = None;
        self.next_poll = Instant::now();
    }

    pub fn current_hint(&self) -> Option<Value> {
        let state = self.previous.filter(|state| state.approved)?;
        let shape = self.standards.iter().find(|(handle, _)| *handle == state.handle).map(|(_, name)| *name);
        Some(json!({
            "shape": shape, "custom": state.handle != 0 && shape.is_none(),
            "visible": state.visible, "suppressed": state.suppressed,
        }))
    }

    pub fn sample(&mut self, force: bool, approved: impl FnOnce(PhysicalPoint) -> bool) -> Option<Completion> {
        let now = Instant::now();
        if !force && now < self.next_poll {
            return None;
        }
        self.next_poll = now + POLL;
        let mut info = CURSORINFO { cbSize: size_of::<CURSORINFO>() as u32, ..Default::default() };
        if unsafe { GetCursorInfo(&mut info) } == 0 {
            return None;
        }
        let point = PhysicalPoint { x: info.ptScreenPos.x, y: info.ptScreenPos.y };
        let approved = approved(point);
        let state = State {
            handle: if approved { info.hCursor as usize } else { 0 },
            visible: info.flags & CURSOR_SHOWING != 0, suppressed: info.flags & CURSOR_SUPPRESSED != 0, approved,
        };
        if self.previous == Some(state) {
            return None;
        }
        self.previous = Some(state);
        let at = clock::qpc().ok()?;
        let standard = self.standards.iter().find(|(handle, _)| *handle == state.handle).map(|(_, name)| *name);
        let observed = Observed { at, state, standard };
        if !approved || standard.is_some() || state.handle == 0 || !state.visible {
            return Some(Completion { observed, resolved: None, kind: ResultKind::State { asset_pending: false } });
        }
        if self.pending.len() < QUEUE && let Some(image) = Pixels::read(info.hCursor)
            && self.sender.as_ref().is_some_and(|sender| sender.try_send(Job { observed, image }).is_ok())
        {
            self.pending.push(observed);
            return Some(Completion { observed, resolved: None, kind: ResultKind::State { asset_pending: true } });
        }
        Some(Completion { observed, resolved: None, kind: ResultKind::State { asset_pending: false } })
    }

    pub fn results(&mut self) -> Vec<Completion> {
        let mut results = Vec::new();
        for _ in 0..QUEUE {
            match self.returned.try_recv() {
                Ok(completion) => {
                    self.pending.retain(|entry| entry.at != completion.observed.at);
                    results.push(completion);
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => break,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    results.extend(self.pending.drain(..).map(|observed| Completion { observed, resolved: None, kind: ResultKind::Asset }));
                    break;
                }
            }
        }
        results
    }

    pub fn close(&mut self) -> Vec<Completion> {
        let before = Instant::now() + Duration::from_millis(100);
        let mut results = self.results();
        while !self.pending.is_empty() && Instant::now() < before {
            let remaining = before.saturating_duration_since(Instant::now());
            let Ok(result) = self.returned.recv_timeout(remaining) else { break };
            self.pending.retain(|entry| entry.at != result.observed.at);
            results.push(result);
        }
        self.cancelled.store(true, Release);
        results.extend(self.pending.drain(..).map(|observed| Completion { observed, resolved: None, kind: ResultKind::Asset }));
        results
    }
}

impl Drop for Tracker {
    fn drop(&mut self) {
        self.cancelled.store(true, Release);
        drop(self.sender.take());
        if let Some(owner) = self.owner.take() {
            let _ = owner.join();
        }
    }
}

#[derive(Clone, Copy)]
struct Observed {
    at: QpcTick,
    state: State,
    standard: Option<&'static str>,
}

struct Job {
    observed: Observed,
    image: Pixels,
}

enum ResultKind {
    State { asset_pending: bool },
    Asset,
}

pub(super) struct Completion {
    observed: Observed,
    resolved: Option<Assets>,
    kind: ResultKind,
}

impl Completion {
    pub fn received(&self) -> QpcTick {
        self.observed.at
    }

    pub fn row(self, origin: ClockOrigin) -> Value {
        let observation = self.observed;
        if !observation.state.approved {
            return json!({ "kind": "cursor", "approved": false });
        }
        let at = origin.time(observation.at).ok();
        match self.kind {
            ResultKind::Asset => json!({
                "kind": "cursor_asset", "cursor_observed_t_us": at,
                "available": self.resolved.is_some(), "image": self.resolved.map(|assets| json!({
                    "png": assets.png.relative_uri(),
                    "xor_mask": assets.xor_mask.map(|mask| mask.relative_uri()),
                    "hotspot": assets.hotspot,
                })),
            }),
            ResultKind::State { asset_pending } => json!({
                "kind": "cursor", "approved": true, "shape": observation.standard,
                "visible": observation.state.visible, "suppressed": observation.state.suppressed,
                "cursor_observed_t_us": at, "custom_asset_pending": asset_pending,
            }),
        }
    }
}

struct Assets {
    png: ArtifactRef,
    xor_mask: Option<ArtifactRef>,
    hotspot: (u32, u32),
}

struct Pixels {
    width: u32,
    height: u32,
    hotspot: (u32, u32),
    rgba: Vec<u8>,
    invert: Option<Vec<u8>>,
}

impl Pixels {
    fn read(cursor: HCURSOR) -> Option<Self> {
        let mut icon = ICONINFO::default();
        if unsafe { GetIconInfo(cursor, &mut icon) } == 0 {
            return None;
        }
        let mask = Bitmap(icon.hbmMask);
        let color = Bitmap(icon.hbmColor);
        let dc = DesktopDc::open()?;
        let (width, mask_height) = mask.size()?;
        let monochrome = color.0.is_null();
        let height = if monochrome {
            if mask_height % 2 != 0 { return None; }
            mask_height / 2
        } else {
            let (color_width, color_height) = color.size()?;
            if (color_width, color_height) != (width, mask_height) { return None; }
            color_height
        };
        if width == 0 || height == 0 || width > MAX_EDGE || height > MAX_EDGE
            || icon.xHotspot as usize >= width || icon.yHotspot as usize >= height
        {
            return None;
        }
        let and_xor = mask.bgra(&dc, width, mask_height)?;
        let pixels = width * height;
        let mut rgba = vec![0; pixels * 4];
        let mut invert = None;
        if monochrome {
            for (index, output) in rgba.chunks_exact_mut(4).enumerate() {
                let and = and_xor[index * 4] != 0;
                let xor = and_xor[(pixels + index) * 4] != 0;
                match (and, xor) {
                    (false, false) => output.copy_from_slice(&[0, 0, 0, 255]),
                    (false, true) => output.copy_from_slice(&[255; 4]),
                    (true, false) => {}
                    (true, true) => {
                        let data = invert.get_or_insert_with(|| vec![0u8; pixels * 4]);
                        data[index * 4..index * 4 + 4].copy_from_slice(&[255; 4]);
                    }
                }
            }
        } else {
            let bgra = color.bgra(&dc, width, height)?;
            let has_alpha = bgra.chunks_exact(4).any(|pixel| pixel[3] != 0);
            for (index, (source, target)) in bgra.chunks_exact(4).zip(rgba.chunks_exact_mut(4)).enumerate() {
                if has_alpha {
                    let alpha = u32::from(source[3]);
                    if alpha != 0 {
                        let unmultiply = |channel: u8| ((u32::from(channel) * 255 + alpha / 2) / alpha).min(255) as u8;
                        target.copy_from_slice(&[unmultiply(source[2]), unmultiply(source[1]), unmultiply(source[0]), source[3]]);
                    }
                } else if and_xor[index * 4] == 0 {
                    target.copy_from_slice(&[source[2], source[1], source[0], 255]);
                }
            }
        }
        Some(Self {
            width: width as u32, height: height as u32, hotspot: (icon.xHotspot, icon.yHotspot), rgba, invert,
        })
    }
}

struct Bitmap(HBITMAP);

impl Bitmap {
    fn size(&self) -> Option<(usize, usize)> {
        if self.0.is_null() {
            return None;
        }
        let mut bitmap = BITMAP::default();
        let dimensions = unsafe { GetObjectW(self.0, size_of::<BITMAP>() as i32, (&mut bitmap as *mut BITMAP).cast()) };
        if dimensions != size_of::<BITMAP>() as i32 {
            return None;
        }
        Some((usize::try_from(bitmap.bmWidth).ok()?, usize::try_from(bitmap.bmHeight).ok()?))
    }

    fn bgra(&self, dc: &DesktopDc, width: usize, height: usize) -> Option<Vec<u8>> {
        if width > MAX_EDGE || height > MAX_EDGE * 2 {
            return None;
        }
        let mut format = BITMAPINFO { bmiHeader: BITMAPINFOHEADER {
            biSize: size_of::<BITMAPINFOHEADER>() as u32, biWidth: width as i32, biHeight: -(height as i32),
            biPlanes: 1, biBitCount: 32, biCompression: BI_RGB, ..Default::default()
        }, ..Default::default() };
        let mut pixels = vec![0; width * height * 4];
        let read = unsafe {
            GetDIBits(dc.0, self.0, 0, height as u32, pixels.as_mut_ptr().cast(), &mut format, DIB_RGB_COLORS)
        };
        (read == height as i32).then_some(pixels)
    }
}

impl Drop for Bitmap {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { DeleteObject(self.0) };
        }
    }
}

struct DesktopDc(windows_sys::Win32::Graphics::Gdi::HDC);

impl DesktopDc {
    fn open() -> Option<Self> {
        let dc = unsafe { GetDC(null_mut()) };
        (!dc.is_null()).then_some(Self(dc))
    }
}

impl Drop for DesktopDc {
    fn drop(&mut self) {
        unsafe { ReleaseDC(null_mut(), self.0) };
    }
}

fn write_assets(layout: &AttemptLayout, image: Pixels, cancelled: &AtomicBool) -> io::Result<Assets> {
    let png = write_asset(layout, image.width, image.height, &image.rgba, cancelled)?;
    let xor_mask = image.invert.map(|mask| write_asset(layout, image.width, image.height, &mask, cancelled)).transpose()?;
    Ok(Assets { png, xor_mask, hotspot: image.hotspot })
}

fn write_asset(layout: &AttemptLayout, width: u32, height: u32, rgba: &[u8], cancelled: &AtomicBool) -> io::Result<ArtifactRef> {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"screen_fix/cursor/rgba8");
    hasher.update(&width.to_le_bytes());
    hasher.update(&height.to_le_bytes());
    hasher.update(rgba);
    let kind = ArtifactKind::CursorImage(ImageDigest(*hasher.finalize().as_bytes()));
    let destination = layout.path(kind);
    let publish = match destination.metadata() {
        Ok(meta) if meta.is_file() && meta.len() != 0 => false,
        Ok(_) => return Err(io::ErrorKind::InvalidData.into()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => true,
        Err(error) => return Err(error),
    };
    if publish {
        let pending = destination.with_extension("pending");
        let encoded = encode(&pending, width, height, rgba).and_then(|()| {
            if cancelled.load(Acquire) {
                Err(io::Error::from(io::ErrorKind::Interrupted))
            } else {
                fs::rename(&pending, &destination)
            }
        });
        if encoded.is_err() {
            let _ = fs::remove_file(pending);
        }
        encoded?;
    }
    Ok(ArtifactRef { attempt: layout.id().clone(), kind })
}

fn encode(path: &Path, width: u32, height: u32, rgba: &[u8]) -> io::Result<()> {
    let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
    let mut encoder = png::Encoder::new(&mut file, width, height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header().map_err(io::Error::other)?;
    writer.write_image_data(rgba).map_err(io::Error::other)?;
    writer.finish().map_err(io::Error::other)?;
    file.sync_all()
}
