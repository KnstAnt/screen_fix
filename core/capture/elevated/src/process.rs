use std::{mem::{offset_of, size_of, MaybeUninit}, ptr::null_mut, sync::Arc};

use screen_fix_protocol::{
    TokenFact, TokenIdentityCheck as Check, TokenIdentityFailure, TokenSubject,
    ipc::ProcessInstance,
};
use windows_sys::Win32::{
    Foundation::{ERROR_INSUFFICIENT_BUFFER, GetLastError, HANDLE, LUID, WAIT_TIMEOUT},
    Security::{
        EqualSid, GetLengthSid, GetSidSubAuthority, GetSidSubAuthorityCount, GetTokenInformation,
        IsValidSid, IsWellKnownSid, PSID, SID_AND_ATTRIBUTES, TOKEN_ELEVATION, TOKEN_GROUPS, TOKEN_LINKED_TOKEN,
        TOKEN_MANDATORY_LABEL, TOKEN_QUERY, TOKEN_STATISTICS, TOKEN_USER, TokenElevation,
        TokenElevationType, TokenElevationTypeFull, TokenElevationTypeLimited, TokenIntegrityLevel,
        TokenGroups, TokenLinkedToken, TokenLogonSid, TokenSessionId, TokenStatistics, TokenUser,
        WinInteractiveSid, WinLocalSystemSid,
    },
    System::Threading::{
        GetCurrentProcessId, GetProcessId, GetProcessInformation, OpenProcess, OpenProcessToken,
        PROCESS_PROTECTION_LEVEL_INFORMATION, PROTECTION_LEVEL_NONE, ProcessProtectionLevelInfo,
        PROCESS_NAME_FORMAT, PROCESS_NAME_NATIVE, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
        QueryFullProcessImageNameW, WaitForSingleObject,
    },
};

use crate::win::{Failure, Handle, Result, read_wide};

const SYNCHRONIZE: u32 = 0x0010_0000;
const MEDIUM: u32 = 0x2000;
const HIGH: u32 = 0x3000;

fn identity_failure(subject: TokenSubject, check: Check, native_code: Option<u32>) -> Failure {
    Failure::TokenIdentity(TokenIdentityFailure { subject, check, native_code })
}

pub(crate) struct Process {
    handle: Arc<Handle>,
    instance: ProcessInstance,
    native_image: Vec<u16>,
}

impl Process {
    pub(crate) fn current() -> Result<Self> {
        Self::open(unsafe { GetCurrentProcessId() })
    }

