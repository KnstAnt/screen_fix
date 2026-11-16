//! Unnamed Windows input channel. One mapped endpoint per process.
//!
//! Each direction has one producer and one consumer. A release-store of the
//! write cursor publishes a complete slot. A release-store of the read cursor
//! returns the slot only after decoding. The peer acquires that cursor before
//! touching the slot. A process exit can leave an uncommitted write, which its
//! peer will never read. Mapping ownership does not cross native threads.

use std::{
    cell::UnsafeCell,
    io,
    mem::size_of,
    os::windows::io::{AsHandle, AsRawHandle, BorrowedHandle, FromRawHandle, OwnedHandle},
    ptr::{NonNull, null},
    sync::atomic::{AtomicU32, Ordering::{Acquire, Relaxed, Release}},
};

use windows_sys::Win32::{
    Foundation::INVALID_HANDLE_VALUE,
    System::Memory::{
        CreateFileMappingW, FILE_MAP_WRITE, MEMORY_MAPPED_VIEW_ADDRESS, MapViewOfFile,
        PAGE_READWRITE, UnmapViewOfFile,
    },
};

use super::{CoreToHelper, HelperToCore, MAX_PACKET_BYTES, PacketError};

const COMMAND_SLOTS: usize = 8;
const EVIDENCE_SLOTS: usize = 256;
const LAYOUT_VERSION: u32 = 2;
const STATUS_SLOTS: usize = 4;

#[repr(C)]
struct Slot {
    length: u32,
    packet: [u8; MAX_PACKET_BYTES],
}

#[repr(C)]
struct Ring<const N: usize> {
    read: AtomicU32,
    write: AtomicU32,
    slots: [UnsafeCell<Slot>; N],
}

#[repr(C)]
struct Layout {
    version: AtomicU32,
    // Stop is independent of the command and evidence queue capacities.
    stop: AtomicU32,
    environment: AtomicU32,
    commands: Ring<COMMAND_SLOTS>,
    status: Ring<STATUS_SLOTS>,
    evidence: Ring<EVIDENCE_SLOTS>,
}

struct View(NonNull<Layout>);

impl View {
    fn open(mapping: BorrowedHandle<'_>) -> io::Result<Self> {
        // The handle denotes a page-file mapping, independently referenced by
        // its view after MapViewOfFile. The fixed view length checks its extent.
        let view = unsafe {
            MapViewOfFile(mapping.as_raw_handle(), FILE_MAP_WRITE, 0, 0, size_of::<Layout>())
        };
        let pointer = NonNull::new(view.Value.cast()).ok_or_else(io::Error::last_os_error)?;
        Ok(Self(pointer))
    }

    fn layout(&self) -> &Layout {
        // Page alignment exceeds Layout alignment. The mapping starts
        // zero-filled and all fields accept zero; only atomics and the slots'
        // interior cells are mutated for the entire lifetime of a view.
        unsafe { self.0.as_ref() }
    }
}

impl Drop for View {
    fn drop(&mut self) {
        // This view is uniquely owned, independent of the mapping handle.
        unsafe { UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS { Value: self.0.as_ptr().cast() }) };
    }
}

pub struct RecorderEndpoint {
    view: View,
    mapping: OwnedHandle,
}

pub struct HelperEndpoint(View);

impl RecorderEndpoint {
    pub fn create() -> io::Result<Self> {
        // No name or writable file is associated with the mapping. The
        // launcher passes a duplicate exclusively through its handle list.
        let handle = unsafe {
            CreateFileMappingW(INVALID_HANDLE_VALUE, null(), PAGE_READWRITE, 0,
                size_of::<Layout>() as u32, null())
        };
        if handle.is_null() {
            return Err(io::Error::last_os_error());
        }
        let mapping = unsafe { OwnedHandle::from_raw_handle(handle) };
        let view = View::open(mapping.as_handle())?;
        view.layout().version.store(LAYOUT_VERSION, Release);
        Ok(Self { view, mapping })
    }

