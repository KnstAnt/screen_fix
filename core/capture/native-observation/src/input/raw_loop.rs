use std::{
    convert::Infallible,
    os::windows::io::{AsRawHandle, OwnedHandle},
    ptr::null_mut,
    sync::{
        Arc,
        atomic::{AtomicU32, Ordering::{AcqRel, Acquire, Release}},
        mpsc::{self, Receiver, SyncSender, TrySendError},
    },
    thread,
};

use screen_fix_protocol::{
    DeviceId, MessageTickMs, QpcTick, SourceSequence,
    ipc::{DeviceDescriptor, EnvironmentChange, EnvironmentState, HelperFailureStage, HelperToCore, Producer, ProducerProgress, Receipt},
};
use windows_sys::Win32::{
    Foundation::HANDLE,
    System::Threading::{GetCurrentThreadId, SetEvent},
    UI::{
        Input::RAWINPUTDEVICELIST,
        WindowsAndMessaging::{
            DefWindowProcW, DispatchMessageW, GIDC_ARRIVAL, GIDC_REMOVAL, GetMessageW, GetSystemMetrics, KillTimer,
            MSG, PostThreadMessageW, SM_SWAPBUTTON, SetTimer, WM_APP, WM_INPUT, WM_INPUT_DEVICE_CHANGE,
            WM_TIMER,
        },
    },
};

use crate::owner::{self, OwnerThread, Startup, StartupError};

use super::{
    InputScope, NativeError,
    device::{self, DeviceIds, Identified, ProbedDevice},
    last_error,
    loss::{Backlog, Sent},
    raw::{RawRecord, RawWindow, qpc},
};

/// The optional collections the Raw Input thread registered.
type RawReady = [bool; screen_fix_protocol::ipc::OptionalCollection::ALL.len()];

const CHECK_TIMER: usize = 1;
const PROGRESS_TIMER: usize = 2;
const WAKE: u32 = WM_APP + 1;
const STOP: u32 = 1 << 31;
const RAW_QUEUE_CAPACITY: usize = 4096;
const RAW_THREAD: [Producer; 3] = [Producer::DeviceNotifications, Producer::RawInput, Producer::OptionalHid];
const DEVICE_REQUESTS: usize = 64;

pub(super) struct RawOwner {
    pending: Arc<AtomicU32>,
    owner: OwnerThread<RawResult>,
    optional: RawReady,
    pub(super) records: Receiver<HelperToCore>,
}

pub(super) struct RawExit {
    pub(super) result: Result<(), NativeError>,
    pub(super) records: Receiver<HelperToCore>,
    pub(super) backlog: Backlog,
}

struct RawResult {
    result: Result<(), NativeError>,
    backlog: Backlog,
}

impl RawOwner {
    pub(super) fn start(
        data_wake: OwnedHandle, frequency: i64, pen_hid: bool, scope: Option<Arc<dyn InputScope>>,
    ) -> Result<Self, NativeError> {
        let scoped = scope.is_some();
        let (ingress, records) = mpsc::sync_channel(RAW_QUEUE_CAPACITY);
        let pending = Arc::new(AtomicU32::new(0));
        let thread_pending = Arc::clone(&pending);
        let started = owner::spawn("input-raw", scoped, || {}, move |ready| {
            let mut output = Evidence { ingress, data_wake, backlog: Backlog::default() };
            let result = raw_loop(&mut output, &thread_pending, &ready, frequency / 80, pen_hid, scope.as_deref());
            if let Err(error) = result {
                ready.fail(error);
                signal(&output.data_wake);
            }
            RawResult { result, backlog: output.backlog }
        });
        let (owner, optional) = match started {
            Ok(started) => started,
            Err(StartupError::Spawn(error)) => return Err(NativeError::io(HelperFailureStage::Startup, &error)),
            Err(StartupError::Failed(error)) => return Err(error),
            Err(StartupError::Timeout) => return Err(NativeError::without_code(HelperFailureStage::MessageLoop)),
        };
        Ok(Self { pending, owner, records, optional })
    }

