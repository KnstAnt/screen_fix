//! The recording worker owns this child, its job and its anonymous channel.
//! A child is launched once, in the recorder's Windows session and token.

use std::{
    ffi::OsStr,
    io,
    mem::size_of,
    os::windows::{
        ffi::OsStrExt,
        io::{AsHandle, AsRawHandle, FromRawHandle, OwnedHandle},
    },
    ptr::{null, null_mut},
    time::{Duration, Instant},
};

use windows_sys::Win32::{
    Foundation::{DUPLICATE_SAME_ACCESS, DuplicateHandle, HANDLE, WAIT_OBJECT_0, WAIT_TIMEOUT},
    System::{
        JobObjects::{
            AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
            JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
            SetInformationJobObject,
        },
        Threading::{
            CREATE_NO_WINDOW, CREATE_SUSPENDED, CreateEventW, CreateProcessW,
            DeleteProcThreadAttributeList, EXTENDED_STARTUPINFO_PRESENT, GetCurrentProcess,
            GetExitCodeProcess, InitializeProcThreadAttributeList,
            LPPROC_THREAD_ATTRIBUTE_LIST, PROCESS_INFORMATION, PROC_THREAD_ATTRIBUTE_HANDLE_LIST,
            ResumeThread, STARTUPINFOEXW, SetEvent, TerminateProcess, UpdateProcThreadAttribute,
            WaitForSingleObject,
        },
    },
};

use crate::protocol::{
    QpcTick, SourceSequence,
    ipc::{ConfigureInput, CoreToHelper, HelperFailure, HelperFailureStage, HelperToCore,
        InputRegistration, PacketError, Producer, shared::RecorderEndpoint},
};

// Standard SYNCHRONIZE access granted on the child copy of the parent process handle.
const WAIT_ONLY: u32 = 0x0010_0000;
const CLEANUP_WAIT: Duration = Duration::from_millis(750);
pub(super) const READY_WAIT: Duration = Duration::from_secs(5);
const POLL_BATCH: usize = 512;
const WAIT_SLICE: Duration = Duration::from_millis(4);
const HELPER_NAME: &str = "screen-fix-input-helper.exe";

