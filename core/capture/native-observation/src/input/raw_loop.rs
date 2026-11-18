use std::{
    convert::Infallible,
    os::windows::io::{AsRawHandle, OwnedHandle},
    ptr::null_mut,
    sync::{
        Arc,
        atomic::{AtomicU32, Ordering::{AcqRel, Acquire, Release}},
        mpsc::{self, Receiver, SyncSender},
    },
    thread,
    time::Duration,
};

use screen_fix_protocol::{
    DeviceId, MessageTickMs, QpcTick, SourceSequence,
    ipc::{DeviceDescriptor, EnvironmentChange, EnvironmentState, HelperFailureStage, HelperToCore, Producer, ProducerProgress, Receipt},
};
use windows_sys::Win32::{
    Foundation::HANDLE,
    System::Threading::{GetCurrentThreadId, SetEvent},
    UI::WindowsAndMessaging::{
        DispatchMessageW, GIDC_ARRIVAL, GIDC_REMOVAL, GetMessageW, GetSystemMetrics, KillTimer,
        MSG, PostThreadMessageW, SM_SWAPBUTTON, SetTimer, WM_APP, WM_INPUT, WM_INPUT_DEVICE_CHANGE,
        WM_QUIT, WM_TIMER,
    },
};

use super::{
    CaptureScope, NativeError, device, last_error,
    loss::{self, Losses, PendingStatus},
    raw::{RawRecord, RawWindow, qpc},
};

/// The Raw Input thread's identity and the optional collections it registered.
type RawReady = (u32, [bool; screen_fix_protocol::ipc::OptionalCollection::ALL.len()]);

const CHECK_TIMER: usize = 1;
const PROGRESS_TIMER: usize = 2;
const WAKE: u32 = WM_APP + 1;
const STOP: u32 = 1 << 31;
const RAW_QUEUE_CAPACITY: usize = 4096;
const RAW_THREAD: [Producer; 3] = [Producer::DeviceNotifications, Producer::RawInput, Producer::OptionalHid];
const DEVICE_REQUESTS: usize = 64;

pub(super) struct RawOwner {
    thread_id: u32,
    pending: Arc<AtomicU32>,
    owner: thread::JoinHandle<RawResult>,
    governed: bool,
    optional: [bool; screen_fix_protocol::ipc::OptionalCollection::ALL.len()],
    pub(super) records: Receiver<HelperToCore>,
}

pub(super) struct RawExit {
    pub(super) result: Result<(), NativeError>,
    pub(super) records: Receiver<HelperToCore>,
    pub(super) losses: Losses,
    pub(super) status: PendingStatus,
}

struct RawResult {
    result: Result<(), NativeError>,
    losses: Losses,
    status: PendingStatus,
}

impl RawOwner {
    pub(super) fn start(data_wake: OwnedHandle, frequency: i64, pen_hid: bool, scope: Option<Arc<CaptureScope>>) -> Result<Self, NativeError> {
        let governed = scope.is_some();
        let (ready, initialized) = mpsc::sync_channel(1);
        let (ingress, records) = mpsc::sync_channel(RAW_QUEUE_CAPACITY);
        let pending = Arc::new(AtomicU32::new(0));
        let thread_pending = Arc::clone(&pending);
        let owner = thread::Builder::new().name("input-raw".into()).spawn(move || {
            let mut output = Evidence { ingress, data_wake, losses: Losses::default(), status: PendingStatus::default() };
            let result = raw_loop(&mut output, &thread_pending, &ready, frequency / 80, pen_hid, scope.as_deref());
            if let Err(error) = result {
                let _ = ready.try_send(Err(error));
                signal(&output.data_wake);
            }
            RawResult { result, losses: output.losses, status: output.status }
        }).map_err(|error| NativeError {
            stage: HelperFailureStage::Startup,
            code: error.raw_os_error(),
            raw_input_registration: None,
        })?;
        let (thread_id, optional) = match initialized.recv_timeout(Duration::from_secs(2)) {
            Ok(Ok(ready)) => ready,
            result => {
                drop(initialized);
                if governed { let _ = owner.join(); }
                return match result {
                    Ok(Err(error)) => Err(error),
                    _ => Err(NativeError::without_code(HelperFailureStage::MessageLoop)),
                };
            }
        };
        Ok(Self { thread_id, pending, owner, records, governed, optional })
    }

