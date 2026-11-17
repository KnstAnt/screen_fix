use std::{mem::{size_of, transmute}, path::Path, ptr::{null, null_mut}};

use windows_sys::{
    Win32::{
        Foundation::{FreeLibrary, HANDLE, HMODULE},
        Security::{
            Cryptography::{
                BCryptHash, BCRYPT_SHA256_ALG_HANDLE, CERT_PUBLIC_KEY_INFO, CryptEncodeObjectEx,
                X509_ASN_ENCODING, X509_PUBLIC_KEY_INFO,
            },
            WinTrust::{
                CRYPT_PROVIDER_SGNR, WINTRUST_ACTION_GENERIC_VERIFY_V2, WINTRUST_DATA,
                WINTRUST_DATA_0, WINTRUST_FILE_INFO, WTD_CACHE_ONLY_URL_RETRIEVAL, WTD_CHOICE_FILE,
                WTD_DISABLE_MD2_MD4, WTD_REVOKE_WHOLECHAIN, WTD_REVOCATION_CHECK_CHAIN_EXCLUDE_ROOT,
                WTD_STATEACTION_CLOSE, WTD_STATEACTION_VERIFY, WTD_UI_NONE, WinVerifyTrustEx,
            },
        },
        System::LibraryLoader::{
            FindResourceW, GetModuleHandleW, GetProcAddress, LOAD_LIBRARY_AS_DATAFILE_EXCLUSIVE,
            LOAD_LIBRARY_AS_IMAGE_RESOURCE, LOAD_LIBRARY_SEARCH_SYSTEM32, LoadLibraryExW,
            LoadResource, LockResource, SizeofResource,
        },
    },
    core::BOOL,
};

use crate::win::{Failure, Handle, Result, wide};

// CRYPT_PROVIDER_DATA is owned by WinTrust and is only passed back to it.
#[repr(C)]
struct CryptProviderData {
    _opaque: [u8; 0],
}

type GetProviderData = unsafe extern "system" fn(HANDLE) -> *mut CryptProviderData;
type GetProviderSigner =
    unsafe extern "system" fn(*mut CryptProviderData, u32, BOOL, u32) -> *mut CRYPT_PROVIDER_SGNR;

struct WinTrustModule(HMODULE);

impl WinTrustModule {
    fn load() -> Option<Self> {
        let name = wide("wintrust.dll");
        let module = unsafe { LoadLibraryExW(name.as_ptr(), null_mut(), LOAD_LIBRARY_SEARCH_SYSTEM32) };
        (!module.is_null()).then(|| Self(module))
    }
}

impl Drop for WinTrustModule {
    fn drop(&mut self) {
        unsafe { FreeLibrary(self.0) };
    }
}

#[derive(Clone, Copy)]
pub(crate) enum ImageRole {
    Recorder = 1,
    Ordinary = 2,
    Collector = 3,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct Release {
    publisher: [u8; 32],
    pub(crate) id: [u8; 16],
}

impl Release {
    pub(crate) fn current(role: ImageRole) -> Result<Self> {
        let current = unsafe { GetModuleHandleW(null()) };
        if current.is_null() { return Err(Failure::DeploymentRequired) }
        Self::read(current, role).ok_or(Failure::DeploymentRequired)
    }

    fn read(module: HMODULE, role: ImageRole) -> Option<Self> {
        // MAKEINTRESOURCEW(271) in RT_RCDATA (10).
        let resource = unsafe { FindResourceW(module, 271usize as *const u16, 10usize as *const u16) };
        if resource.is_null() || unsafe { SizeofResource(module, resource) } != 52 {
            return None;
        }
        let loaded = unsafe { LoadResource(module, resource) };
        if loaded.is_null() { return None }
        let raw = unsafe { LockResource(loaded) };
        let data = unsafe { raw.cast::<[u8; 52]>().as_ref() }?;
        if data[..4] != [1, role as u8, 0, 0] { return None }
        let publisher = data[4..36].try_into().ok()?;
        let id = data[36..].try_into().ok()?;
        if publisher == [0; 32] || id == [0; 16] { return None }
        Some(Self { publisher, id })
    }

