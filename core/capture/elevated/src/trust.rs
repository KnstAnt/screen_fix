mod permissions;
mod signature;

use std::{ffi::OsString, mem::size_of, os::windows::ffi::OsStringExt, path::{Component, Path, PathBuf, Prefix}, ptr::{null, null_mut}};

use windows_sys::Win32::{
    Foundation::GENERIC_READ,
    Globalization::{CSTR_EQUAL, CompareStringOrdinal},
    Storage::FileSystem::{
        CreateFileW, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT, FILE_ATTRIBUTE_TAG_INFO,
        FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_ID_INFO, FILE_LIST_DIRECTORY,
        FILE_READ_ATTRIBUTES,
        FILE_SHARE_READ, FILE_TYPE_DISK, FileAttributeTagInfo, FileIdInfo, GetDriveTypeW,
        GetFileInformationByHandleEx, GetFileType, GetFinalPathNameByHandleW,
        GetVolumeInformationByHandleW, OPEN_EXISTING, READ_CONTROL, VOLUME_NAME_GUID, VOLUME_NAME_NT,
    },
    System::{
        Com::CoTaskMemFree, LibraryLoader::GetModuleHandleW,
        ProcessStatus::K32GetMappedFileNameW, SystemInformation::GetSystemWindowsDirectoryW,
        Threading::GetCurrentProcess,
    },
    UI::Shell::{FOLDERID_ProgramFilesX64, SHGetKnownFolderPath},
};

use crate::{process::Process, win::{Failure, Handle, Result, wide}};

pub(crate) use signature::{ImageRole, Release};

const PRODUCT: &str = "screen_fix";
const RECORDER: &str = "screen-fix.exe";
const ORDINARY: &str = "screen-fix-input-helper.exe";
const COLLECTOR: &str = "screen-fix-elevated-capture.exe";
const DRIVE_FIXED: u32 = 3;

pub(crate) struct Installation {
    directory: PathBuf,
    recorder: Item,
    collector: Item,
    _ordinary: Item,
    _ancestors: Vec<Item>,
    pub(crate) release: Release,
}

impl Installation {
    pub(crate) fn verify(own_role: ImageRole) -> Result<Self> {
        let release = Release::current(own_role)?;
        let program_files = program_files()?;
        let root = drive_root(&program_files)?;
        let root_item = Item::open(&root, true)?;
        let fs = root_item.local_volume()?;
        root_item.verify_system_volume()?;
        permissions::verify(&root_item.handle, true)?;

        let system_windows = windows_directory()?;
        let system_item = Item::open(&system_windows, true)?;
        if system_item.id.VolumeSerialNumber != root_item.id.VolumeSerialNumber
            || !same_path(&system_item.final_volume_path()?, &fs)?
        {
            return Err(Failure::InstalledPath);
        }

        let mut ancestors = vec![root_item];
        let mut path = root;
        let parts = program_files.strip_prefix(&path).map_err(|_| Failure::InstalledPath)?;
        for part in parts.components() {
            let Component::Normal(part) = part else { return Err(Failure::InstalledPath) };
            path.push(part);
            let item = Item::open(&path, true)?;
            item.check_volume(&fs, ancestors[0].id.VolumeSerialNumber)?;
            permissions::verify(&item.handle, true)?;
            ancestors.push(item);
        }
        if ancestors.len() == 1 {
            return Err(Failure::InstalledPath);
        }
        let directory = path.join(PRODUCT);
        let product = Item::open(&directory, true)?;
        product.check_volume(&fs, ancestors[0].id.VolumeSerialNumber)?;
        permissions::verify(&product.handle, false)?;
        ancestors.push(product);
        let recorder = Item::open(&directory.join(RECORDER), false)?;
        let collector = Item::open(&directory.join(COLLECTOR), false)?;
        let ordinary = Item::open(&directory.join(ORDINARY), false)?;
        for (image, role) in [
            (&recorder, ImageRole::Recorder),
            (&collector, ImageRole::Collector),
            (&ordinary, ImageRole::Ordinary),
        ] {
            image.check_volume(&fs, ancestors[0].id.VolumeSerialNumber)?;
            permissions::verify(&image.handle, false)?;
            release.verify(&image.handle, &image.path, role)?;
        }
        let recorder_id = recorder.id.FileId.Identifier;
        let collector_id = collector.id.FileId.Identifier;
        let ordinary_id = ordinary.id.FileId.Identifier;
        if recorder_id == collector_id || recorder_id == ordinary_id || collector_id == ordinary_id {
            return Err(Failure::InstalledPath);
        }
        Ok(Self { directory, recorder, collector, _ordinary: ordinary, _ancestors: ancestors, release })
    }

    pub(crate) fn directory(&self) -> &Path { &self.directory }