    /// Optional collections registered at startup, in `OptionalCollection::ALL` order.
    pub(super) fn optional(&self) -> [bool; screen_fix_protocol::ipc::OptionalCollection::ALL.len()] {
        self.optional
    }

    pub(super) fn ended(&self) -> bool {
        self.owner.is_finished()
    }

    pub(super) fn notify(&self, changes: u32) -> Result<(), NativeError> {
        self.pending.fetch_or(changes, Release);
        self.post(WAKE)
    }

    pub(super) fn finish(self, changes: u32) -> Result<RawExit, NativeError> {
        if !self.owner.is_finished() {
            self.pending.fetch_or(STOP | changes, Release);
            if let Err(error) = self.post(WM_QUIT) {
                if self.governed { let _ = self.owner.join(); }
                return Err(error);
            }
        }
        let RawResult { result, losses, status } = self.owner.join()
            .map_err(|_| NativeError::without_code(HelperFailureStage::MessageLoop))?;
        Ok(RawExit { result, losses, records: self.records, status })
    }

    fn post(&self, message: u32) -> Result<(), NativeError> {
        if unsafe { PostThreadMessageW(self.thread_id, message, 0, 0) } == 0 {
            return Err(last_error(HelperFailureStage::MessageLoop));
        }
        Ok(())
    }
}

struct Evidence {
    ingress: SyncSender<HelperToCore>,
    data_wake: OwnedHandle,
    losses: Losses,
    /// Status records waiting for queue room.
    status: PendingStatus,
}

impl Evidence {
    fn offer(&mut self, record: HelperToCore) {
        let record = match self.status.defer(record) {
            Ok(()) => return self.flush_status(),
            Err(record) => record,
        };
        if let Some(producer) = loss::producer(&record)
            && !self.flush_loss(producer)
        {
            self.losses.drop_record(&record);
            return;
        }
        if let Err(mpsc::TrySendError::Full(record)) = self.ingress.try_send(record) {
            self.losses.drop_record(&record);
        } else {
            signal(&self.data_wake);
        }
    }

    fn flush_loss(&mut self, producer: Producer) -> bool {
        let Self { ingress, data_wake, losses, .. } = self;
        let Ok(flushed) = losses.flush(producer, |gap| {
            Ok::<_, Infallible>(deliver(ingress, data_wake, HelperToCore::Gap(gap)))
        });
        flushed
    }

    fn flush_status(&mut self) {
        let Self { ingress, data_wake, status, .. } = self;
        let Ok(_) = status.flush(|record| Ok::<_, Infallible>(deliver(ingress, data_wake, record)));
    }
}

/// Queue a record and wake the collector. `false` when the queue refused it.
fn deliver(ingress: &SyncSender<HelperToCore>, data_wake: &OwnedHandle, record: HelperToCore) -> bool {
    let sent = ingress.try_send(record).is_ok();
    if sent { signal(data_wake); }
    sent
}

fn signal(data_wake: &OwnedHandle) {
    unsafe { SetEvent(data_wake.as_raw_handle()) };
}

/// An environment record also attests a passed registration check (E4): the
/// check runs now, whatever the last check, and the record carries its QPC.
/// A record that waits for queue room keeps that QPC.
fn report_environment(raw: &mut RawWindow, output: &mut Evidence, changes: u32) -> Result<(), NativeError> {
    let observed_qpc = raw.verify_now()?;
    output.offer(HelperToCore::Environment(EnvironmentState {
        changes,
        observed_qpc,
        swapped_buttons: unsafe { GetSystemMetrics(SM_SWAPBUTTON) } != 0,
    }));
    Ok(())
}

/// Device descriptor IO (`CreateFileW`, `HidD_*`, `SetupDi*`) runs on this
/// helper-owned worker, never on the Raw Input thread (ARCH-2, I6). A device
/// is announced at once with its Raw Input facts; the identified descriptor
/// follows as a second arrival with the same session ID.
struct DeviceWorker {
    requests: SyncSender<(DeviceDescriptor, Vec<u16>)>,
    replies: Receiver<DeviceDescriptor>,
}