    pub(crate) fn open_handle(pid: u32) -> Result<Arc<Handle>> {
        let raw = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION | SYNCHRONIZE, 0, pid) };
        let handle = unsafe { Handle::take(raw, Failure::ProcessIdentity)? };
        Ok(Arc::new(handle))
    }

    pub(crate) fn open(pid: u32) -> Result<Self> {
        let process = Self::inspect(Self::open_handle(pid)?)?;
        if process.pid() != pid {
            return Err(Failure::ProcessIdentity);
        }
        Ok(process)
    }

    pub(crate) fn inspect(handle: Arc<Handle>) -> Result<Self> {
        let pid = std::num::NonZeroU32::new(unsafe { GetProcessId(handle.raw()) })
            .ok_or(Failure::ProcessIdentity)?;
        let instance = screen_fix_native_observation::process_instance(handle.raw(), pid).ok_or(Failure::ProcessIdentity)?;
        let native_image = image_name(handle.raw(), PROCESS_NAME_NATIVE)?;
        let process = Self { handle, instance, native_image };
        process.ensure_live()?;
        Ok(process)
    }

    pub(crate) fn pid(&self) -> u32 { self.instance.pid.get() }

    pub(crate) fn instance(&self) -> ProcessInstance { self.instance }

    pub(crate) fn native_image(&self) -> &[u16] { &self.native_image }

    pub(crate) fn raw(&self) -> HANDLE { self.handle.raw() }

    pub(crate) fn ensure_live(&self) -> Result<()> {
        if unsafe { WaitForSingleObject(self.raw(), 0) } != WAIT_TIMEOUT {
            return Err(Failure::PeerExited);
        }
        Ok(())
    }

    pub(crate) fn matches_win32_image(&self, claimed: &str) -> Result<bool> {
        let actual = self.win32_image()?;
        Ok(!actual.is_empty() && actual.iter().copied().eq(claimed.encode_utf16()))
    }

    /// The Win32 image path Windows reports for this live process.
    pub(crate) fn win32_image(&self) -> Result<Vec<u16>> {
        image_name(self.raw(), PROCESS_NAME_WIN32)
    }

    pub(crate) fn session(&self) -> Result<u32> {
        self.token(TokenSubject::Process)?.fixed(TokenSessionId, TokenFact::Session)
    }

    fn unprotected(&self, subject: TokenSubject) -> Result<()> {
        let mut info = PROCESS_PROTECTION_LEVEL_INFORMATION { ProtectionLevel: 0 };
        if unsafe { GetProcessInformation(self.raw(), ProcessProtectionLevelInfo,
            (&mut info as *mut PROCESS_PROTECTION_LEVEL_INFORMATION).cast(), size_of::<PROCESS_PROTECTION_LEVEL_INFORMATION>() as u32) } == 0
        {
            let code = unsafe { GetLastError() };
            return Err(identity_failure(subject, Check::QueryProcessProtection, Some(code)));
        }
        if info.ProtectionLevel != PROTECTION_LEVEL_NONE {
            return Err(identity_failure(subject, Check::ProtectedProcess, None));
        }
        Ok(())
    }

    fn token(&self, subject: TokenSubject) -> Result<Token> {
        let mut token = null_mut();
        if unsafe { OpenProcessToken(self.raw(), TOKEN_QUERY, &mut token) } == 0 {
            let code = unsafe { GetLastError() };
            return Err(identity_failure(subject, Check::OpenProcessToken, Some(code)));
        }
        let handle = unsafe { Handle::take(token, identity_failure(subject, Check::InvalidTokenHandle, None))? };
        Ok(Token { handle, subject })
    }
}

