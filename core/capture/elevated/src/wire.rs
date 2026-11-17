use std::{ffi::OsString, num::{NonZeroU32, NonZeroU64}};

use screen_fix_protocol::{
    QpcFrequency,
    ipc::{
        ProcessInstance,
        elevated::attachment::{self, ConnectionId, HEADER_BYTES, Role, SCOPE_BYTES, Scope, Transition},
    },
};
use windows_sys::Win32::System::Performance::QueryPerformanceFrequency;

use crate::{
    deadline::Deadline,
    pipe::{Name, Pipe},
    secret::{Secret, erase, hmac, random},
    win::{Failure, Result},
};

const BOOTSTRAP_MAGIC: [u8; 4] = *b"SFBT";
const BOOTSTRAP_BYTES: usize = 8 + 16 + SCOPE_BYTES + 32;
const MAX_PAYLOAD: usize = Transition::Challenge.payload_len();
const MAX_FRAME: usize = HEADER_BYTES + MAX_PAYLOAD;

pub(crate) struct Bootstrap {
    pub(crate) connection: ConnectionId,
    pub(crate) scope: Scope,
    secret: Secret<32>,
}

impl Bootstrap {
    /// The secret is generated in its final heap location (E4).
    pub(crate) fn new(scope: Scope) -> Result<Box<Self>> {
        let mut connection = [0u8; 16];
        random(&mut connection)?;
        let connection = ConnectionId::new(connection).map_err(|_| Failure::Cryptography)?;
        let mut bootstrap = Box::new(Self { connection, scope, secret: Secret::zeroed() });
        bootstrap.secret.randomize()?;
        Ok(bootstrap)
    }

    pub(crate) fn send(&self, pipe: &mut Pipe, deadline: Deadline<'_>) -> Result<()> {
        let mut record = Secret([0u8; BOOTSTRAP_BYTES]);
        record.0[..4].copy_from_slice(&BOOTSTRAP_MAGIC);
        record.0[4..6].copy_from_slice(&attachment::VERSION.to_le_bytes());
        record.0[8..24].copy_from_slice(&self.connection.as_bytes());
        record.0[24..24 + SCOPE_BYTES].copy_from_slice(&self.scope.encode());
        record.0[24 + SCOPE_BYTES..].copy_from_slice(&self.secret.0);
        pipe.write(&record.0, deadline)
    }

    /// The received secret is copied straight into its final heap location;
    /// the record buffer is erased on drop (E4).
    pub(crate) fn receive(pipe: &mut Pipe, deadline: Deadline<'_>) -> Result<Box<Self>> {
        let mut record = Secret([0u8; BOOTSTRAP_BYTES]);
        pipe.read_exact(&mut record.0, deadline)?;
        pipe.expect_eof(deadline)?;
        if record.0[..4] != BOOTSTRAP_MAGIC
            || record.0[4..6] != attachment::VERSION.to_le_bytes()
            || record.0[6..8] != [0, 0]
        {
            return Err(Failure::Protocol);
        }
        let mut connection = [0; 16];
        connection.copy_from_slice(&record.0[8..24]);
        let connection = ConnectionId::new(connection).map_err(|_| Failure::Protocol)?;
        let scope = Scope::decode(&record.0[24..24 + SCOPE_BYTES]).map_err(|_| Failure::Protocol)?;
        let mut bootstrap = Box::new(Self { connection, scope, secret: Secret::zeroed() });
        bootstrap.secret.0.copy_from_slice(&record.0[24 + SCOPE_BYTES..]);
        if bootstrap.secret.0 == [0; 32] { return Err(Failure::Protocol) }
        Ok(bootstrap)
    }

    pub(crate) fn name(&self, role: Role) -> Result<Name> {
        const DOMAIN: &[u8] = b"ScreenFix/elevation/name/v1\0";
        let mut input = [0u8; DOMAIN.len() + 1];
        let domain = DOMAIN;
        input[..domain.len()].copy_from_slice(domain);
        input[domain.len()] = role as u8;
        let mut digest = Secret::zeroed();
        hmac(&self.secret, &input[..=domain.len()], &mut digest)?;
        Ok(Name::hex("lane.", &digest.0, role.suffix()))
    }