impl DeviceWorker {
    fn start(raw_thread: u32) -> Result<Self, NativeError> {
        let (requests, pending) = mpsc::sync_channel::<(DeviceDescriptor, Vec<u16>)>(DEVICE_REQUESTS);
        let (reply, replies) = mpsc::sync_channel(DEVICE_REQUESTS);
        // The worker is detached: it can be blocked in device IO at exit, and
        // ends when either channel closes.
        thread::Builder::new().name("input-devices".into()).spawn(move || {
            for (mut descriptor, path) in pending {
                device::identify(&path, &mut descriptor);
                if reply.send(descriptor).is_err() { return; }
                unsafe { PostThreadMessageW(raw_thread, WAKE, 0, 0) };
            }
        }).map_err(|error| NativeError {
            stage: HelperFailureStage::Startup,
            code: error.raw_os_error(),
            raw_input_registration: None,
        })?;
        Ok(Self { requests, replies })
    }

    /// Announce an arrived device and queue its identification; without a session ID it is dropped.
    fn announce(&self, output: &mut Evidence, devices: &mut device::DeviceIds, sequence: &mut u64, receipt: Receipt,
        descriptor: DeviceDescriptor, path: Option<Vec<u16>>) -> Result<(), NativeError> {
        let Some(id) = devices.assign(descriptor.id) else {
            output.losses.drop_record(&HelperToCore::DeviceArrived { receipt, descriptor });
            return Ok(());
        };
        self.announce_as(output, sequence, receipt, id, descriptor, path)
    }

    /// Announce a device under its session ID and queue its identification.
    fn announce_as(&self, output: &mut Evidence, sequence: &mut u64, receipt: Receipt, id: DeviceId,
        mut descriptor: DeviceDescriptor, path: Option<Vec<u16>>) -> Result<(), NativeError> {
        descriptor.id = id;
        output.offer(HelperToCore::DeviceArrived { receipt, descriptor: descriptor.clone() });
        self.request(output, sequence, descriptor, path)
    }

    /// Queue identification of an announced device. A full queue leaves the
    /// descriptor unidentified and is a device-notification loss.
    fn request(
        &self, output: &mut Evidence, sequence: &mut u64, descriptor: DeviceDescriptor, path: Option<Vec<u16>>,
    ) -> Result<(), NativeError> {
        let Some(path) = path else { return Ok(()) };
        if let Err(mpsc::TrySendError::Full((descriptor, _)) | mpsc::TrySendError::Disconnected((descriptor, _)))
            = self.requests.try_send((descriptor, path))
        {
            let receipt = device_receipt_at(sequence, None, qpc()?)?;
            output.losses.drop_record(&HelperToCore::DeviceArrived { receipt, descriptor });
        }
        Ok(())
    }

    /// Announce identified descriptors of devices that are still present.
    fn deliver(&self, output: &mut Evidence, devices: &device::DeviceIds, sequence: &mut u64) -> Result<(), NativeError> {
        while let Ok(descriptor) = self.replies.try_recv() {
            if devices.active(descriptor.id) {
                let receipt = device_receipt_at(sequence, None, qpc()?)?;
                output.offer(HelperToCore::DeviceArrived { receipt, descriptor });
            }
        }
        Ok(())
    }
}