/// The image path of a process in the requested format, sized to fit. An
/// empty path is a failure.
fn image_name(process: HANDLE, format: PROCESS_NAME_FORMAT) -> Result<Vec<u16>> {
    let mut path = read_wide(|buffer, capacity| {
        let mut length = capacity;
        if unsafe { QueryFullProcessImageNameW(process, format, buffer, &mut length) } == 0 { 0 } else { length }
    }).ok_or(Failure::ProcessIdentity)?;
    path.shrink_to_fit();
    Ok(path)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForegroundAccess {
    Ordinary,
    Elevated,
}

/// Select the standard source when the Recorder already has the required access.
/// A protected visit still requires the collector's reciprocal split-token proof.
/// The Recorder owns window, desktop and profile proof for either source.
pub fn foreground_access(instance: ProcessInstance, image: &str) -> Result<ForegroundAccess> {
    let recorder = Process::current()?;
    let target = Process::open(instance.pid.get())?;
    if target.instance() != instance || !target.matches_win32_image(image)? {
        return Err(Failure::TargetChanged);
    }
    target.unprotected(TokenSubject::Foreground)?;
    let recorder_token = recorder.token(TokenSubject::Recorder)?;
    let target_token = target.token(TokenSubject::Foreground)?;
    let recorder_scope = recorder_token.scope()?;
    let target_scope = target_token.scope()?;
    if recorder_scope.session == 0 {
        return Err(identity_failure(TokenSubject::Recorder, Check::NoInteractiveSession, None));
    }
    if target_scope.session != recorder_scope.session {
        return Err(identity_failure(TokenSubject::Foreground, Check::DifferentSession, None));
    }
    if !recorder_scope.interactive {
        return Err(identity_failure(TokenSubject::Recorder, Check::NotInteractive, None));
    }
    if !target_scope.interactive {
        return Err(identity_failure(TokenSubject::Foreground, Check::NotInteractive, None));
    }
    if !recorder_scope.user.matches(target_scope.user.raw()) {
        return Err(identity_failure(TokenSubject::Foreground, Check::DifferentUser, None));
    }
    let access = if recorder_scope.elevated {
        verify_inherited_access(&recorder_token, &recorder_scope, &target_token, &target_scope)?;
        ForegroundAccess::Ordinary
    } else if !target_scope.elevated && target_scope.integrity <= recorder_scope.integrity {
        ForegroundAccess::Ordinary
    } else {
        verify_capture_target_for(
            &recorder, &target, TokenSubject::Foreground, TokenSubject::ForegroundLinkedToken,
        )?;
        ForegroundAccess::Elevated
    };
    target.ensure_live()?;
    Ok(access)
}

pub(crate) struct Sid(Vec<u32>);

impl Sid {
    /// The supplied SID lives in a Windows-owned or validated structure.
    pub(crate) unsafe fn copy(raw: PSID) -> Option<Self> {
        if raw.is_null() || unsafe { IsValidSid(raw) } == 0 {
            return None;
        }
        let bytes = unsafe { GetLengthSid(raw) } as usize;
        if bytes == 0 || bytes % size_of::<u32>() != 0 {
            return None;
        }
        let mut data = vec![0u32; bytes / size_of::<u32>()];
        unsafe { std::ptr::copy_nonoverlapping(raw.cast::<u8>(), data.as_mut_ptr().cast(), bytes) };
        Some(Self(data))
    }

    pub(crate) fn raw(&self) -> PSID { self.0.as_ptr().cast_mut().cast() }

    pub(crate) fn matches(&self, raw: PSID) -> bool {
        !raw.is_null() && unsafe { EqualSid(self.raw(), raw) } != 0
    }
}

struct Token {
    handle: Handle,
    subject: TokenSubject,
}

impl Token {
    fn failure(&self, check: Check, native_code: Option<u32>) -> Failure {
        identity_failure(self.subject, check, native_code)
    }

    fn fixed<T: FixedTokenInformation>(&self, class: i32, fact: TokenFact) -> Result<T> {
        let mut value = MaybeUninit::<T>::uninit();
        let len = size_of::<T>() as u32;
        let mut returned = 0;
        if unsafe { GetTokenInformation(self.handle.raw(), class, value.as_mut_ptr().cast(), len, &mut returned) } == 0 {
            let code = unsafe { GetLastError() };
            return Err(self.failure(Check::ReadToken { class: fact }, Some(code)));
        }
        if returned != len {
            return Err(self.failure(Check::TokenShape { class: fact }, None));
        }
        // SAFETY: Windows wrote all `len` bytes, and every bit pattern is a
        // valid `T` (`FixedTokenInformation`).
        Ok(unsafe { value.assume_init() })
    }

    fn variable(&self, class: i32, fact: TokenFact) -> Result<Vec<usize>> {
        let mut bytes = 0;
        let probe = unsafe { GetTokenInformation(self.handle.raw(), class, null_mut(), 0, &mut bytes) };
        let probe_error = (probe == 0).then(|| unsafe { GetLastError() });
        if bytes == 0 || bytes > 65536 {
            if let Some(code) = probe_error.filter(|code| *code != ERROR_INSUFFICIENT_BUFFER) {
                return Err(self.failure(Check::QueryTokenSize { class: fact }, Some(code)));
            }
            return Err(self.failure(Check::TokenShape { class: fact }, None));
        }
        let mut buffer = vec![0usize; (bytes as usize).div_ceil(size_of::<usize>())];
        let mut returned = 0;
        if unsafe {
            GetTokenInformation(self.handle.raw(), class, buffer.as_mut_ptr().cast(), bytes, &mut returned)
        } == 0 {
            let code = unsafe { GetLastError() };
            return Err(self.failure(Check::ReadToken { class: fact }, Some(code)));
        }
        if returned != bytes {
            return Err(self.failure(Check::TokenShape { class: fact }, None));
        }
        Ok(buffer)
    }

    pub(crate) fn user(&self) -> Result<Sid> {
        let data = self.variable(TokenUser, TokenFact::User)?;
        let user = unsafe { &*data.as_ptr().cast::<TOKEN_USER>() };
        unsafe { Sid::copy(user.User.Sid) }.ok_or_else(|| self.failure(Check::TokenShape { class: TokenFact::User }, None))
    }

    fn logon_sid(&self) -> Result<Sid> {
        let data = self.variable(TokenLogonSid, TokenFact::LogonSid)?;
        let groups = unsafe { &*data.as_ptr().cast::<TOKEN_GROUPS>() };
        let invalid = || self.failure(Check::TokenShape { class: TokenFact::LogonSid }, None);
        if groups.GroupCount != 1 {
            return Err(invalid());
        }
        unsafe { Sid::copy(groups.Groups[0].Sid) }.ok_or_else(invalid)
    }

    fn integrity(&self) -> Result<u32> {
        let data = self.variable(TokenIntegrityLevel, TokenFact::Integrity)?;
        let label = unsafe { &*data.as_ptr().cast::<TOKEN_MANDATORY_LABEL>() };
        let sid = label.Label.Sid;
        let invalid = || self.failure(Check::TokenShape { class: TokenFact::Integrity }, None);
        if sid.is_null() || unsafe { IsValidSid(sid) } == 0 {
            return Err(invalid());
        }
        let count = unsafe { *GetSidSubAuthorityCount(sid) };
        if count != 1 {
            return Err(invalid());
        }
        Ok(unsafe { *GetSidSubAuthority(sid, 0) })
    }

    fn interactive(&self) -> Result<bool> {
        const ENABLED: u32 = 4;
        let data = self.variable(TokenGroups, TokenFact::Groups)?;
        let groups = unsafe { &*data.as_ptr().cast::<TOKEN_GROUPS>() };
        let offset = offset_of!(TOKEN_GROUPS, Groups);
        let maximum = (data.len() * size_of::<usize>()).saturating_sub(offset) / size_of::<SID_AND_ATTRIBUTES>();
        let count = groups.GroupCount as usize;
        if count > maximum {
            return Err(self.failure(Check::TokenShape { class: TokenFact::Groups }, None));
        }
        let list = unsafe { std::slice::from_raw_parts(data.as_ptr().cast::<u8>().add(offset).cast::<SID_AND_ATTRIBUTES>(), count) };
        Ok(list.iter().any(|group| {
            group.Attributes & ENABLED != 0 && unsafe { IsWellKnownSid(group.Sid, WinInteractiveSid) } != 0
        }))
    }

    fn linked(&self, subject: TokenSubject) -> Result<Token> {
        let info: TOKEN_LINKED_TOKEN = self.fixed(TokenLinkedToken, TokenFact::LinkedToken)?;
        let handle = unsafe { Handle::take(info.LinkedToken, identity_failure(subject, Check::InvalidTokenHandle, None))? };
        Ok(Token { handle, subject })
    }

    fn scope(&self) -> Result<TokenScope> {
        let statistics: TOKEN_STATISTICS = self.fixed(TokenStatistics, TokenFact::Statistics)?;
        let elevation: TOKEN_ELEVATION = self.fixed(TokenElevation, TokenFact::Elevation)?;
        Ok(TokenScope {
            user: self.user()?,
            logon: self.logon_sid()?,
            authentication: statistics.AuthenticationId,
            session: self.fixed(TokenSessionId, TokenFact::Session)?,
            elevation_type: self.fixed(TokenElevationType, TokenFact::ElevationType)?,
            elevated: elevation.TokenIsElevated != 0,
            integrity: self.integrity()?,
            interactive: self.interactive()?,
        })
    }
}

/// A fixed-size token information structure.
///
/// # Safety
/// Every bit pattern of the type's size must be a valid value of it.
unsafe trait FixedTokenInformation {}

// SAFETY: plain integers and C structures of integers and raw handles.
unsafe impl FixedTokenInformation for u32 {}
unsafe impl FixedTokenInformation for i32 {}
unsafe impl FixedTokenInformation for TOKEN_STATISTICS {}
unsafe impl FixedTokenInformation for TOKEN_ELEVATION {}
unsafe impl FixedTokenInformation for TOKEN_LINKED_TOKEN {}

struct TokenScope {
    user: Sid,
    logon: Sid,
    authentication: LUID,
    session: u32,
    elevation_type: i32,
    elevated: bool,
    integrity: u32,
    interactive: bool,
}

impl TokenScope {
    fn same_sign_in(&self, other: &Self) -> bool {
        self.user.matches(other.user.raw())
            && self.logon.matches(other.logon.raw())
            && self.authentication.HighPart == other.authentication.HighPart
            && self.authentication.LowPart == other.authentication.LowPart
            && self.session == other.session
    }

    fn matches_token_scope(&self, other: &Self) -> bool {
        self.same_sign_in(other)
            && self.elevation_type == other.elevation_type
            && self.elevated == other.elevated
            && self.integrity == other.integrity
            && self.interactive == other.interactive
    }
}

fn verify_inherited_access(
    recorder: &Token, recorder_scope: &TokenScope, target: &Token, target_scope: &TokenScope,
) -> Result<()> {
    if unsafe { IsWellKnownSid(recorder_scope.user.raw(), WinLocalSystemSid) } != 0 {
        return Err(recorder.failure(Check::SystemAccount, None));
    }
    if target_scope.integrity > recorder_scope.integrity {
        return Err(target.failure(Check::UnsupportedIntegrity, None));
    }
    // A full administrator sign-in can have no linked token. Only consult
    // linked tokens when the live scopes are the two halves of a split sign-in.
    if recorder_scope.same_sign_in(target_scope) {
        return Ok(());
    }
    if split_scope(target_scope, recorder_scope) {
        return verify_reciprocal_links(
            target, target_scope, TokenSubject::ForegroundLinkedToken,
            recorder, recorder_scope, TokenSubject::RecorderLinkedToken,
        );
    }
    Err(target.failure(Check::DifferentLogon, None))
}

fn split_scope(medium: &TokenScope, high: &TokenScope) -> bool {
    !medium.elevated
        && medium.elevation_type == TokenElevationTypeLimited
        && medium.integrity == MEDIUM
        && high.elevated
        && high.elevation_type == TokenElevationTypeFull
        && high.integrity == HIGH
        && unsafe { IsWellKnownSid(high.user.raw(), WinLocalSystemSid) } == 0
        && medium.user.matches(high.user.raw())
        && medium.interactive
        && high.interactive
        && medium.session != 0
        && medium.session == high.session
}

pub(crate) fn verify_medium(process: &Process) -> Result<(Sid, Sid)> {
    let token = process.token(TokenSubject::Recorder)?;
    let medium = token.scope()?;
    let high = token.linked(TokenSubject::RecorderLinkedToken)?.scope()?;
    if !split_scope(&medium, &high) {
        return Err(identity_failure(TokenSubject::TokenPair, Check::UnsupportedSplitPair, None));
    }
    process.ensure_live()?;
    Ok((medium.user, medium.logon))
}

pub(crate) fn verify_linked_pair(medium: &Process, high: &Process) -> Result<Sid> {
    verify_linked_pair_with_subject(medium, high, TokenSubject::Collector, TokenSubject::CollectorLinkedToken)
}

fn verify_linked_pair_with_subject(
    medium: &Process, high: &Process, high_subject: TokenSubject, linked_subject: TokenSubject,
) -> Result<Sid> {
    let medium_token = medium.token(TokenSubject::Recorder)?;
    let high_token = high.token(high_subject)?;
    let medium_scope = medium_token.scope()?;
    let high_scope = high_token.scope()?;
    if !split_scope(&medium_scope, &high_scope) {
        return Err(identity_failure(TokenSubject::TokenPair, Check::UnsupportedSplitPair, None));
    }
    verify_reciprocal_links(
        &medium_token, &medium_scope, TokenSubject::RecorderLinkedToken,
        &high_token, &high_scope, linked_subject,
    )?;
    medium.ensure_live()?;
    high.ensure_live()?;
    Ok(medium_scope.user)
}

fn verify_reciprocal_links(
    medium: &Token, medium_scope: &TokenScope, medium_link: TokenSubject,
    high: &Token, high_scope: &TokenScope, high_link: TokenSubject,
) -> Result<()> {
    if !medium_scope.matches_token_scope(&high.linked(high_link)?.scope()?) {
        return Err(identity_failure(high_link, Check::LinkedLogonMismatch, None));
    }
    if !high_scope.matches_token_scope(&medium.linked(medium_link)?.scope()?) {
        return Err(identity_failure(medium_link, Check::LinkedLogonMismatch, None));
    }
    Ok(())
}

pub(crate) fn verify_capture_target(medium: &Process, target: &Process) -> Result<()> {
    verify_capture_target_for(
        medium, target, TokenSubject::CaptureTarget, TokenSubject::CaptureTargetLinkedToken,
    )
}

fn verify_capture_target_for(
    medium: &Process, target: &Process, subject: TokenSubject, linked_subject: TokenSubject,
) -> Result<()> {
    // The protection level of a live process does not change.
    target.unprotected(subject)?;
    verify_linked_pair_with_subject(medium, target, subject, linked_subject)?;
    target.ensure_live()
}