#[derive(Debug)]
pub(super) enum HelperError {
    Missing,
    Windows(&'static str, io::Error),
    Protocol(PacketError),
    Reported(HelperFailure),
    UnexpectedExit(u32),
    ReadyTimeout,
    ShutdownTimeout,
    SuspectedHookLoss(Producer),
    ProducerStalled(Producer),
}

pub(super) struct InputHelper {
    channel: RecorderEndpoint,
    wake: OwnedHandle,
    process: OwnedHandle,
    // Closing this is the last-resort process cleanup, including parent crash.
    job: Option<OwnedHandle>,
    received: [Option<(SourceSequence, QpcTick)>; Producer::ALL.len()],
    progress: [Option<QpcTick>; Producer::ALL.len()],
    registration: Option<InputRegistration>,
}

impl InputHelper {
    pub(super) fn start(config: ConfigureInput) -> Result<Self, HelperError> {
        let exe = std::env::current_exe().map_err(|error|
            HelperError::Windows("Find the recorder executable", error))?.with_file_name(HELPER_NAME);
        let executable: Vec<u16> = exe.as_os_str().encode_wide().chain([0]).collect();
        let mut channel = RecorderEndpoint::create()
            .map_err(|error| HelperError::Windows("Create the anonymous input channel", error))?;
        if !channel.configure(&CoreToHelper::Configure(config)).map_err(HelperError::Protocol)? {
            return Err(HelperError::Protocol(PacketError::InvalidRecord("Input configuration queue was full.")));
        }
        // Both objects start non-inheritable. Only the explicit duplicates below
        // enter the launch list, and their parent copies close after CreateProcessW.
        let wake = own(unsafe { CreateEventW(null(), 0, 0, null()) }, "Create the input control wake")?;
        let job = configured_job()?;
        let parent = unsafe { GetCurrentProcess() };
        let mapping_for_child = duplicate(channel.mapping_handle().as_raw_handle(), None)?;
        let wake_for_child = duplicate(wake.as_handle().as_raw_handle(), None)?;
        let parent_for_child = duplicate(parent, Some(WAIT_ONLY))?;
        let handles = [
            mapping_for_child.as_raw_handle(), wake_for_child.as_raw_handle(), parent_for_child.as_raw_handle(),
        ];
        let mut attributes = HandleList::new()?;
        attributes.set_handles(&handles)?;
        let mut startup = STARTUPINFOEXW::default();
        startup.StartupInfo.cb = size_of::<STARTUPINFOEXW>() as u32;
        startup.lpAttributeList = attributes.pointer();
        // argv[0] has no user-controlled characters. The separate application
        // argument supplies an explicit absolute path, with no PATH search.
        let arguments = format!("{HELPER_NAME} --recorder-child {} {} {}",
            handles[0] as usize, handles[1] as usize, handles[2] as usize);
        let mut arguments: Vec<u16> = OsStr::new(&arguments).encode_wide().chain([0]).collect();
        let mut child = PROCESS_INFORMATION::default();
        let result = unsafe {
            CreateProcessW(executable.as_ptr(), arguments.as_mut_ptr(), null(), null(), 1,
                CREATE_SUSPENDED | CREATE_NO_WINDOW | EXTENDED_STARTUPINFO_PRESENT,
                null(), null(), &startup.StartupInfo, &mut child)
        };
        if result == 0 {
            let error = io::Error::last_os_error();
            return Err(match error.kind() {
                io::ErrorKind::NotFound => HelperError::Missing,
                _ => HelperError::Windows("Start the installed input helper", error),
            });
        }
        // Successful CreateProcessW returns two independently owned handles.
        let process = unsafe { OwnedHandle::from_raw_handle(child.hProcess) };
        let thread = unsafe { OwnedHandle::from_raw_handle(child.hThread) };
        if unsafe { AssignProcessToJobObject(job.as_raw_handle(), process.as_raw_handle()) } == 0 {
            let failure = native_error("Attach the input helper to the recorder job");
            // The child is still suspended and no callback has run.
            unsafe { TerminateProcess(process.as_raw_handle(), 1) };
            return Err(failure);
        }
        if unsafe { ResumeThread(thread.as_raw_handle()) } == u32::MAX {
            return Err(native_error("Resume the input helper"));
        }
        Ok(Self {
            channel, wake, process, job: Some(job), received: [None; Producer::ALL.len()], progress: [None; Producer::ALL.len()], registration: None,
        })
    }

    pub(super) fn poll(&mut self, consume: impl FnMut(HelperToCore)) -> Result<(), HelperError> {
        self.drain(consume)?;
        self.poll_status()?;
        if let Some(code) = self.exit_code()? {
            self.poll_status()?;
            return Err(HelperError::UnexpectedExit(code));
        }
        Ok(())
    }

    pub(super) fn poll_status(&mut self) -> Result<Option<InputRegistration>, HelperError> {
        let Some(record) = self.channel.receive_status().map_err(HelperError::Protocol)? else {
            return Ok(self.registration);
        };
        match record {
            HelperToCore::Ready(registration) if self.registration.is_none() => {
                self.registration = Some(registration);
                Ok(self.registration)
            }
            HelperToCore::Failed(failure) => Err(HelperError::Reported(failure)),
            _ => Err(HelperError::Protocol(PacketError::InvalidRecord("Unexpected input helper status."))),
        }
    }

    fn drain(&mut self, mut consume: impl FnMut(HelperToCore)) -> Result<bool, HelperError> {
        for _ in 0..POLL_BATCH {
            let Some(record) = self.channel.receive().map_err(HelperError::Protocol)? else { return Ok(true) };
            self.validate(&record)?;
            consume(record);
        }
        Ok(false)
    }