fn raw_loop(
    output: &mut Evidence,
    pending: &AtomicU32,
    ready: &SyncSender<Result<RawReady, NativeError>>,
    progress_interval: i64,
    pen_hid: bool,
    scope: Option<&CaptureScope>,
) -> Result<(), NativeError> {
    let mut raw = RawWindow::open(pen_hid)?;
    let worker = DeviceWorker::start(unsafe { GetCurrentThreadId() })?;
    let mut device_sequence = 0;
    let mut devices = device::DeviceIds::default();
    let first_inventory = scope.is_none().then(device::inventory).transpose()?;
    let mut inventory_serial = None;
    let _timer = Timer::start(&raw)?;
    ready.try_send(Ok((unsafe { GetCurrentThreadId() }, raw.optional())))
        .map_err(|_| NativeError::without_code(HelperFailureStage::Startup))?;
    if let Some(inventory) = first_inventory {
        observe_inventory(&mut raw, &worker, &mut devices, &mut device_sequence, output, inventory)?;
        inventory_serial = Some(0);
    }
    let mut next_progress = QpcTick(0);
    let mut batch = Vec::new();
    loop {
        let mut message = MSG::default();
        match unsafe { GetMessageW(&mut message, null_mut(), 0, 0) } {
            -1 => return Err(last_error(HelperFailureStage::MessageLoop)),
            0 if pending.load(Acquire) & STOP != 0 => {
                let changes = pending.fetch_and(STOP, AcqRel) & EnvironmentChange::ALL_BITS;
                if changes != 0 { report_environment(&mut raw, output, changes)?; }
                return Ok(());
            }
            0 => return Err(NativeError::without_code(HelperFailureStage::MessageLoop)),
            _ => {}
        }
        let observed = qpc();
        let progress = observed.as_ref().ok().copied();
        let changes = pending.fetch_and(STOP, AcqRel);
        if changes & STOP != 0 {
            if message.message == WM_INPUT && message.hwnd == raw.handle() {
                // Give default foreground cleanup even for a closing message.
                unsafe { windows_sys::Win32::UI::WindowsAndMessaging::DefWindowProcW(
                    message.hwnd, message.message, message.wParam, message.lParam,
                ) };
            }
            let changes = changes & EnvironmentChange::ALL_BITS;
            if changes != 0 { report_environment(&mut raw, output, changes)?; }
            return Ok(());
        }
        let serial = scope.map_or(0, |scope| (scope.serial)());
        let allowed = || scope.is_none_or(|scope| (scope.serial)() == serial && (scope.raw_open)());
        let scoped_message = message.hwnd == raw.handle()
            && matches!(message.message, WM_INPUT | WM_INPUT_DEVICE_CHANGE | WM_TIMER);
        let active = !scoped_message || allowed();
        if scoped_message && active && inventory_serial != Some(serial) {
            devices.clear();
            raw.reset_devices();
            // The old scope's pending gaps keep their sequence and QPC ranges (E3).
            for producer in RAW_THREAD { output.flush_loss(producer); }
            output.status.discard_environment();
            observe_inventory(&mut raw, &worker, &mut devices, &mut device_sequence, output, device::inventory()?)?;
            inventory_serial = Some(serial);
        }
        match message.message {
            WM_INPUT if message.hwnd == raw.handle() && !active => {
                unsafe { windows_sys::Win32::UI::WindowsAndMessaging::DefWindowProcW(
                    message.hwnd, message.message, message.wParam, message.lParam,
                ) };
            }
            WM_INPUT if message.hwnd == raw.handle() => {
                let qpc = match observed {
                    Ok(qpc) => qpc,
                    Err(error) => {
                        unsafe { windows_sys::Win32::UI::WindowsAndMessaging::DefWindowProcW(
                            message.hwnd, message.message, message.wParam, message.lParam,
                        ) };
                        return Err(error);
                    }
                };
                raw.read(&message, qpc, true, |record| batch.push(record), || pending.load(Acquire) & STOP != 0)?;
                // The governed scope is checked once per drained batch: before
                // the read (`active`) and after it (E3).
                if !allowed() { batch.clear(); }
                for record in batch.drain(..) {
                    let mut input = match record {
                        RawRecord::Input(input) => input,
                        RawRecord::Malformed(producer, receipt) => {
                            output.losses.malformed(producer, receipt);
                            continue;
                        }
                    };
                    match devices.identify(&mut input.sample) {
                        Some(first_seen) => {
                            // A device without a prior arrival is announced
                            // before its first record, so the core inventory resolves it.
                            if let Some((native, id)) = first_seen {
                                announce_first_seen(&mut raw, &worker, output, &mut device_sequence, native, id, input.receipt)?;
                            }
                            output.offer(HelperToCore::Input(input));
                        }
                        None => output.losses.drop_record(&HelperToCore::Input(input)),
                    }
                }
            }
            WM_INPUT_DEVICE_CHANGE if message.hwnd == raw.handle() && active => {
                let handle = message.lParam as HANDLE;
                if message.wParam == GIDC_ARRIVAL as usize {
                    let received = observed?;
                    if let Some((mut descriptor, path)) = device::descriptor(handle, None) {
                        raw.remove_device(handle);
                        raw.observe_device(handle, &mut descriptor);
                        let receipt = device_receipt_at(&mut device_sequence, Some(MessageTickMs(message.time)), received)?;
                        worker.announce(output, &mut devices, &mut device_sequence, receipt, descriptor, path)?;
                    }
                } else if message.wParam == GIDC_REMOVAL as usize {
                    raw.remove_device(handle);
                    if let Some(id) = device::id(handle).and_then(|handle| devices.remove(handle)) {
                        let receipt = device_receipt_at(&mut device_sequence, Some(MessageTickMs(message.time)), observed?)?;
                        output.offer(HelperToCore::DeviceRemoved { receipt, id });
                    }
                }
            }
            WM_TIMER if message.hwnd == raw.handle() && message.wParam == CHECK_TIMER => {
                raw.check_registration()?;
                if let Ok(observed_qpc) = observed {
                    for collection in raw.take_lost_optional() {
                        output.offer(HelperToCore::OptionalUnavailable { collection, observed_qpc });
                    }
                }
                output.flush_status();
                for producer in RAW_THREAD { output.flush_loss(producer); }
            }
            WM_TIMER if message.hwnd == raw.handle() && message.wParam == PROGRESS_TIMER => {}
            WAKE if message.hwnd.is_null() => {}
            _ => { unsafe { DispatchMessageW(&message) }; }
        }
        let changes = changes & EnvironmentChange::ALL_BITS;
        if changes != 0 {
            // The Recorder requests a registration check before it returns to this source (E4).
            report_environment(&mut raw, output, changes)?;
        }
        if inventory_serial == Some(serial) && allowed() {
            worker.deliver(output, &devices, &mut device_sequence)?;
        }
        if inventory_serial == Some(serial) && active && let Some(before_qpc) = progress && before_qpc >= next_progress {
            for producer in RAW_THREAD {
                output.offer(HelperToCore::Progress(ProducerProgress { producer, before_qpc }));
            }
            next_progress = QpcTick(before_qpc.0.saturating_add(progress_interval.max(1)));
        }
    }
}