    pub(crate) fn collector(&self) -> &Path { &self.collector.path }

    pub(crate) fn verify_recorder(&self, process: &Process) -> Result<()> {
        self.recorder.verify_process(process)
    }

    pub(crate) fn verify_collector(&self, process: &Process) -> Result<()> {
        self.collector.verify_process(process)
    }
}

fn program_files() -> Result<PathBuf> {
    let mut path = null_mut();
    // No create/verify side effects or caller-provided token.
    if unsafe { SHGetKnownFolderPath(&FOLDERID_ProgramFilesX64, 0, null_mut(), &mut path) } < 0
        || path.is_null()
    {
        if !path.is_null() {
            unsafe { CoTaskMemFree(path.cast()) };
        }
        return Err(Failure::InstalledPath);
    }
    let length = (0..32768).find(|&i| unsafe { *path.add(i) == 0 });
    let result = length.map(|len| PathBuf::from(OsString::from_wide(unsafe {
        std::slice::from_raw_parts(path, len)
    })));
    unsafe { CoTaskMemFree(path.cast()) };
    result.ok_or(Failure::InstalledPath)
}

fn windows_directory() -> Result<PathBuf> {
    let mut path = vec![0u16; 32768];
    let length = unsafe { GetSystemWindowsDirectoryW(path.as_mut_ptr(), path.len() as u32) } as usize;
    if length == 0 || length >= path.len() {
        return Err(Failure::InstalledPath);
    }
    Ok(PathBuf::from(OsString::from_wide(&path[..length])))
}

fn drive_root(path: &Path) -> Result<PathBuf> {
    let mut components = path.components();
    match (components.next(), components.next()) {
        (Some(Component::Prefix(prefix)), Some(Component::RootDir)) => match prefix.kind() {
            Prefix::Disk(drive) => Ok(PathBuf::from(format!("{}:\\", char::from(drive)))),
            _ => Err(Failure::InstalledPath),
        },
        _ => Err(Failure::InstalledPath),
    }
}

fn same_path(a: &[u16], b: &[u16]) -> Result<bool> {
    let result = unsafe {
        CompareStringOrdinal(a.as_ptr(), a.len() as i32, b.as_ptr(), b.len() as i32, 1)
    };
    if result == 0 { return Err(Failure::InstalledPath) }
    Ok(result == CSTR_EQUAL as i32)
}

struct Item {
    handle: Handle,
    path: PathBuf,
    id: FILE_ID_INFO,
}