    fn validate(&mut self, record: &HelperToCore) -> Result<(), HelperError> {
        let (producer, first, last, first_qpc, last_qpc) = match record {
            HelperToCore::Input(input) => {
                let receipt = input.receipt;
                (input.sample.producer(), receipt.sequence, receipt.sequence, receipt.qpc, receipt.qpc)
            }
            HelperToCore::DeviceArrived { receipt, .. } | HelperToCore::DeviceRemoved { receipt, .. } =>
                (Producer::DeviceNotifications, receipt.sequence, receipt.sequence, receipt.qpc, receipt.qpc),
            HelperToCore::Gap(gap) => (gap.producer, gap.first_sequence, gap.last_sequence, gap.first_qpc, gap.last_qpc),
            HelperToCore::Environment(_) | HelperToCore::OptionalUnavailable { .. } => return Ok(()),
            HelperToCore::Progress(progress) => {
                let bound = &mut self.progress[progress.producer.index()];
                if bound.is_some_and(|previous| progress.before_qpc < previous) {
                    return Err(HelperError::Protocol(PacketError::InvalidRecord("An input progress bound moved backward.")));
                }
                *bound = Some(progress.before_qpc);
                return Ok(());
            }
            _ => return Err(HelperError::Protocol(PacketError::InvalidRecord("Input status used the evidence queue."))),
        };
        let index = producer.index();
        let previous = &mut self.received[index];
        let expected = previous.map_or(Some(1), |(seq, _)| seq.0.checked_add(1));
        if Some(first.0) != expected || first_qpc.0 < 0
            || self.progress[index].is_some_and(|bound| first_qpc < bound)
            || previous.is_some_and(|(_, at)| first_qpc < at)
        {
            return Err(HelperError::Protocol(PacketError::InvalidRecord("An input producer prefix is inconsistent.")));
        }
        *previous = Some((last, last_qpc));
        Ok(())
    }

    pub(super) fn exit_code(&self) -> Result<Option<u32>, HelperError> {
        self.wait_for_exit(Duration::ZERO)
    }

    fn wait_for_exit(&self, timeout: Duration) -> Result<Option<u32>, HelperError> {
        match unsafe { WaitForSingleObject(self.process.as_raw_handle(), timeout.as_millis() as u32) } {
            WAIT_TIMEOUT => Ok(None),
            WAIT_OBJECT_0 => {
                let mut code = 0;
                if unsafe { GetExitCodeProcess(self.process.as_raw_handle(), &mut code) } == 0 {
                    return Err(native_error("Read the input helper exit code"));
                }
                Ok(Some(code))
            }
            _ => Err(native_error("Wait for the input helper")),
        }
    }

    pub(super) fn notify(&mut self, changes: u32) -> Result<(), HelperError> {
        if changes == 0 { return Ok(()); }
        self.channel.notify_environment(changes);
        self.signal()
    }

    pub(super) fn retire(&mut self) {
        drop(self.job.take());
    }

    pub(super) fn shutdown(
        &mut self, limit: Option<Instant>, mut consume: impl FnMut(HelperToCore),
    ) -> Result<(), HelperError> {
        self.channel.request_stop();
        self.signal()?;
        let normal = Instant::now() + CLEANUP_WAIT;
        let deadline = limit.map_or(normal, |limit| normal.min(limit));
        loop {
            self.drain(&mut consume)?;
            if let Some(code) = self.wait_for_exit(WAIT_SLICE)? {
                while !self.drain(&mut consume)? {
                    if Instant::now() >= deadline { return Err(HelperError::ShutdownTimeout); }
                }
                self.poll_status()?;
                return if code == 0 { Ok(()) } else { Err(HelperError::UnexpectedExit(code)) };
            }
            if Instant::now() >= deadline {
                drop(self.job.take());
                return Err(HelperError::ShutdownTimeout);
            }
        }
    }

