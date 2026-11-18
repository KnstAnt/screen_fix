use std::{
    os::windows::io::{IntoRawHandle, OwnedHandle},
    path::Path,
    sync::Arc,
    time::Instant,
};

use screen_fix_protocol::{
    AttemptId, SessionId,
    ipc::elevated::attachment::{ConnectionId, Role, Scope, Transition},
};

use crate::{
    deadline::{Cancellation, Deadline, READINESS},
    pipe::{self, Pipe},
    process::{Process, verify_linked_pair, verify_medium},
    runtime::{Links, RecorderLanes},
    secret::Secret,
    trust::{ImageRole, Installation},
    win::{Failure, Handle, Result},
    wire::{Bootstrap, LaunchArguments, frequency},
};

/// The prepared bootstrap and pinned installation stay live independently of
/// a potentially blocked consent call.
pub struct RecorderBootstrap {
    installation: Arc<Installation>,
    parent: Process,
    bootstrap_pipe: Pipe,
    scope: Scope,
}

impl RecorderBootstrap {
    pub fn prepare(
        attempt: AttemptId, session: SessionId, cancel: &Cancellation, consent_deadline: Instant,
    ) -> Result<(Self, LaunchRequest)> {
        pipe::available()?;
        check_consent(cancel, consent_deadline)?;
        let installation = Arc::new(Installation::verify(ImageRole::Recorder)?);
        check_consent(cancel, consent_deadline)?;
        let parent = Process::current()?;
        installation.verify_recorder(&parent)?;
        let (sid, server_logon) = verify_medium(&parent)?;
        let mut args = Box::new(LaunchArguments { parent: parent.instance(), rendezvous: Secret::zeroed() });
        args.rendezvous.randomize()?;
        let scope = Scope { attempt, session, release: installation.release.id, qpc_frequency: frequency()? };
        check_consent(cancel, consent_deadline)?;
        let bootstrap_pipe = Pipe::bootstrap(&args.bootstrap(), &sid, &server_logon)?;
        check_consent(cancel, consent_deadline)?;
        let launch = LaunchRequest { installation: Arc::clone(&installation), args };
        Ok((Self { installation, parent, bootstrap_pipe, scope }, launch))
    }

    pub fn authenticate(
        self, original_handle: OwnedHandle, shell_returned: Instant, cancel: &Cancellation,
    ) -> Result<Authenticated> {
        let Self { installation, parent, mut bootstrap_pipe, scope } = self;
        let ready_at = shell_returned + READINESS;
        let initial = Deadline::new(ready_at, Failure::ReadinessDeadline, cancel);
        initial.check()?;
        let handle = unsafe { Handle::take(original_handle.into_raw_handle(), Failure::ConsentLaunch)? };
        let child = Process::inspect(Arc::new(handle))?;
        let deadline = initial.peer(&child);
        installation.verify_collector(&child)?;
        verify_linked_pair(&parent, &child)?;

        bootstrap_pipe.accept(&child, deadline)?;
        let bootstrap = Bootstrap::new(scope)?;
        bootstrap.send(&mut bootstrap_pipe, deadline)?;
        drop(bootstrap_pipe);

        let mut pipes = Vec::with_capacity(Role::ORDER.len());
        for role in Role::ORDER {
            let mut lane = Pipe::client(&bootstrap.name(role)?, deadline)?;
            bootstrap.bind_client(&mut lane, role, deadline)?;
            pipes.push(lane);
        }
        let mut pipes: [Pipe; 3] = pipes.try_into().map_err(|_| Failure::Protocol)?;
        bootstrap.send_seal(&mut pipes[0], Transition::Seal, deadline)?;
        bootstrap.receive_seal(&mut pipes[0], Transition::Authenticated, deadline)?;
        deadline.check()?;
        Ok(Authenticated { scope: bootstrap.scope.clone(), connection: bootstrap.connection, child, pipes })
    }
}

fn check_consent(cancel: &Cancellation, until: Instant) -> Result<()> {
    cancel.check()?;
    if Instant::now() >= until { return Err(Failure::ConsentDeadline) }
    Ok(())
}

/// The fixed shell inputs own all installation guards through the shell call.
pub struct LaunchRequest {
    installation: Arc<Installation>,
    args: Box<LaunchArguments>,
}

impl LaunchRequest {
    pub fn executable(&self) -> &Path { self.installation.collector() }

    pub fn directory(&self) -> &Path { self.installation.directory() }

    pub fn arguments(&self) -> String { self.args.command() }
}

/// Authentication completed on all three retained lanes. Native registration
/// is a later transition. Dropping this value closes the attachment.
pub struct Authenticated {
    pub scope: Scope,
    pub connection: ConnectionId,
    child: Process,
    pipes: [Pipe; 3],
}

impl Authenticated {
    pub fn ensure_live(&self) -> Result<()> { self.child.ensure_live() }
}

impl Authenticated {
    /// Consume the one-shot attachment. Each returned lane direction has
    /// exactly one owner and uses the original process as its peer witness.
    pub fn into_runtime(self) -> Result<RecorderLanes> {
        self.child.ensure_live()?;
        let links = Links::new(self.pipes, self.scope, self.connection, Arc::new(self.child), Arc::new(Cancellation::new()?));
        Ok(links.recorder())
    }
}