    pub fn mapping_handle(&self) -> BorrowedHandle<'_> {
        self.mapping.as_handle()
    }

    pub fn configure(&mut self, record: &CoreToHelper) -> Result<bool, PacketError> {
        self.view.layout().commands.push(|bytes| record.encode(bytes))
    }

    pub fn receive_status(&mut self) -> Result<Option<HelperToCore>, PacketError> {
        self.view.layout().status.pop(HelperToCore::decode)
    }

    pub fn receive(&mut self) -> Result<Option<HelperToCore>, PacketError> {
        self.view.layout().evidence.pop(HelperToCore::decode)
    }

    pub fn request_stop(&mut self) {
        self.view.layout().stop.store(1, Release);
    }

    pub fn notify_environment(&mut self, bits: u32) {
        self.view.layout().environment.fetch_or(bits, Release);
    }
}

impl HelperEndpoint {
    /// # Safety
    /// The handle must be the inherited channel from the recorder launch,
    /// with exactly one active helper endpoint and one recorder endpoint.
    pub unsafe fn attach(mapping: BorrowedHandle<'_>) -> io::Result<Self> {
        let view = View::open(mapping)?;
        if view.layout().version.load(Acquire) != LAYOUT_VERSION {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "Input channel layout version differs."));
        }
        Ok(Self(view))
    }

    pub fn receive_configuration(&mut self) -> Result<Option<CoreToHelper>, PacketError> {
        self.0.layout().commands.pop(CoreToHelper::decode)
    }

    pub fn send_status(&mut self, record: &HelperToCore) -> Result<bool, PacketError> {
        if !matches!(record, HelperToCore::Ready(_) | HelperToCore::Failed(_)) {
            return Err(PacketError::InvalidRecord("Input IPC status requires readiness or failure."));
        }
        self.0.layout().status.push(|bytes| record.encode(bytes))
    }

    pub fn send(&mut self, record: &HelperToCore) -> Result<bool, PacketError> {
        self.0.layout().evidence.push(|bytes| record.encode(bytes))
    }

    pub fn stop_requested(&self) -> bool {
        self.0.layout().stop.load(Acquire) != 0
    }

    pub fn take_environment(&mut self) -> u32 {
        self.0.layout().environment.swap(0, Acquire)
    }
}

impl<const N: usize> Ring<N> {
    fn push(&self, encode: impl FnOnce(&mut [u8]) -> Result<usize, PacketError>) -> Result<bool, PacketError> {
        let write = self.write.load(Relaxed);
        let occupied = write.wrapping_sub(self.read.load(Acquire));
        if occupied >= N as u32 {
            return if occupied == N as u32 {
                Ok(false)
            } else {
                Err(PacketError::InvalidRecord("Input IPC write cursor is invalid."))
            };
        }
        // This cursor denotes a returned slot. The consumer cannot read it
        // until the release-store publishes the newly encoded record.
        let slot = unsafe { &mut *self.slots[write as usize % N].get() };
        slot.length = encode(&mut slot.packet)? as u32;
        self.write.store(write.wrapping_add(1), Release);
        Ok(true)
    }

    fn pop<T>(&self, decode: impl FnOnce(&[u8]) -> Result<T, PacketError>) -> Result<Option<T>, PacketError> {
        let read = self.read.load(Relaxed);
        let available = self.write.load(Acquire).wrapping_sub(read);
        if available == 0 {
            return Ok(None);
        }
        if available > N as u32 {
            return Err(PacketError::InvalidRecord("Input IPC read cursor is invalid."));
        }
        // This cursor denotes a published slot. The producer can reuse it
        // only after the release-store below returns it to the free prefix.
        let slot = unsafe { &mut *self.slots[read as usize % N].get() };
        let decoded = slot.packet.get_mut(..slot.length as usize)
            .ok_or(PacketError::InvalidLength).and_then(|bytes| {
                let result = decode(bytes);
                // Clear consumed private evidence before returning the slot.
                bytes.fill(0);
                result
            });
        slot.length = 0;
        self.read.store(read.wrapping_add(1), Release);
        decoded.map(Some)
    }
}
