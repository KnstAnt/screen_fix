use std::{
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
    MessageTickMs, QpcTick, SourceSequence,
    ipc::{EnvironmentChange, EnvironmentState, HelperFailureStage, HelperToCore, Producer, ProducerProgress, Receipt},
};
use windows_sys::Win32::{
    System::Threading::{GetCurrentThreadId, SetEvent},
    UI::WindowsAndMessaging::{
        DispatchMessageW, GIDC_ARRIVAL, GIDC_REMOVAL, GetMessageW, GetSystemMetrics, KillTimer,
        MSG, PostThreadMessageW, SM_SWAPBUTTON, SetTimer, WM_APP, WM_INPUT, WM_INPUT_DEVICE_CHANGE,
        WM_QUIT, WM_TIMER,
    },
};

use super::{NativeError, device, last_error, loss::{self, Losses}, raw::{RawWindow, qpc}};

const CHECK_TIMER: usize = 1;
const PROGRESS_TIMER: usize = 2;
const WAKE: u32 = WM_APP + 1;
const STOP: u32 = 1 << 31;
const RAW_QUEUE_CAPACITY: usize = 4096;
const DEVICE_AND_RAW: [Producer; 2] = [Producer::DeviceNotifications, Producer::RawInput];

pub(super) struct RawOwner {
    thread_id: u32,
    pending: Arc<AtomicU32>,
    owner: thread::JoinHandle<RawResult>,
    pub(super) records: Receiver<HelperToCore>,
}

pub(super) struct RawExit {
    pub(super) result: Result<(), NativeError>,
    pub(super) records: Receiver<HelperToCore>,
    pub(super) losses: Losses,
    pub(super) environment: Option<EnvironmentState>,
}

struct RawResult {
    result: Result<(), NativeError>,
    losses: Losses,
    environment: Option<EnvironmentState>,
}

impl RawOwner {
    pub(super) fn start(data_wake: OwnedHandle, frequency: i64, pen_hid: bool) -> Result<Self, NativeError> {
        let (ready, initialized) = mpsc::sync_channel(1);
        let (ingress, records) = mpsc::sync_channel(RAW_QUEUE_CAPACITY);
        let pending = Arc::new(AtomicU32::new(0));
        let thread_pending = Arc::clone(&pending);
        let owner = thread::Builder::new().name("input-raw".into()).spawn(move || {
            let mut output = Evidence { ingress, data_wake, losses: Losses::default(), environment: None };
            let result = raw_loop(&mut output, &thread_pending, &ready, frequency / 80, pen_hid);
            if let Err(error) = result {
                let _ = ready.try_send(Err(error));
                output.signal();
            }
            RawResult { result, losses: output.losses, environment: output.environment }
        }).map_err(|error| NativeError {
            stage: HelperFailureStage::Startup,
            code: error.raw_os_error(),
        })?;
        let thread_id = match initialized.recv_timeout(Duration::from_secs(2)) {
            Ok(status) => status?,
            Err(_) => return Err(NativeError::without_code(HelperFailureStage::MessageLoop)),
        };
        Ok(Self { thread_id, pending, owner, records })
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
            self.post(WM_QUIT)?;
        }
        let RawResult { result, losses, environment } = self.owner.join()
            .map_err(|_| NativeError::without_code(HelperFailureStage::MessageLoop))?;
        Ok(RawExit { result, losses, records: self.records, environment })
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
    environment: Option<EnvironmentState>,
}

impl Evidence {
    fn offer(&mut self, record: HelperToCore) {
        if let Some(producer) = loss::producer(&record)
            && !self.flush_loss(producer)
        {
            self.losses.drop_record(&record);
            return;
        }
        if let Err(mpsc::TrySendError::Full(record)) = self.ingress.try_send(record) {
            self.losses.drop_record(&record);
        } else {
            self.signal();
        }
    }

    fn flush_loss(&mut self, producer: Producer) -> bool {
        let Some(gap) = self.losses.pending(producer) else { return true };
        if self.ingress.try_send(HelperToCore::Gap(gap)).is_err() { return false; }
        self.losses.clear(producer);
        self.signal();
        true
    }