    pub(super) fn verify(self, handle: &Handle, path: &Path, role: ImageRole) -> Result<()> {
        let name = wide(path);
        let mut file = WINTRUST_FILE_INFO {
            cbStruct: size_of::<WINTRUST_FILE_INFO>() as u32,
            pcwszFilePath: name.as_ptr(),
            hFile: handle.raw(),
            pgKnownSubject: null_mut(),
        };
        let mut trust = WINTRUST_DATA {
            cbStruct: size_of::<WINTRUST_DATA>() as u32,
            dwUIChoice: WTD_UI_NONE,
            fdwRevocationChecks: WTD_REVOKE_WHOLECHAIN,
            dwUnionChoice: WTD_CHOICE_FILE,
            Anonymous: WINTRUST_DATA_0 { pFile: &mut file },
            dwStateAction: WTD_STATEACTION_VERIFY,
            dwProvFlags: WTD_CACHE_ONLY_URL_RETRIEVAL | WTD_REVOCATION_CHECK_CHAIN_EXCLUDE_ROOT | WTD_DISABLE_MD2_MD4,
            ..Default::default()
        };
        let mut policy = WINTRUST_ACTION_GENERIC_VERIFY_V2;
        let success = unsafe { WinVerifyTrustEx(null_mut(), &mut policy, &mut trust) } == 0;
        let module = success.then(WinTrustModule::load).flatten();
        let publisher = module.as_ref().ok_or(Failure::InstalledSignature)
            .and_then(|module| signer_key(&trust, module));
        trust.dwStateAction = WTD_STATEACTION_CLOSE;
        unsafe { WinVerifyTrustEx(null_mut(), &mut policy, &mut trust) };
        drop(module);
        if publisher? != self.publisher {
            return Err(Failure::InstalledSignature);
        }

        // Data-file/image-resource mapping never runs another image or resolves its imports.
        // The already open file and ancestor guards prevent replacement between the reads.
        let image = unsafe {
            LoadLibraryExW(name.as_ptr(), null_mut(), LOAD_LIBRARY_AS_DATAFILE_EXCLUSIVE | LOAD_LIBRARY_AS_IMAGE_RESOURCE)
        };
        if image.is_null() { return Err(Failure::ReleaseMismatch) }
        let peer = Self::read(image, role);
        unsafe { FreeLibrary(image) };
        if peer != Some(self) { return Err(Failure::ReleaseMismatch) }
        Ok(())
    }
}

fn signer_key(trust: &WINTRUST_DATA, module: &WinTrustModule) -> Result<[u8; 32]> {
    // WinTrust requires runtime lookup for these two exports. Their signatures
    // are the documented WinTrust ABI, and the module and state remain live.
    let get_data = unsafe { GetProcAddress(module.0, c"WTHelperProvDataFromStateData".as_ptr().cast()) }
        .ok_or(Failure::InstalledSignature)?;
    let get_signer = unsafe { GetProcAddress(module.0, c"WTHelperGetProvSignerFromChain".as_ptr().cast()) }
        .ok_or(Failure::InstalledSignature)?;
    let get_data: GetProviderData = unsafe { transmute(get_data) };
    let get_signer: GetProviderSigner = unsafe { transmute(get_signer) };
    let data = unsafe { get_data(trust.hWVTStateData) };
    if data.is_null() {
        return Err(Failure::InstalledSignature);
    }
    let signer = unsafe { get_signer(data, 0, 0, 0).as_ref() }
        .ok_or(Failure::InstalledSignature)?;
    if signer.dwError != 0 || signer.csCertChain == 0 {
        return Err(Failure::InstalledSignature);
    }
    let certificate = unsafe { signer.pasCertChain.as_ref().and_then(|cert| cert.pCert.as_ref())
        .and_then(|context| context.pCertInfo.as_ref()) }
        .ok_or(Failure::InstalledSignature)?;
    spki_hash(&certificate.SubjectPublicKeyInfo)
}

fn spki_hash(key: &CERT_PUBLIC_KEY_INFO) -> Result<[u8; 32]> {
    let mut length = 0;
    let source = (key as *const CERT_PUBLIC_KEY_INFO).cast();
    if unsafe { CryptEncodeObjectEx(X509_ASN_ENCODING, X509_PUBLIC_KEY_INFO, source, 0, null_mut(), null_mut(), &mut length) } == 0
        || length == 0 || length > 65536
    {
        return Err(Failure::InstalledSignature);
    }
    let mut encoded = vec![0u8; length as usize];
    if unsafe { CryptEncodeObjectEx(X509_ASN_ENCODING, X509_PUBLIC_KEY_INFO, source, 0, null_mut(), encoded.as_mut_ptr().cast(), &mut length) } == 0 {
        return Err(Failure::InstalledSignature);
    }
    let mut digest = [0u8; 32];
    if unsafe { BCryptHash(BCRYPT_SHA256_ALG_HANDLE, null_mut(), 0, encoded.as_mut_ptr(), length, digest.as_mut_ptr(), digest.len() as u32) } != 0 {
        return Err(Failure::Cryptography);
    }
    Ok(digest)
}