    /// Optional collections registered at startup, in `OptionalCollection::ALL` order.
    pub(super) fn optional(&self) -> RawReady {
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
        // An ended thread no longer reads the pending changes.
        self.pending.fetch_or(STOP | changes, Release);
        let RawResult { result, backlog } = self.owner.post_quit_and_join()
            .map_err(|error| NativeError::io(HelperFailureStage::MessageLoop, &error))?
            .map_err(|_| NativeError::without_code(HelperFailureStage::MessageLoop))?;
        Ok(RawExit { result, records: self.records, backlog })
    }

    fn post(&self, message: u32) -> Result<(), NativeError> {
        if unsafe { PostThreadMessageW(self.owner.thread_id, message, 0, 0) } == 0 {
            return Err(last_error(HelperFailureStage::MessageLoop));
        }
        Ok(())
    }
}

/// The Raw Input thread's side of the record queue.
struct Evidence {
    ingress: SyncSender<HelperToCore>,
    data_wake: OwnedHandle,
    /// Gaps and status records waiting for queue room.
    backlog: Backlog,
}

impl Evidence {
    fn offer(&mut self, record: HelperToCore) {
        let Self { ingress, data_wake, backlog } = self;
        let Ok(()) = backlog.offer(record, |record| Ok::<_, Infallible>(deliver(ingress, data_wake, record)));
    }

    /// Deliver the pending gaps of each Raw Input thread producer on its own.
    fn flush_losses(&mut self) {
        let Self { ingress, data_wake, backlog } = self;
        for producer in RAW_THREAD {
            let Ok(_) = backlog.flush_losses(producer, |record| Ok::<_, Infallible>(deliver(ingress, data_wake, record)));
        }
    }

    fn flush_status(&mut self) {
        let Self { ingress, data_wake, backlog } = self;
        let Ok(_) = backlog.flush_status(|record| Ok::<_, Infallible>(deliver(ingress, data_wake, record)));
    }
}

/// Queue a record and wake the collector. The collector holds the receiver
/// until this thread is joined, so a refused record found the queue full.
fn deliver(ingress: &SyncSender<HelperToCore>, data_wake: &OwnedHandle, record: HelperToCore) -> Sent {
    match ingress.try_send(record) {
        Ok(()) => {
            signal(data_wake);
            Sent::Accepted
        }
        Err(TrySendError::Full(record) | TrySendError::Disconnected(record)) => Sent::Full(record),
    }
}

fn signal(data_wake: &OwnedHandle) {
    unsafe { SetEvent(data_wake.as_raw_handle()) };
}

/// Give a retrieved message its default cleanup.
fn default_cleanup(message: &MSG) {
    unsafe { DefWindowProcW(message.hwnd, message.message, message.wParam, message.lParam) };
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
        }).map_err(|error| NativeError::io(HelperFailureStage::Startup, &error))?;
        Ok(Self { requests, replies })
    }
}

/// The device-notification producer: session device IDs, the identification
/// worker and the producer's sequence.
struct Devices {
    worker: DeviceWorker,
    ids: DeviceIds,
    /// The last device-notification sequence number.
    sequence: u64,
}

impl Devices {
    fn new(worker: DeviceWorker) -> Self {
        Self { worker, ids: DeviceIds::default(), sequence: 0 }
    }

    /// The next device-notification receipt.
    fn receipt(&mut self, tick: Option<MessageTickMs>, qpc: QpcTick) -> Result<Receipt, NativeError> {
        self.sequence = self.sequence.checked_add(1)
            .ok_or_else(|| NativeError::without_code(HelperFailureStage::DeviceDiscovery))?;
        Ok(Receipt { sequence: SourceSequence(self.sequence), qpc, message_tick_ms: tick })
    }