    pub(crate) fn bind_client(&self, pipe: &mut Pipe, role: Role, deadline: Deadline<'_>) -> Result<()> {
        let mut request = Secret([0; MAX_PAYLOAD]);
        request.0[..SCOPE_BYTES].copy_from_slice(&self.scope.encode());
        random(&mut request.0[SCOPE_BYTES..])?;
        self.send_frame(pipe, role, Transition::Challenge, &request.0, deadline)?;
        let mut reply = Secret::zeroed();
        self.receive_frame(pipe, role, Transition::Proof, deadline, &mut reply)?;
        let actual = (&reply.0[..32]).try_into().map_err(|_| Failure::Protocol)?;
        let mut expected = Secret::zeroed();
        self.proof(role, &request.0, &mut expected)?;
        if !crate::secret::matches(&expected.0, actual) { return Err(Failure::Protocol) }
        Ok(())
    }

    pub(crate) fn bind_server(&self, pipe: &mut Pipe, role: Role, deadline: Deadline<'_>) -> Result<()> {
        let mut request = Secret::zeroed();
        self.receive_frame(pipe, role, Transition::Challenge, deadline, &mut request)?;
        if Scope::decode(&request.0[..SCOPE_BYTES]).map_err(|_| Failure::Protocol)? != self.scope
            || request.0[SCOPE_BYTES..].iter().all(|&byte| byte == 0)
        {
            return Err(Failure::Protocol);
        }
        let mut proof = Secret::zeroed();
        self.proof(role, &request.0[..MAX_PAYLOAD], &mut proof)?;
        self.send_frame(pipe, role, Transition::Proof, &proof.0, deadline)
    }

    pub(crate) fn send_seal(&self, control: &mut Pipe, transition: Transition, deadline: Deadline<'_>) -> Result<()> {
        self.send_frame(control, Role::Control, transition, &self.scope.encode(), deadline)
    }

    pub(crate) fn receive_seal(&self, control: &mut Pipe, transition: Transition, deadline: Deadline<'_>) -> Result<()> {
        let mut frame = Secret::zeroed();
        self.receive_frame(control, Role::Control, transition, deadline, &mut frame)?;
        let scope = Scope::decode(&frame.0[..SCOPE_BYTES]).map_err(|_| Failure::Protocol)?;
        if scope != self.scope { return Err(Failure::Protocol) }
        Ok(())
    }

    fn proof(&self, role: Role, request: &[u8], digest: &mut Secret<32>) -> Result<()> {
        const DOMAIN: &[u8] = b"ScreenFix/elevation/proof/v1\0";
        let domain = DOMAIN;
        let mut transcript = Secret([0u8; DOMAIN.len() + 1 + MAX_FRAME]);
        transcript.0[..domain.len()].copy_from_slice(domain);
        transcript.0[domain.len()] = role as u8;
        let body = &mut transcript.0[domain.len() + 1..];
        let header = attachment::header(role, Transition::Challenge, self.connection).map_err(|_| Failure::Protocol)?;
        body[..HEADER_BYTES].copy_from_slice(&header);
        body[HEADER_BYTES..].copy_from_slice(request);
        hmac(&self.secret, &transcript.0, digest)
    }

    fn send_frame(
        &self, pipe: &mut Pipe, role: Role, transition: Transition, payload: &[u8], deadline: Deadline<'_>,
    ) -> Result<()> {
        if payload.len() != transition.payload_len() { return Err(Failure::Protocol) }
        let mut frame = Secret([0u8; MAX_FRAME]);
        let header = attachment::header(role, transition, self.connection).map_err(|_| Failure::Protocol)?;
        frame.0[..HEADER_BYTES].copy_from_slice(&header);
        let length = HEADER_BYTES + payload.len();
        frame.0[HEADER_BYTES..length].copy_from_slice(payload);
        pipe.write(&frame.0[..length], deadline)
    }