    fn environment(&mut self, changes: u32) -> Result<(), NativeError> {
        let value = EnvironmentState {
            changes: changes | self.environment.map_or(0, |old| old.changes),
            observed_qpc: qpc()?,
            swapped_buttons: unsafe { GetSystemMetrics(SM_SWAPBUTTON) } != 0,
        };
        if self.ingress.try_send(HelperToCore::Environment(value)).is_ok() {
            self.environment = None;
            self.signal();
        } else {
            self.environment = Some(value);
        }
        Ok(())
    }

    fn signal(&self) {
        unsafe { SetEvent(self.data_wake.as_raw_handle()) };
    }
}

fn raw_loop(
    output: &mut Evidence,
    pending: &AtomicU32,
    ready: &SyncSender<Result<u32, NativeError>>,
    progress_interval: i64,
    pen_hid: bool,
) -> Result<(), NativeError> {
    let mut raw = RawWindow::open(pen_hid)?;
    let mut device_sequence = 0;
    let mut devices = device::DeviceIds::default();
    let inventory = device::inventory()?;
    let _timer = Timer::start(&raw)?;
    ready.try_send(Ok(unsafe { GetCurrentThreadId() }))
        .map_err(|_| NativeError::without_code(HelperFailureStage::Startup))?;
    output.environment(EnvironmentChange::Settings.bit() | EnvironmentChange::Display.bit())?;
    for entry in inventory {
        let received = qpc()?;
        if let Some(mut descriptor) = device::descriptor(entry.hDevice, Some(entry.dwType)) {
            raw.observe_device(entry.hDevice, &mut descriptor);
            let receipt = device_receipt_at(&mut device_sequence, None, received)?;
            if let Some(id) = devices.assign(descriptor.id) {
                descriptor.id = id;
                output.offer(HelperToCore::DeviceArrived { receipt, descriptor });
            } else {
                output.losses.drop_record(&HelperToCore::DeviceArrived { receipt, descriptor });
            }
        }
    }
    let mut next_progress = QpcTick(0);
    loop {
        let mut message = MSG::default();
        match unsafe { GetMessageW(&mut message, null_mut(), 0, 0) } {
            -1 => return Err(last_error(HelperFailureStage::MessageLoop)),
            0 if pending.load(Acquire) & STOP != 0 => {
                let changes = pending.fetch_and(STOP, AcqRel) & EnvironmentChange::ALL_BITS;
                if changes != 0 { output.environment(changes)?; }
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
            if changes != 0 { output.environment(changes)?; }
            return Ok(());
        }
        match message.message {
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
                raw.read(&message, qpc, |mut input| {
                    if devices.identify(&mut input.sample) {
                        output.offer(HelperToCore::Input(input));
                    } else {
                        output.losses.drop_record(&HelperToCore::Input(input));
                    }
                }, || pending.load(Acquire) & STOP != 0)?;
            }
            WM_INPUT_DEVICE_CHANGE if message.hwnd == raw.handle() => {
                let handle = message.lParam as windows_sys::Win32::Foundation::HANDLE;
                if message.wParam == GIDC_ARRIVAL as usize {
                    let received = observed?;
                    if let Some(mut descriptor) = device::descriptor(handle, None) {
                        raw.remove_device(handle);
                        raw.observe_device(handle, &mut descriptor);
                        let receipt = device_receipt_at(&mut device_sequence, Some(MessageTickMs(message.time)), received)?;
                        if let Some(id) = devices.assign(descriptor.id) {
                            descriptor.id = id;
                            output.offer(HelperToCore::DeviceArrived { receipt, descriptor });
                        } else {
                            output.losses.drop_record(&HelperToCore::DeviceArrived { receipt, descriptor });
                        }
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
                for producer in DEVICE_AND_RAW { output.flush_loss(producer); }
                if output.environment.is_some() { output.environment(0)?; }
            }
            WM_TIMER if message.hwnd == raw.handle() && message.wParam == PROGRESS_TIMER => {}
            WAKE if message.hwnd.is_null() => {}
            _ => { unsafe { DispatchMessageW(&message) }; }
        }
        let changes = changes & EnvironmentChange::ALL_BITS;
        if changes != 0 { output.environment(changes)?; }
        if let Some(before_qpc) = progress && before_qpc >= next_progress {
            for producer in DEVICE_AND_RAW {
                output.offer(HelperToCore::Progress(ProducerProgress { producer, before_qpc }));
            }
            next_progress = QpcTick(before_qpc.0.saturating_add(progress_interval.max(1)));
        }
    }
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