    /// Report the environment, then announce every listed device.
    fn inventory(
        &mut self, raw: &mut RawWindow, output: &mut Evidence, entries: Vec<RAWINPUTDEVICELIST>,
    ) -> Result<(), NativeError> {
        report_environment(raw, output, EnvironmentChange::Settings.bit() | EnvironmentChange::Display.bit())?;
        for entry in entries {
            let received = qpc()?;
            if let Some(mut probed) = device::descriptor(entry.hDevice, Some(entry.dwType)) {
                raw.observe_device(entry.hDevice, &mut probed.descriptor);
                let receipt = self.receipt(None, received)?;
                self.announce(output, receipt, probed)?;
            }
        }
        Ok(())
    }

    /// Forget the old scope's devices and announce the current inventory.
    fn rescope(&mut self, raw: &mut RawWindow, output: &mut Evidence) -> Result<(), NativeError> {
        self.ids.clear();
        raw.reset_devices();
        // The old scope's pending gaps keep their sequence and QPC ranges (E3).
        output.flush_losses();
        output.backlog.discard_environment();
        self.inventory(raw, output, device::inventory()?)
    }

    /// Announce an arrived device and queue its identification; without a session ID it is dropped.
    fn announce(&mut self, output: &mut Evidence, receipt: Receipt, probed: ProbedDevice) -> Result<(), NativeError> {
        let Some(id) = self.ids.assign(probed.descriptor.id) else {
            output.backlog.drop_record(&HelperToCore::DeviceArrived { receipt, descriptor: probed.descriptor });
            return Ok(());
        };
        self.announce_as(output, receipt, id, probed)
    }

    /// Announce a device seen first in an input record, with its Raw Input
    /// facts and HID evidence, and queue its identification.
    fn first_seen(
        &mut self, raw: &mut RawWindow, output: &mut Evidence, native: DeviceId, id: DeviceId, input: Receipt,
    ) -> Result<(), NativeError> {
        let handle = native.0 as usize as HANDLE;
        let receipt = self.receipt(input.message_tick_ms, input.qpc)?;
        let Some(mut probed) = device::descriptor(handle, None) else {
            output.backlog.malformed(Producer::DeviceNotifications, receipt);
            return Ok(());
        };
        raw.observe_device(handle, &mut probed.descriptor);
        self.announce_as(output, receipt, id, probed)
    }

    /// Announce the removal of a device that has a session ID.
    fn removed(
        &mut self, output: &mut Evidence, handle: HANDLE, tick: Option<MessageTickMs>,
        observed: Result<QpcTick, NativeError>,
    ) -> Result<(), NativeError> {
        if let Some(id) = device::id(handle).and_then(|handle| self.ids.remove(handle)) {
            let receipt = self.receipt(tick, observed?)?;
            output.offer(HelperToCore::DeviceRemoved { receipt, id });
        }
        Ok(())
    }

    /// Announce a device under its session ID and queue its identification.
    fn announce_as(
        &mut self, output: &mut Evidence, receipt: Receipt, id: DeviceId, probed: ProbedDevice,
    ) -> Result<(), NativeError> {
        let ProbedDevice { mut descriptor, path } = probed;
        descriptor.id = id;
        output.offer(HelperToCore::DeviceArrived { receipt, descriptor: descriptor.clone() });
        self.request(output, descriptor, path)
    }

    /// Queue identification of an announced device. A full queue leaves the
    /// descriptor unidentified and is a device-notification loss.
    fn request(
        &mut self, output: &mut Evidence, descriptor: DeviceDescriptor, path: Option<Vec<u16>>,
    ) -> Result<(), NativeError> {
        let Some(path) = path else { return Ok(()) };
        if let Err(TrySendError::Full((descriptor, _)) | TrySendError::Disconnected((descriptor, _)))
            = self.worker.requests.try_send((descriptor, path))
        {
            let receipt = self.receipt(None, qpc()?)?;
            output.backlog.drop_record(&HelperToCore::DeviceArrived { receipt, descriptor });
        }
        Ok(())
    }

    /// Announce identified descriptors of devices that are still present.
    fn deliver(&mut self, output: &mut Evidence) -> Result<(), NativeError> {
        while let Ok(descriptor) = self.worker.replies.try_recv() {
            if self.ids.active(descriptor.id) {
                let receipt = self.receipt(None, qpc()?)?;
                output.offer(HelperToCore::DeviceArrived { receipt, descriptor });
            }
        }
        Ok(())
    }
}

