mod security;

use std::{
    cell::UnsafeCell,
    ptr::{null, null_mut},
    time::Instant,
    sync::{Arc, Mutex, PoisonError, atomic::{AtomicUsize, Ordering}},
};

use windows_sys::Win32::{
    Foundation::{
        ERROR_BROKEN_PIPE, ERROR_FILE_NOT_FOUND, ERROR_IO_INCOMPLETE, ERROR_IO_PENDING, ERROR_MORE_DATA,
        ERROR_PIPE_BUSY, ERROR_PIPE_CONNECTED, ERROR_PIPE_NOT_CONNECTED, INVALID_HANDLE_VALUE, WAIT_OBJECT_0,
    },
    Storage::FileSystem::{
        CreateFileW, FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_FLAG_OVERLAPPED,
        FILE_READ_DATA, FILE_WRITE_DATA, OPEN_EXISTING, PIPE_ACCESS_DUPLEX,
        ReadFile, SECURITY_IDENTIFICATION, SECURITY_SQOS_PRESENT, SYNCHRONIZE, WriteFile,
    },
    System::{
        IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED},
        Pipes::{
            ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, GetNamedPipeClientProcessId,
            PIPE_READMODE_MESSAGE, PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_MESSAGE,
        },
        Threading::{ResetEvent, WaitForSingleObject},
    },
};

use crate::{
    deadline::{CLEANUP, Deadline, event, milliseconds},
    process::{Process, Sid},
    secret::erase,
    win::{Failure, Handle, Result, last_error},
};

pub(crate) const LIMIT: usize = 65536;
pub(crate) const PIPE_PREFIX: &str = r"\\.\pipe\ScreenFix.Elevation.v1.";
const CLIENT_ACCESS: u32 = FILE_READ_DATA | FILE_WRITE_DATA | SYNCHRONIZE;

static DRAINING: AtomicUsize = AtomicUsize::new(0);

pub(crate) fn available() -> Result<()> {
    if DRAINING.load(Ordering::Acquire) != 0 { return Err(Failure::Transport) }
    Ok(())
}

pub(crate) struct Name(Vec<u16>);

impl Name {
    pub(crate) fn hex(domain: &str, bytes: &[u8], role: &str) -> Self {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let mut name = Vec::with_capacity(PIPE_PREFIX.len() + domain.len() + bytes.len()*2 + role.len() + 1);
        name.extend(PIPE_PREFIX.encode_utf16());
        name.extend(domain.encode_utf16());
        for &byte in bytes {
            name.push(u16::from(HEX[usize::from(byte >> 4)]));
            name.push(u16::from(HEX[usize::from(byte & 15)]));
        }
        name.extend(role.encode_utf16());
        name.push(0);
        Self(name)
    }

    fn raw(&self) -> *const u16 { self.0.as_ptr() }
}

impl Drop for Name {
    fn drop(&mut self) { erase(&mut self.0); }
}

pub(crate) struct Pipe {
    handle: Arc<Handle>,
    server: bool,
    /// The idle operation of each direction, kept for reuse. Connect and the
    /// reads share the read direction.
    reader: Mutex<Option<Box<Pending>>>,
    writer: Mutex<Option<Box<Pending>>>,
}

impl Pipe {
    fn new(handle: Handle, server: bool) -> Self {
        Self { handle: Arc::new(handle), server, reader: Mutex::default(), writer: Mutex::default() }
    }

    pub(crate) fn bootstrap(name: &Name, medium: &Sid, server_logon: &Sid) -> Result<Self> {
        Self::server(name, security::Owner::Bootstrap { user: medium, server_logon })
    }

    pub(crate) fn collector(name: &Name, medium: &Sid) -> Result<Self> {
        Self::server(name, security::Owner::Collector(medium))
    }

    fn server(name: &Name, owner: security::Owner<'_>) -> Result<Self> {
        let descriptor = owner.descriptor()?;
        let attributes = security::attributes(&descriptor);
        // Duplex grants generic read/write to the server, including the
        // READ_CONTROL needed for security descriptor readback.
        let access = PIPE_ACCESS_DUPLEX | FILE_FLAG_OVERLAPPED | FILE_FLAG_FIRST_PIPE_INSTANCE;
        let mode = PIPE_TYPE_MESSAGE | PIPE_READMODE_MESSAGE | PIPE_REJECT_REMOTE_CLIENTS;
        let raw = unsafe {
            CreateNamedPipeW(name.raw(), access, mode, 1, LIMIT as u32, LIMIT as u32, 0, &attributes)
        };
        let handle = unsafe { Handle::take(raw, Failure::PipeConnection)? };
        owner.verify(&handle)?;
        Ok(Self::new(handle, true))
    }

