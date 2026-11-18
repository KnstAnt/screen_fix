use std::{
    os::windows::io::{AsHandle, AsRawHandle, BorrowedHandle, FromRawHandle, OwnedHandle},
    process::ExitCode,
    time::{Duration, Instant},
};

use screen_fix_native_observation::{
    channel::HelperEndpoint,
    input::{self, Collector, NativeError, Settings, last_error, loss::{self, Losses, PendingStatus}},
};
use screen_fix_protocol::ipc::{
    CoreToHelper, HelperFailureStage, HelperToCore,
    InputRegistration, PacketError, Producer,
};
use windows_sys::Win32::{
    Foundation::{GetHandleInformation, HANDLE, HANDLE_FLAG_INHERIT, SetHandleInformation,
        WAIT_OBJECT_0, WAIT_TIMEOUT},
    System::Threading::WaitForSingleObject,
};

const FINAL_FLUSH: Duration = Duration::from_millis(400);
const FINAL_FLUSH_POLL_MS: u32 = 4;

pub(super) fn run_child() -> ExitCode {
    let Some([mapping, wake, parent]) = inherited_handles() else { return ExitCode::FAILURE };
    // The parent passed this mapping through CreateProcess's explicit
    // inheritance list. Each process has just one owning endpoint.
    let Ok(channel) = (unsafe { HelperEndpoint::attach(mapping.as_handle()) }) else {
        return ExitCode::FAILURE;
    };
    drop(mapping);
    let mut output = Output::new(channel);
    let result = receive_settings(&mut output).and_then(|settings| {
        input::run(settings, parent.as_handle(), wake.as_handle(), &mut output)
    });
    if let Err(error) = result {
        let _ = output.status(HelperToCore::Failed(error.into_helper_failure()));
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

fn inherited_handles() -> Option<[OwnedHandle; 3]> {
    let mut args = std::env::args_os().skip(1);
    if args.next()?.to_str()? != "--recorder-child" { return None; }
    let handles: [usize; 3] = [
        args.next()?.to_str()?.parse().ok()?,
        args.next()?.to_str()?.parse().ok()?,
        args.next()?.to_str()?.parse().ok()?,
    ];
    if args.next().is_some() || handles.contains(&0) || handles.contains(&usize::MAX)
        || handles[0] == handles[1] || handles[0] == handles[2] || handles[1] == handles[2]
    {
        return None;
    }
    for value in handles {
        let mut attributes = 0;
        let handle = value as HANDLE;
        if unsafe { GetHandleInformation(handle, &mut attributes) } == 0
            || attributes & HANDLE_FLAG_INHERIT == 0
            || unsafe { SetHandleInformation(handle, HANDLE_FLAG_INHERIT, 0) } == 0
        {
            return None;
        }
    }
    // The successful checks above prove three distinct, live inherited handles
    // in this child. Ownership transfers once and their inheritance is cleared.
    Some(handles.map(|value| unsafe { OwnedHandle::from_raw_handle(value as HANDLE) }))
}

struct Output {
    channel: HelperEndpoint,
    losses: Losses,
    /// Status records waiting for queue room (INP-14, INP-15, M1-05).
    status: PendingStatus,
}

impl Output {
    fn new(channel: HelperEndpoint) -> Self {
        Self { channel, losses: Losses::default(), status: PendingStatus::default() }
    }

    fn status(&mut self, record: HelperToCore) -> Result<(), NativeError> {
        if self.channel.send_status(&record).map_err(|_| transport_error())? {
            Ok(())
        } else {
            Err(transport_error())
        }
    }
}

fn receive_settings(output: &mut Output) -> Result<Settings, NativeError> {
    let Some(CoreToHelper::Configure(config)) = output.channel.receive_configuration()
        .map_err(|_| transport_error())? else {
        return Err(transport_error());
    };
    Ok(Settings { qpc_frequency: config.qpc_frequency, pen_hid: config.pen_hid })
}

impl Collector for Output {
    fn stop_requested(&self) -> bool {
        self.channel.stop_requested()
    }

    fn take_environment(&mut self) -> u32 {
        self.channel.take_environment()
    }

    fn ready(&mut self, registration: InputRegistration) -> Result<(), NativeError> {
        self.status(HelperToCore::Ready(registration))
    }

    fn flush(&mut self) -> Result<(), NativeError> {
        self.flush_pending().map(|_| ())
    }

    fn finish(&mut self, parent: BorrowedHandle<'_>) -> Result<(), NativeError> {
        self.flush_final(parent)
    }

    fn offer(&mut self, record: HelperToCore) -> Result<(), NativeError> {
        // A status record waits for queue room behind the pending ones.
        let record = match self.status.defer(record) {
            Ok(()) => return self.flush_status().map(|_| ()),
            Err(record) => record,
        };
        if let Some(producer) = loss::producer(&record)
            && !self.flush_loss(producer)?
        {
            self.losses.drop_record(&record);
            return Ok(());
        }
        match self.channel.send(&record) {
            Ok(true) => {}
            Ok(false) => self.losses.drop_record(&record),
            // A record that cannot be encoded is skipped with a counted gap.
            // Only a corrupt queue breaks the transport.
            Err(PacketError::InvalidCursor) => return Err(transport_error()),
            Err(_) => self.losses.malformed_record(&record),
        }
        Ok(())
    }
}

impl Output {
    fn flush_loss(&mut self, producer: Producer) -> Result<bool, NativeError> {
        let Self { channel, losses, .. } = self;
        losses.flush(producer, |gap| channel.send(&HelperToCore::Gap(gap)).map_err(|_| transport_error()))
    }

    /// A status record that cannot be encoded is skipped, as in `offer`.
    /// Only a corrupt queue breaks the transport.
    fn flush_status(&mut self) -> Result<bool, NativeError> {
        let Self { channel, status, .. } = self;
        status.flush(|record| match channel.send(&record) {
            Ok(sent) => Ok(sent),
            Err(PacketError::InvalidCursor) => Err(transport_error()),
            Err(_) => Ok(true),
        })
    }

    fn flush_pending(&mut self) -> Result<bool, NativeError> {
        for producer in Producer::ALL {
            if !self.flush_loss(producer)? { return Ok(false); }
        }
        self.flush_status()
    }

    fn flush_final(&mut self, parent: BorrowedHandle<'_>) -> Result<(), NativeError> {
        let deadline = Instant::now() + FINAL_FLUSH;
        while !self.flush_pending()? {
            if Instant::now() >= deadline { return Err(transport_error()); }
            match unsafe { WaitForSingleObject(parent.as_raw_handle(), FINAL_FLUSH_POLL_MS) } {
                WAIT_OBJECT_0 => return Ok(()),
                WAIT_TIMEOUT => {}
                _ => return Err(last_error(HelperFailureStage::Transport)),
            }
        }
        Ok(())
    }
}

fn transport_error() -> NativeError {
    NativeError::without_code(HelperFailureStage::Transport)
}