/// The capture scope as the Raw Input thread checks it.
#[derive(Clone, Copy)]
struct Scope<'a>(Option<&'a dyn InputScope>);

impl Scope<'_> {
    /// The current scope serial; 0 without a scope.
    fn serial(self) -> u64 {
        self.0.map_or(0, |scope| scope.serial())
    }

    /// Whether the scope of `serial` is still current and open.
    fn allowed(self, serial: u64) -> bool {
        self.0.is_none_or(|scope| scope.serial() == serial && scope.raw_open())
    }
}

/// The Raw Input thread's state between messages.
///
/// Fields drop in declaration order: the device worker's channels close
/// before the Raw Input window is destroyed, as before the split.
struct RawThread<'a> {
    devices: Devices,
    raw: RawWindow,
    output: &'a mut Evidence,
    pending: &'a AtomicU32,
    scope: Scope<'a>,
    /// The scope serial whose device inventory was announced.
    inventory_serial: Option<u64>,
    progress_interval: i64,
    next_progress: QpcTick,
    /// Records of one read, reused across reads.
    batch: Vec<RawRecord>,
}

fn raw_loop(
    output: &mut Evidence,
    pending: &AtomicU32,
    ready: &Startup<RawReady, NativeError>,
    progress_interval: i64,
    pen_hid: bool,
    scope: Option<&dyn InputScope>,
) -> Result<(), NativeError> {
    let raw = RawWindow::open(pen_hid)?;
    let worker = DeviceWorker::start(unsafe { GetCurrentThreadId() })?;
    let first_inventory = scope.is_none().then(device::inventory).transpose()?;
    let mut thread = RawThread {
        devices: Devices::new(worker), raw, output, pending, scope: Scope(scope),
        inventory_serial: None, progress_interval, next_progress: QpcTick(0), batch: Vec::new(),
    };
    // Declared after `thread`, so the timers stop before the window is destroyed.
    let _timer = Timer::start(&thread.raw)?;
    if !ready.ready(thread.raw.optional()) {
        return Err(NativeError::without_code(HelperFailureStage::Startup));
    }
    if let Some(inventory) = first_inventory {
        thread.devices.inventory(&mut thread.raw, thread.output, inventory)?;
        thread.inventory_serial = Some(0);
    }
    loop {
        let mut message = MSG::default();
        match unsafe { GetMessageW(&mut message, null_mut(), 0, 0) } {
            -1 => return Err(last_error(HelperFailureStage::MessageLoop)),
            0 if pending.load(Acquire) & STOP != 0 => return thread.close(pending.fetch_and(STOP, AcqRel)),
            0 => return Err(NativeError::without_code(HelperFailureStage::MessageLoop)),
            _ => {}
        }
        let observed = qpc();
        let changes = pending.fetch_and(STOP, AcqRel);
        if changes & STOP != 0 {
            if message.message == WM_INPUT && message.hwnd == thread.raw.handle() {
                // Give default foreground cleanup even for a closing message.
                default_cleanup(&message);
            }
            return thread.close(changes);
        }
        thread.on_message(&message, observed, changes)?;
    }
}