    pub(crate) fn client(name: &Name, deadline: Deadline<'_>) -> Result<Self> {
        loop {
            deadline.check()?;
            let raw = unsafe {
                CreateFileW(name.raw(), CLIENT_ACCESS, 0, null(), OPEN_EXISTING,
                    FILE_FLAG_OVERLAPPED | SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION, null_mut())
            };
            if raw != INVALID_HANDLE_VALUE {
                let handle = unsafe { Handle::take(raw, Failure::PipeConnection)? };
                deadline.check()?;
                return Ok(Self::new(handle, false));
            }
            if !matches!(last_error(), ERROR_FILE_NOT_FOUND | ERROR_PIPE_BUSY) {
                return Err(Failure::PipeConnection);
            }
            deadline.pause()?;
        }
    }

    pub(crate) fn accept(&mut self, peer: &Process, deadline: Deadline<'_>) -> Result<()> {
        if !self.server { return Err(Failure::PipeConnection) }
        self.perform(Operation::Connect, 0, &[], &mut [], deadline)?;
        peer.ensure_live()?;
        let mut pid = 0;
        if unsafe { GetNamedPipeClientProcessId(self.handle.raw(), &mut pid) } == 0 || pid != peer.pid() {
            return Err(Failure::PipePeer);
        }
        peer.ensure_live()
    }

    pub(crate) fn write(&self, frame: &[u8], deadline: Deadline<'_>) -> Result<()> {
        if frame.is_empty() || frame.len() > LIMIT { return Err(Failure::Protocol) }
        let outcome = self.perform(Operation::Write, frame.len(), frame, &mut [], deadline)?;
        if outcome.transferred != frame.len() { return Err(Failure::Transport) }
        Ok(())
    }

    pub(crate) fn read_message(&self, buffer: &mut [u8], deadline: Deadline<'_>) -> Result<usize> {
        if !self.server || buffer.is_empty() || buffer.len() > LIMIT { return Err(Failure::Protocol) }
        let transferred = self.perform(Operation::Read, buffer.len(), &[], buffer, deadline)?.transferred;
        if transferred == 0 || transferred > buffer.len() {
            return Err(Failure::Transport);
        }
        Ok(transferred)
    }

    pub(crate) fn read_exact(&self, mut buffer: &mut [u8], deadline: Deadline<'_>) -> Result<()> {
        if self.server || buffer.len() > LIMIT { return Err(Failure::Protocol) }
        while !buffer.is_empty() {
            let transferred = self.perform(Operation::Read, buffer.len(), &[], &mut *buffer, deadline)?.transferred;
            if transferred == 0 || transferred > buffer.len() {
                return Err(Failure::Transport);
            }
            buffer = &mut buffer[transferred..];
        }
        Ok(())
    }

    pub(crate) fn expect_eof(&mut self, deadline: Deadline<'_>) -> Result<()> {
        if self.server { return Err(Failure::Protocol) }
        let outcome = self.perform(Operation::Eof, 1, &[], &mut [], deadline)?;
        if !outcome.closed { return Err(Failure::Protocol) }
        Ok(())
    }

    /// Read one frame into `buffer` and return its length. The server reads
    /// one message; the client reads `header` bytes, then the payload whose
    /// length `payload` derives from that header.
    pub(crate) fn read_frame(
        &self, buffer: &mut [u8], header: usize, payload: impl FnOnce(&[u8]) -> Result<usize>, deadline: Deadline<'_>,
    ) -> Result<usize> {
        if self.server { return self.read_message(buffer, deadline); }
        self.read_exact(&mut buffer[..header], deadline)?;
        let end = header + payload(&buffer[..header])?;
        self.read_exact(&mut buffer[header..end], deadline)?;
        Ok(end)
    }