fn observe_inventory(
    raw: &mut RawWindow, worker: &DeviceWorker, devices: &mut device::DeviceIds, device_sequence: &mut u64,
    output: &mut Evidence, inventory: Vec<windows_sys::Win32::UI::Input::RAWINPUTDEVICELIST>,
) -> Result<(), NativeError> {
    report_environment(raw, output, EnvironmentChange::Settings.bit() | EnvironmentChange::Display.bit())?;
    for entry in inventory {
        let received = qpc()?;
        if let Some((mut descriptor, path)) = device::descriptor(entry.hDevice, Some(entry.dwType)) {
            raw.observe_device(entry.hDevice, &mut descriptor);
            let receipt = device_receipt_at(device_sequence, None, received)?;
            worker.announce(output, devices, device_sequence, receipt, descriptor, path)?;
        }
    }
    Ok(())
}

/// Announce a device seen first in an input record, with its Raw Input facts
/// and HID evidence, and queue its identification.
fn announce_first_seen(
    raw: &mut RawWindow, worker: &DeviceWorker, output: &mut Evidence, device_sequence: &mut u64,
    native: DeviceId, id: DeviceId, input: Receipt,
) -> Result<(), NativeError> {
    let handle = native.0 as usize as HANDLE;
    let receipt = device_receipt_at(device_sequence, input.message_tick_ms, input.qpc)?;
    let Some((mut descriptor, path)) = device::descriptor(handle, None) else {
        output.losses.malformed(Producer::DeviceNotifications, receipt);
        return Ok(());
    };
    raw.observe_device(handle, &mut descriptor);
    worker.announce_as(output, device_sequence, receipt, id, descriptor, path)
}

fn device_receipt_at(
    sequence: &mut u64, tick: Option<MessageTickMs>, qpc: screen_fix_protocol::QpcTick,
) -> Result<Receipt, NativeError> {
    *sequence = sequence.checked_add(1)
        .ok_or_else(|| NativeError::without_code(HelperFailureStage::DeviceDiscovery))?;
    Ok(Receipt { sequence: SourceSequence(*sequence), qpc, message_tick_ms: tick })
}

struct Timer(windows_sys::Win32::Foundation::HWND);

impl Timer {
    fn start(raw: &RawWindow) -> Result<Self, NativeError> {
        if unsafe { SetTimer(raw.handle(), CHECK_TIMER, 5000, None) } == 0 {
            return Err(last_error(HelperFailureStage::MessageLoop));
        }
        if unsafe { SetTimer(raw.handle(), PROGRESS_TIMER, 12, None) } == 0 {
            unsafe { KillTimer(raw.handle(), CHECK_TIMER) };
            return Err(last_error(HelperFailureStage::MessageLoop));
        }
        Ok(Self(raw.handle()))
    }
}

impl Drop for Timer {
    fn drop(&mut self) {
        unsafe {
            KillTimer(self.0, CHECK_TIMER);
            KillTimer(self.0, PROGRESS_TIMER);
        }
    }
}
