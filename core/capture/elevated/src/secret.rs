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

pub(crate) struct Secret<const N: usize>(pub(crate) [u8; N]);

impl<const N: usize> Secret<N> {
    pub(crate) fn random() -> Result<Self> {
        let mut bytes = Self([0; N]);
        if unsafe {
            BCryptGenRandom(std::ptr::null_mut(), bytes.0.as_mut_ptr(), N as u32, BCRYPT_USE_SYSTEM_PREFERRED_RNG)
        } != 0 || bytes.0.iter().all(|&byte| byte == 0)
        {
            return Err(Failure::Cryptography);
        }
        Ok(bytes)
    }
}

impl<const N: usize> Drop for Secret<N> {
    fn drop(&mut self) { erase(&mut self.0); }
}

pub(crate) fn hmac(key: &Secret<32>, input: &[u8]) -> Result<Secret<32>> {
    let mut digest = Secret([0u8; 32]);
    if unsafe {
        BCryptHash(BCRYPT_HMAC_SHA256_ALG_HANDLE, key.0.as_ptr().cast_mut(), key.0.len() as u32,
            input.as_ptr().cast_mut(), input.len() as u32, digest.0.as_mut_ptr(), digest.0.len() as u32)
    } != 0
    {
        return Err(Failure::Cryptography);
    }
    Ok(digest)
}

pub(crate) fn matches(expected: &[u8; 32], actual: &[u8; 32]) -> bool {
    let mut difference = 0u8;
    for (left, right) in expected.iter().zip(actual) {
        // Volatile loads preserve complete fixed-count traversal under optimization.
        difference |= unsafe { std::ptr::read_volatile(left) ^ std::ptr::read_volatile(right) };
    }
    difference == 0
}