    /// Run one operation of `size` bytes: a write sends `data`, a read
    /// copies what it received into `received` when that fits.
    fn perform(
        &self, kind: Operation, size: usize, data: &[u8], received: &mut [u8], deadline: Deadline<'_>,
    ) -> Result<Outcome> {
        deadline.check()?;
        let mut pending = self.idle(kind)?;
        pending.prepare(size, data);
        // A closed scope here is known to have submitted no kernel write.
        if let Err(failure) = deadline.check() {
            self.recycle(kind, pending);
            return Err(failure);
        }
        let transferred = match pending.begin(kind) {
            Ok(Submission::Complete) => pending.completion(),
            Ok(Submission::Pending) => match deadline.wait(&pending.event) {
                Ok(()) => pending.completion(),
                Err(Failure::ScopeClosed) if kind == Operation::Write => {
                    // CancelIoEx can race a completely delivered message.
                    // Only a successful completion of the whole write proves
                    // the position of the next message.
                    return match pending.cancel() {
                        Some(Ok(transferred)) if transferred == size => Ok(Outcome { transferred, closed: false }),
                        _ => Err(Failure::Transport),
                    };
                }
                Err(failure) => {
                    let _ = pending.cancel();
                    return Err(failure);
                }
            },
            // Nothing was submitted.
            Err(error) => {
                self.recycle(kind, pending);
                return match error {
                    ERROR_PIPE_CONNECTED if kind == Operation::Connect => {
                        deadline.check()?;
                        Ok(Outcome { transferred: 0, closed: false })
                    }
                    ERROR_BROKEN_PIPE | ERROR_PIPE_NOT_CONNECTED if kind == Operation::Eof => {
                        deadline.check()?;
                        Ok(Outcome { transferred: 0, closed: true })
                    }
                    ERROR_MORE_DATA => Err(Failure::Protocol),
                    _ => Err(kind.failure()),
                };
            }
        };
        let transferred = match transferred {
            Ok(transferred) => transferred,
            Err(ERROR_IO_INCOMPLETE | ERROR_IO_PENDING) => {
                let _ = pending.cancel();
                return Err(kind.failure());
            }
            // The operation completed with an error.
            Err(error) => {
                self.recycle(kind, pending);
                return match error {
                    ERROR_MORE_DATA => Err(Failure::Protocol),
                    ERROR_BROKEN_PIPE | ERROR_PIPE_NOT_CONNECTED if kind == Operation::Eof => {
                        deadline.check()?;
                        Ok(Outcome { transferred: 0, closed: true })
                    }
                    _ => Err(kind.failure()),
                };
            }
        };
        if let Some(received) = received.get_mut(..transferred) {
            received.copy_from_slice(&pending.buffer[..transferred]);
        }
        self.recycle(kind, pending);
        // Scoped runtime writes advance the lane on a successful transfer,
        // including when that visit closed between submission and completion.
        // The evidence owner separately decides whether it can certify it.
        if kind != Operation::Write || !deadline.has_scope() {
            deadline.check()?;
        }
        Ok(Outcome { transferred, closed: false })
    }

    fn slot(&self, kind: Operation) -> &Mutex<Option<Box<Pending>>> {
        if kind == Operation::Write { &self.writer } else { &self.reader }
    }

    fn idle(&self, kind: Operation) -> Result<Box<Pending>> {
        match self.slot(kind).lock().ok().and_then(|mut idle| idle.take()) {
            Some(pending) => Ok(pending),
            None => Pending::new(&self.handle),
        }
    }

    /// Keep an operation the kernel no longer references for the next one.
    fn recycle(&self, kind: Operation, mut pending: Box<Pending>) {
        erase(&mut pending.buffer);
        if let Ok(mut idle) = self.slot(kind).lock() {
            *idle = Some(pending);
        }
    }
}