impl Item {
    fn open(path: &Path, directory: bool) -> Result<Self> {
        let item = Self::open_item(path, directory)?;
        let name = wide(path);
        let final_dos = item.final_path(0)?;
        let dos_prefix = wide(r"\\?\");
        if !final_dos.starts_with(&dos_prefix[..dos_prefix.len() - 1])
            || !same_path(&final_dos[dos_prefix.len() - 1..], &name[..name.len() - 1])?
        {
            return Err(Failure::InstalledPath);
        }
        Ok(item)
    }

    fn open_item(path: &Path, directory: bool) -> Result<Self> {
        let flags = FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS;
        // List/read access participates in file sharing. An attributes-only
        // handle would not pin a directory against rename or writes.
        let access = if directory { FILE_LIST_DIRECTORY | FILE_READ_ATTRIBUTES | READ_CONTROL } else { GENERIC_READ | READ_CONTROL };
        let name = wide(path);
        let raw = unsafe { CreateFileW(name.as_ptr(), access, FILE_SHARE_READ, null(), OPEN_EXISTING, flags, null_mut()) };
        let handle = unsafe { Handle::take(raw, Failure::InstalledPath)? };
        let mut attributes = FILE_ATTRIBUTE_TAG_INFO::default();
        let mut id = FILE_ID_INFO::default();
        if unsafe { GetFileType(handle.raw()) } != FILE_TYPE_DISK
            || unsafe {
                GetFileInformationByHandleEx(handle.raw(), FileAttributeTagInfo,
                    (&mut attributes as *mut FILE_ATTRIBUTE_TAG_INFO).cast(), size_of::<FILE_ATTRIBUTE_TAG_INFO>() as u32)
            } == 0
            || unsafe {
                GetFileInformationByHandleEx(handle.raw(), FileIdInfo,
                    (&mut id as *mut FILE_ID_INFO).cast(), size_of::<FILE_ID_INFO>() as u32)
            } == 0
            || attributes.FileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0
            || (attributes.FileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0) != directory
        {
            return Err(Failure::InstalledPath);
        }
        Ok(Self { handle, path: path.to_owned(), id })
    }

    fn verify_system_volume(&self) -> Result<()> {
        let root = self.path.to_str().ok_or(Failure::InstalledPath)?;
        // GLOBALROOT resolves from the object manager root, bypassing a logon-local
        // DOS device map. The shell receives an ordinary path on this global
        // system letter, which an interactive token cannot redefine locally.
        let global = PathBuf::from(format!(r"\\?\GLOBALROOT\Global??\{root}"));
        let global = Self::open_item(&global, true)?;
        let nt_root = self.final_path(VOLUME_NAME_NT)?;
        if global.id.VolumeSerialNumber != self.id.VolumeSerialNumber
            || global.id.FileId.Identifier != self.id.FileId.Identifier
            || !same_path(&global.final_path(VOLUME_NAME_NT)?, &nt_root)?
        {
            return Err(Failure::InstalledPath);
        }

        // Read the native path of the system DLL already mapped by the loader.
        // A configured Windows or Program Files DOS pathname alone is insufficient.
        let dll = wide("ntdll.dll");
        let module = unsafe { GetModuleHandleW(dll.as_ptr()) };
        if module.is_null() { return Err(Failure::InstalledPath) }
        let mut mapped = vec![0u16; 32768];
        let length = unsafe {
            K32GetMappedFileNameW(GetCurrentProcess(), module.cast(), mapped.as_mut_ptr(), mapped.len() as u32)
        } as usize;
        if length == 0 || length >= mapped.len() { return Err(Failure::InstalledPath) }
        let mut prefix = nt_root;
        if prefix.last() != Some(&(b'\\' as u16)) {
            prefix.push(b'\\' as u16);
        }
        if length <= prefix.len() || !same_path(&mapped[..prefix.len()], &prefix)? {
            return Err(Failure::InstalledPath);
        }
        Ok(())
    }

    fn final_path(&self, flags: u32) -> Result<Vec<u16>> {
        let mut path = vec![0u16; 32768];
        let length = unsafe { GetFinalPathNameByHandleW(self.handle.raw(), path.as_mut_ptr(), path.len() as u32, flags) } as usize;
        if length == 0 || length >= path.len() {
            return Err(Failure::InstalledPath);
        }
        path.truncate(length);
        Ok(path)
    }

    fn final_volume_path(&self) -> Result<Vec<u16>> {
        let path = self.final_path(VOLUME_NAME_GUID)?;
        let prefix = wide(r"\\?\Volume{");
        let prefix = &prefix[..prefix.len() - 1];
        let guid_end = prefix.len() + 36;
        if !path.starts_with(prefix)
            || path.get(guid_end..guid_end + 2) != Some(&[b'}' as u16, b'\\' as u16])
        {
            return Err(Failure::InstalledPath);
        }
        Ok(path[..guid_end + 2].to_vec())
    }

    fn local_volume(&self) -> Result<Vec<u16>> {
        const FILE_PERSISTENT_ACLS: u32 = 8;
        let mut fs = [0u16; 16];
        let mut flags = 0;
        if unsafe { GetDriveTypeW(wide(&self.path).as_ptr()) } != DRIVE_FIXED
            || unsafe {
                GetVolumeInformationByHandleW(self.handle.raw(), null_mut(), 0, null_mut(),
                    null_mut(), &mut flags, fs.as_mut_ptr(), fs.len() as u32)
            } == 0
            || flags & FILE_PERSISTENT_ACLS == 0
            || !fs.starts_with(&wide("NTFS"))
        {
            return Err(Failure::InstalledPath);
        }
        self.final_volume_path()
    }

    fn check_volume(&self, volume: &[u16], serial: u64) -> Result<()> {
        if self.id.VolumeSerialNumber != serial || !same_path(&self.final_volume_path()?, volume)? {
            return Err(Failure::InstalledPath);
        }
        Ok(())
    }

    fn verify_process(&self, process: &Process) -> Result<()> {
        process.ensure_live()?;
        // The kernel reports the original process's native image name. Match it
        // before any file is opened, then resolve that fixed path to the held ID.
        let native = self.final_path(VOLUME_NAME_NT)?;
        if process.native_image() != native.as_slice() {
            return Err(Failure::ProcessIdentity);
        }
        let mut reported = wide(r"\\?\GLOBALROOT");
        reported.pop();
        reported.extend_from_slice(process.native_image());
        let reported = PathBuf::from(OsString::from_wide(&reported));
        let actual = Item::open_item(&reported, false).map_err(|_| Failure::ProcessIdentity)?;
        if actual.final_path(VOLUME_NAME_NT)? != native
            || actual.id.VolumeSerialNumber != self.id.VolumeSerialNumber
            || actual.id.FileId.Identifier != self.id.FileId.Identifier
        {
            return Err(Failure::ProcessIdentity);
        }
        process.ensure_live()
    }
}