impl RawThread<'_> {
    /// The stop protocol: report the environment changes still pending.
    fn close(&mut self, changes: u32) -> Result<(), NativeError> {
        let changes = changes & EnvironmentChange::ALL_BITS;
        if changes != 0 { report_environment(&mut self.raw, self.output, changes)?; }
        Ok(())
    }

    fn on_message(
        &mut self, message: &MSG, observed: Result<QpcTick, NativeError>, changes: u32,
    ) -> Result<(), NativeError> {
        let progress = observed.as_ref().ok().copied();
        let serial = self.scope.serial();
        let own = message.hwnd == self.raw.handle();
        let scoped_message = own && matches!(message.message, WM_INPUT | WM_INPUT_DEVICE_CHANGE | WM_TIMER);
        let active = !scoped_message || self.scope.allowed(serial);
        if scoped_message && active && self.inventory_serial != Some(serial) {
            self.devices.rescope(&mut self.raw, self.output)?;
            self.inventory_serial = Some(serial);
        }
        match message.message {
            WM_INPUT if own && !active => default_cleanup(message),
            WM_INPUT if own => self.on_input(message, observed, serial)?,
            WM_INPUT_DEVICE_CHANGE if own && active => self.on_device_change(message, observed)?,
            WM_TIMER if own && message.wParam == CHECK_TIMER => self.on_timer(observed)?,
            WM_TIMER if own && message.wParam == PROGRESS_TIMER => {}
            WAKE if message.hwnd.is_null() => {}
            _ => { unsafe { DispatchMessageW(message) }; }
        }
        let changes = changes & EnvironmentChange::ALL_BITS;
        if changes != 0 {
            // The Recorder requests a registration check before it returns to this source (E4).
            report_environment(&mut self.raw, self.output, changes)?;
        }
        if self.inventory_serial == Some(serial) && self.scope.allowed(serial) {
            self.devices.deliver(self.output)?;
        }
        if self.inventory_serial == Some(serial) && active && let Some(before_qpc) = progress
            && before_qpc >= self.next_progress
        {
            for producer in RAW_THREAD {
                self.output.offer(HelperToCore::Progress(ProducerProgress { producer, before_qpc }));
            }
            self.next_progress = QpcTick(before_qpc.0.saturating_add(self.progress_interval.max(1)));
        }
        Ok(())
    }

    fn on_input(&mut self, message: &MSG, observed: Result<QpcTick, NativeError>, serial: u64) -> Result<(), NativeError> {
        let qpc = match observed {
            Ok(qpc) => qpc,
            Err(error) => {
                default_cleanup(message);
                return Err(error);
            }
        };
        let Self { devices, raw, output, pending, scope, batch, .. } = self;
        raw.read(message, qpc, true, |record| batch.push(record), || pending.load(Acquire) & STOP != 0)?;
        // The scope is checked once per drained batch: before the read
        // (`active`) and after it (E3).
        if !scope.allowed(serial) { batch.clear(); }
        for record in batch.drain(..) {
            let mut input = match record {
                RawRecord::Input(input) => input,
                RawRecord::Malformed(producer, receipt) => {
                    output.backlog.malformed(producer, receipt);
                    continue;
                }
            };
            match devices.ids.identify(&mut input.sample) {
                Identified::Full => output.backlog.drop_record(&HelperToCore::Input(input)),
                identified => {
                    // A device without a prior arrival is announced before
                    // its first record, so the core inventory resolves it.
                    if let Identified::FirstSeen { native, id } = identified {
                        devices.first_seen(raw, output, native, id, input.receipt)?;
                    }
                    output.offer(HelperToCore::Input(input));
                }
            }
        }
        Ok(())
    }

    fn on_device_change(&mut self, message: &MSG, observed: Result<QpcTick, NativeError>) -> Result<(), NativeError> {
        let handle = message.lParam as HANDLE;
        let tick = Some(MessageTickMs(message.time));
        if message.wParam == GIDC_ARRIVAL as usize {
            let received = observed?;
            if let Some(mut probed) = device::descriptor(handle, None) {
                self.raw.remove_device(handle);
                self.raw.observe_device(handle, &mut probed.descriptor);
                let receipt = self.devices.receipt(tick, received)?;
                self.devices.announce(self.output, receipt, probed)?;
            }
        } else if message.wParam == GIDC_REMOVAL as usize {
            self.raw.remove_device(handle);
            self.devices.removed(self.output, handle, tick, observed)?;
        }
        Ok(())
    }

    fn on_timer(&mut self, observed: Result<QpcTick, NativeError>) -> Result<(), NativeError> {
        self.raw.check_registration()?;
        if let Ok(observed_qpc) = observed {
            for collection in self.raw.take_lost_optional() {
                self.output.offer(HelperToCore::OptionalUnavailable { collection, observed_qpc });
            }
        }
        self.output.flush_status();
        self.output.flush_losses();
        Ok(())
    }
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