impl Drop for Pipe {
    fn drop(&mut self) {
        // Idle operations hold no kernel I/O; only a deferred drain keeps
        // another reference to the handle.
        for idle in [&mut self.reader, &mut self.writer] {
            *idle.get_mut().unwrap_or_else(PoisonError::into_inner) = None;
        }
        if self.server && Arc::strong_count(&self.handle) != 1 {
            // A deferred kernel cancellation still holds the handle. Revoke the
            // connection now. On a normal close, preserve already queued bytes.
            unsafe { DisconnectNamedPipe(self.handle.raw()) };
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Operation { Connect, Read, Eof, Write }

enum Submission { Complete, Pending }

impl Operation {
    fn failure(self) -> Failure {
        match self {
            Self::Connect => Failure::PipeConnection,
            Self::Read | Self::Eof | Self::Write => Failure::Transport,
        }
    }
}

struct Outcome {
    transferred: usize,
    closed: bool,
}

struct Pending {
    handle: Arc<Handle>,
    overlap: UnsafeCell<OVERLAPPED>,
    event: Handle,
    buffer: Vec<u8>,
}

// A submitted operation is boxed at a stable address. Only its cleanup owner
// uses it after handoff. The kernel may complete it on another thread. An
// idle operation, kept by its pipe for reuse, references no kernel I/O.
unsafe impl Send for Pending {}

impl Pending {
    fn new(handle: &Arc<Handle>) -> Result<Box<Self>> {
        let done = event()?;
        Ok(Box::new(Self {
            handle: Arc::clone(handle),
            overlap: UnsafeCell::new(OVERLAPPED { hEvent: done.raw(), ..Default::default() }),
            event: done,
            buffer: Vec::new(),
        }))
    }

    /// Reset an idle operation for `size` bytes starting with `data`. Its
    /// buffer was erased when it became idle.
    fn prepare(&mut self, size: usize, data: &[u8]) {
        self.buffer.resize(size, 0);
        self.buffer[..data.len()].copy_from_slice(data);
        *self.overlap.get_mut() = OVERLAPPED { hEvent: self.event.raw(), ..Default::default() };
        unsafe { ResetEvent(self.event.raw()) };
    }

    fn begin(&mut self, kind: Operation) -> std::result::Result<Submission, u32> {
        let raw = self.handle.raw();
        let size = self.buffer.len() as u32;
        let overlap = self.overlap.get();
        let result = unsafe {
            match kind {
                Operation::Connect => ConnectNamedPipe(raw, overlap),
                Operation::Read | Operation::Eof => ReadFile(raw, self.buffer.as_mut_ptr(), size, null_mut(), overlap),
                Operation::Write => WriteFile(raw, self.buffer.as_ptr(), size, null_mut(), overlap),
            }
        };
        if result != 0 { return Ok(Submission::Complete) }
        match last_error() {
            ERROR_IO_PENDING => Ok(Submission::Pending),
            error => Err(error),
        }
    }

    fn completion(&self) -> std::result::Result<usize, u32> {
        let mut bytes = 0;
        if unsafe { GetOverlappedResult(self.handle.raw(), self.overlap.get(), &mut bytes, 0) } == 0 {
            return Err(last_error());
        }
        Ok(bytes as usize)
    }

    fn cancel(self: Box<Self>) -> Option<std::result::Result<usize, u32>> {
        let until = Instant::now() + CLEANUP;
        self.request_cancel();
        self.drain_cancelled(until)
    }

    fn request_cancel(&self) {
        unsafe { CancelIoEx(self.handle.raw(), self.overlap.get()) };
    }

    fn drain_cancelled(self: Box<Self>, until: Instant) -> Option<std::result::Result<usize, u32>> {
        let completion = self.completion();
        if !matches!(completion, Err(ERROR_IO_INCOMPLETE | ERROR_IO_PENDING)) {
            return Some(completion);
        }
        let remaining = until.saturating_duration_since(Instant::now());
        if unsafe { WaitForSingleObject(self.event.raw(), milliseconds(remaining)) } == WAIT_OBJECT_0 {
            let completion = self.completion();
            if !matches!(completion, Err(ERROR_IO_INCOMPLETE | ERROR_IO_PENDING)) {
                return Some(completion);
            }
        }

        // Retain the allocation until the kernel acknowledges cancellation. Refuse
        // another attachment in this process while an abandoned operation exists.
        DRAINING.fetch_add(1, Ordering::AcqRel);
        let owner = Deferred(std::mem::ManuallyDrop::new(self));
        let _ = std::thread::Builder::new().name("elevated-io-drain".into()).spawn(move || owner.drain());
        None
    }
}

impl Drop for Pending {
    fn drop(&mut self) { erase(&mut self.buffer); }
}

struct Deferred(std::mem::ManuallyDrop<Box<Pending>>);

impl Deferred {
    fn drain(mut self) {
        let mut bytes = 0;
        let pending: &Pending = &self.0;
        let result = unsafe { GetOverlappedResult(pending.handle.raw(), pending.overlap.get(), &mut bytes, 1) };
        if result == 0 && matches!(last_error(), ERROR_IO_INCOMPLETE | ERROR_IO_PENDING) {
            return;
        }
        unsafe { std::mem::ManuallyDrop::drop(&mut self.0) };
        DRAINING.fetch_sub(1, Ordering::AcqRel);
    }
}