    fn signal(&self) -> Result<(), HelperError> {
        if unsafe { SetEvent(self.wake.as_raw_handle()) } == 0 {
            return Err(native_error("Wake the input helper"));
        }
        Ok(())
    }
}

impl Drop for InputHelper {
    fn drop(&mut self) {
        // Closing the job kills the helper; the wait for its exit keeps the
        // ordinary cleanup bound (A2).
        drop(self.job.take());
        unsafe { WaitForSingleObject(self.process.as_raw_handle(), CLEANUP_WAIT.as_millis() as u32) };
    }
}

fn configured_job() -> Result<OwnedHandle, HelperError> {
    let job = own(unsafe { CreateJobObjectW(null(), null()) }, "Create the input helper job")?;
    let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
    limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    if unsafe { SetInformationJobObject(job.as_raw_handle(), JobObjectExtendedLimitInformation,
        (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(), size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32) } == 0
    {
        return Err(native_error("Set input helper job limits"));
    }
    Ok(job)
}

fn duplicate(source: HANDLE, access: Option<u32>) -> Result<OwnedHandle, HelperError> {
    let parent = unsafe { GetCurrentProcess() };
    let mut duplicate = null_mut();
    let flags = if access.is_some() { 0 } else { DUPLICATE_SAME_ACCESS };
    if unsafe { DuplicateHandle(parent, source, parent, &mut duplicate, access.unwrap_or(0), 1, flags) } == 0 {
        return Err(native_error("Prepare input helper handle inheritance"));
    }
    own(duplicate, "Prepare input helper handle inheritance")
}

fn own(handle: HANDLE, stage: &'static str) -> Result<OwnedHandle, HelperError> {
    if handle.is_null() {
        return Err(native_error(stage));
    }
    // All callers pass handles just returned by successful creation or duplication.
    Ok(unsafe { OwnedHandle::from_raw_handle(handle) })
}

fn native_error(stage: &'static str) -> HelperError {
    HelperError::Windows(stage, io::Error::last_os_error())
}

/// Owns aligned storage for the Win32 attribute list across CreateProcessW.
/// The caller retains the backing handle array through that call.
struct HandleList(Vec<usize>);

impl HandleList {
    fn new() -> Result<Self, HelperError> {
        let mut bytes = 0;
        unsafe { InitializeProcThreadAttributeList(null_mut(), 1, 0, &mut bytes) };
        if bytes == 0 {
            return Err(native_error("Measure the input helper handle list"));
        }
        let mut entries = vec![0; bytes.div_ceil(size_of::<usize>())];
        if unsafe { InitializeProcThreadAttributeList(entries.as_mut_ptr().cast(), 1, 0, &mut bytes) } == 0 {
            return Err(native_error("Initialize the input helper handle list"));
        }
        Ok(Self(entries))
    }

    fn pointer(&mut self) -> LPPROC_THREAD_ATTRIBUTE_LIST {
        self.0.as_mut_ptr().cast()
    }

    fn set_handles(&mut self, handles: &[HANDLE]) -> Result<(), HelperError> {
        if unsafe {
            UpdateProcThreadAttribute(self.pointer(), 0, PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize,
                handles.as_ptr().cast(), std::mem::size_of_val(handles), null_mut(), null())
        } == 0 {
            return Err(native_error("Restrict inherited input helper handles"));
        }
        Ok(())
    }
}

impl Drop for HandleList {
    fn drop(&mut self) {
        unsafe { DeleteProcThreadAttributeList(self.pointer()) };
    }
}

impl HelperError {
    pub(super) fn runtime_failure(
        self, at: crate::protocol::SessionTimeUs, first_missing: crate::protocol::SessionTimeUs,
    ) -> crate::session::windows::InputFailure {
        use crate::{
            package::{diagnostics::{Failure, FailureComponent}, metadata::InputBlindReason},
            session::windows::InputFailure,
        };
        let (reason, native_code) = match &self {
            Self::UnexpectedExit(code) => (InputBlindReason::HelperExited, Some(*code as i32)),
            Self::ShutdownTimeout => (InputBlindReason::HelperCleanupFailed, None),
            Self::SuspectedHookLoss(_) => (InputBlindReason::HookLost, None),
            Self::ProducerStalled(_) => (InputBlindReason::ProducerStall, None),
            Self::Reported(failure) => {
                let reason = match failure.stage {
                    HelperFailureStage::RawInputRegistration => InputBlindReason::RegistrationLost,
                    HelperFailureStage::KeyboardHook | HelperFailureStage::PointerHook => InputBlindReason::HookLost,
                    _ => InputBlindReason::HelperCommunicationFailed,
                };
                (reason, failure.native_code)
            }
            Self::Windows(_, error) => (InputBlindReason::HelperCommunicationFailed, error.raw_os_error()),
            _ => (InputBlindReason::HelperCommunicationFailed, None),
        };
        let error = super::ProbeError::from(self);
        let operation = error.stage;
        let mut failure = Failure::new(FailureComponent::InputHelper, operation, native_code);
        failure.raw_input_registration = error.raw_input_registration;
        InputFailure { operation, at, first_missing: first_missing.min(at), reason, failure }
    }
}

impl From<HelperError> for super::ProbeError {
    fn from(error: HelperError) -> Self {
        use windows::core::{Error, HRESULT};
        let raw_input_registration = match &error {
            HelperError::Reported(failure) => failure.raw_input_registration,
            _ => None,
        };
        let (stage, code) = match error {
            HelperError::Missing => ("The installed screen-fix-input-helper.exe is missing", None),
            HelperError::Windows(stage, error) => (stage, error.raw_os_error()),
            HelperError::Protocol(error) => {
                let stage = match error {
                    PacketError::UnsupportedVersion(_) => "The installed input helper protocol version differs",
                    _ => "The input helper returned an invalid IPC record",
                };
                (stage, None)
            }
            HelperError::Reported(failure) => {
                let stage = match failure.stage {
                    HelperFailureStage::Startup => "Initialize the input helper",
                    HelperFailureStage::PowerThrottling => "Disable input helper execution-speed power throttling",
                    HelperFailureStage::ClockMismatch => "Match the input helper QPC frequency",
                    HelperFailureStage::RawInputRegistration => "Verify input helper Raw Input registrations",
                    HelperFailureStage::DeviceDiscovery => "Discover input devices",
                    HelperFailureStage::KeyboardHook => "Install or monitor the input helper keyboard hook",
                    HelperFailureStage::PointerHook => "Install or monitor the input helper mouse hook",
                    HelperFailureStage::MessageLoop => "Run the input helper message loop",
                    HelperFailureStage::Transport => "Run input helper IPC",
                };
                (stage, failure.native_code)
            }
            HelperError::UnexpectedExit(_code) => ("The input helper exited unexpectedly", None),
            HelperError::ReadyTimeout => ("The input helper did not report readiness in time", None),
            HelperError::ShutdownTimeout => ("The input helper did not finish normal cleanup in time", None),
            HelperError::SuspectedHookLoss(producer) => (match producer {
                Producer::KeyboardHook => "The keyboard hook stopped producing callbacks during keyboard input",
                Producer::PointerHook => "The mouse hook stopped producing callbacks during mouse input",
                _ => "An input hook stopped producing callbacks",
            }, None),
            HelperError::ProducerStalled(producer) => (match producer {
                Producer::RawInput => "Raw Input progress stalled",
                Producer::DeviceNotifications => "Input device-notification progress stalled",
                Producer::KeyboardHook => "Keyboard-hook progress stalled",
                Producer::PointerHook => "Mouse-hook progress stalled",
                Producer::OptionalHid => "Optional HID progress stalled",
            }, None),
        };
        Self {
            stage,
            source: code.map(|code| Error::from_hresult(HRESULT::from_win32(code as u32))),
            journal: None,
            raw_input_registration,
            owner: Some(crate::package::diagnostics::FailureComponent::InputHelper),
        }
    }
}