    fn receive_frame(
        &self, pipe: &mut Pipe, role: Role, transition: Transition, deadline: Deadline<'_>,
        payload: &mut Secret<MAX_PAYLOAD>,
    ) -> Result<()> {
        let mut frame = Secret([0u8; MAX_FRAME]);
        let expected = HEADER_BYTES + transition.payload_len();
        if pipe.is_server() {
            if pipe.read_message(&mut frame.0, deadline)? != expected {
                return Err(Failure::Protocol);
            }
        } else {
            pipe.read_exact(&mut frame.0[..HEADER_BYTES], deadline)?;
        }
        attachment::check_header(&frame.0[..HEADER_BYTES], role, transition, self.connection)
            .map_err(|_| Failure::Protocol)?;
        if !pipe.is_server() {
            pipe.read_exact(&mut frame.0[HEADER_BYTES..expected], deadline)?;
        }
        payload.0[..transition.payload_len()].copy_from_slice(&frame.0[HEADER_BYTES..expected]);
        Ok(())
    }
}

pub(crate) fn frequency() -> Result<QpcFrequency> {
    let mut value = 0i64;
    if unsafe { QueryPerformanceFrequency(&mut value) } == 0 {
        return Err(Failure::Protocol);
    }
    let value = u64::try_from(value).map_err(|_| Failure::Protocol)?;
    Ok(QpcFrequency(NonZeroU64::new(value).ok_or(Failure::Protocol)?))
}

pub(crate) struct LaunchArguments {
    pub(crate) parent: ProcessInstance,
    pub(crate) rendezvous: Secret<16>,
}

impl LaunchArguments {
    pub(crate) fn command(&self) -> String {
        use std::fmt::Write;
        let mut command = format!("{} {} {} ", attachment::VERSION,
            self.parent.pid, self.parent.creation_filetime);
        for byte in &self.rendezvous.0 {
            write!(&mut command, "{byte:02x}").expect("format rendezvous ID");
        }
        command
    }

    /// The rendezvous ID is decoded into its final heap location, and the
    /// argument text is erased (E4).
    pub(crate) fn parse(mut arguments: impl Iterator<Item = OsString>) -> Result<Box<Self>> {
        fn next(arguments: &mut impl Iterator<Item = OsString>) -> Result<String> {
            arguments.next().and_then(|value| value.into_string().ok()).ok_or(Failure::Arguments)
        }
        fn canonical(s: &str) -> Result<u64> {
            if s.is_empty() || s.starts_with('0') || !s.bytes().all(|c| c.is_ascii_digit()) {
                return Err(Failure::Arguments);
            }
            s.parse().map_err(|_| Failure::Arguments)
        }
        let version = canonical(&next(&mut arguments)?)?;
        if version != u64::from(attachment::VERSION) { return Err(Failure::Arguments) }
        let pid = canonical(&next(&mut arguments)?)?;
        let pid = u32::try_from(pid).ok().and_then(NonZeroU32::new).ok_or(Failure::Arguments)?;
        let birth = canonical(&next(&mut arguments)?)?;
        let mut token = next(&mut arguments)?.into_bytes();
        let parsed = (|| {
            if token.len() != 32 || arguments.next().is_some() { return Err(Failure::Arguments) }
            let hex = |c: u8| match c {
                b'0'..=b'9' => Ok(c - b'0'),
                b'a'..=b'f' => Ok(c - b'a' + 10),
                _ => Err(Failure::Arguments),
            };
            let mut parsed = Box::new(Self {
                parent: ProcessInstance { pid, creation_filetime: birth }, rendezvous: Secret::zeroed(),
            });
            for (byte, pair) in parsed.rendezvous.0.iter_mut().zip(token.chunks_exact(2)) {
                *byte = (hex(pair[0])? << 4) | hex(pair[1])?;
            }
            if parsed.rendezvous.0 == [0; 16] { return Err(Failure::Arguments) }
            Ok(parsed)
        })();
        erase(&mut token);
        parsed
    }

    pub(crate) fn bootstrap(&self) -> Name { Name::hex("boot.", &self.rendezvous.0, ".bootstrap") }
}
