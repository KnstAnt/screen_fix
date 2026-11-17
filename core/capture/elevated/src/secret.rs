use std::sync::atomic::{Ordering, compiler_fence};

use windows_sys::Win32::Security::Cryptography::{
    BCryptGenRandom, BCryptHash, BCRYPT_HMAC_SHA256_ALG_HANDLE, BCRYPT_USE_SYSTEM_PREFERRED_RNG,
};

use crate::win::{Failure, Result};

pub(crate) fn erase<T: Copy + Default>(values: &mut [T]) {
    for item in values {
        unsafe { std::ptr::write_volatile(item, T::default()) };
    }
    compiler_fence(Ordering::SeqCst);
}

/// Secret bytes are written in their final location and passed by
/// reference; a move would leave an unerased copy behind (E4).
pub(crate) struct Secret<const N: usize>(pub(crate) [u8; N]);

impl<const N: usize> Secret<N> {
    pub(crate) const fn zeroed() -> Self { Self([0; N]) }

    pub(crate) fn randomize(&mut self) -> Result<()> { random(&mut self.0) }
}

/// Fill `bytes` in place from the system RNG; all zeros is a failure.
pub(crate) fn random(bytes: &mut [u8]) -> Result<()> {
    if unsafe {
        BCryptGenRandom(std::ptr::null_mut(), bytes.as_mut_ptr(), bytes.len() as u32, BCRYPT_USE_SYSTEM_PREFERRED_RNG)
    } != 0 || bytes.iter().all(|&byte| byte == 0)
    {
        return Err(Failure::Cryptography);
    }
    Ok(())
}

impl<const N: usize> Drop for Secret<N> {
    fn drop(&mut self) { erase(&mut self.0); }
}

pub(crate) fn hmac(key: &Secret<32>, input: &[u8], digest: &mut Secret<32>) -> Result<()> {
    if unsafe {
        BCryptHash(BCRYPT_HMAC_SHA256_ALG_HANDLE, key.0.as_ptr().cast_mut(), key.0.len() as u32,
            input.as_ptr().cast_mut(), input.len() as u32, digest.0.as_mut_ptr(), digest.0.len() as u32)
    } != 0
    {
        return Err(Failure::Cryptography);
    }
    Ok(())
}

pub(crate) fn matches(expected: &[u8; 32], actual: &[u8; 32]) -> bool {
    let mut difference = 0u8;
    for (left, right) in expected.iter().zip(actual) {
        // Volatile loads preserve complete fixed-count traversal under optimization.
        difference |= unsafe { std::ptr::read_volatile(left) ^ std::ptr::read_volatile(right) };
    }
    difference == 0
}
