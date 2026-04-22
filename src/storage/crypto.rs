//! AES-256-GCM block 加密 + DPAPI 主密钥包裹。
//!
//! - 主密钥：32 字节随机；DPAPI 包裹后保存到 `key.bin`。
//! - 每条加密 block：`[u32 plaintext_len][12 nonce][ciphertext..plaintext_len + 16 tag]`
//! - 跨 block / 跨文件的关联数据（AAD）：`[file_magic | date | block_index]`，用于绑定上下文。

use std::path::Path;

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Nonce};
use rand::RngCore;

pub const KEY_LEN: usize = 32;
pub const NONCE_LEN: usize = 12;
pub const TAG_LEN: usize = 16;

#[derive(Clone)]
pub struct MasterKey(pub [u8; KEY_LEN]);

impl MasterKey {
    pub fn new_random() -> Self {
        let mut k = [0u8; KEY_LEN];
        rand::thread_rng().fill_bytes(&mut k);
        Self(k)
    }
}

#[derive(Clone)]
pub struct Cipher {
    inner: Aes256Gcm,
}

impl Cipher {
    pub fn new(key: &MasterKey) -> Self {
        let inner = Aes256Gcm::new_from_slice(&key.0).expect("32-byte key");
        Self { inner }
    }

    /// 加密一个 block。返回完整 wire 格式 `[len|nonce|ct+tag]`。
    pub fn seal_block(&self, plaintext: &[u8], aad: &[u8]) -> Vec<u8> {
        let mut nonce_bytes = [0u8; NONCE_LEN];
        rand::thread_rng().fill_bytes(&mut nonce_bytes);
        let nonce = Nonce::from_slice(&nonce_bytes);
        let ct = self
            .inner
            .encrypt(nonce, Payload { msg: plaintext, aad })
            .expect("aes-gcm encrypt never fails for valid key");
        let mut out = Vec::with_capacity(4 + NONCE_LEN + ct.len());
        out.extend_from_slice(&(plaintext.len() as u32).to_le_bytes());
        out.extend_from_slice(&nonce_bytes);
        out.extend_from_slice(&ct);
        out
    }

    /// 解密一个 block。返回明文与本 block 在 wire 中占用的总字节数。
    pub fn open_block(&self, wire: &[u8], aad: &[u8]) -> Result<(Vec<u8>, usize), CryptoError> {
        if wire.len() < 4 + NONCE_LEN + TAG_LEN {
            return Err(CryptoError::Truncated);
        }
        let plaintext_len = u32::from_le_bytes(wire[0..4].try_into().unwrap()) as usize;
        let total = 4 + NONCE_LEN + plaintext_len + TAG_LEN;
        if wire.len() < total {
            return Err(CryptoError::Truncated);
        }
        let nonce = Nonce::from_slice(&wire[4..4 + NONCE_LEN]);
        let ct = &wire[4 + NONCE_LEN..total];
        let pt = self
            .inner
            .decrypt(nonce, Payload { msg: ct, aad })
            .map_err(|_| CryptoError::Auth)?;
        Ok((pt, total))
    }
}

#[derive(Debug)]
pub enum CryptoError {
    Truncated,
    Auth,
    Io(std::io::Error),
}

impl std::fmt::Display for CryptoError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CryptoError::Truncated => write!(f, "encrypted block truncated"),
            CryptoError::Auth => write!(f, "authentication failed (bad key or corrupted)"),
            CryptoError::Io(e) => write!(f, "io: {e}"),
        }
    }
}
impl std::error::Error for CryptoError {}
impl From<std::io::Error> for CryptoError {
    fn from(e: std::io::Error) -> Self {
        CryptoError::Io(e)
    }
}

// ─── 主密钥包裹（Windows DPAPI / fallback：明文，仅 dev/测试） ─────────────

/// 加载或生成主密钥。
///
/// - 不存在 → 新生成 + 包裹 + 写入。
/// - 存在 → 解包返回。
pub fn load_or_create_master_key(
    key_file: &Path,
    machine_scope: bool,
) -> std::io::Result<MasterKey> {
    if key_file.exists() {
        let blob = std::fs::read(key_file)?;
        let raw = unwrap_blob(&blob).map_err(|e| {
            std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string())
        })?;
        if raw.len() != KEY_LEN {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "key.bin: bad size",
            ));
        }
        let mut k = [0u8; KEY_LEN];
        k.copy_from_slice(&raw);
        return Ok(MasterKey(k));
    }
    if let Some(parent) = key_file.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let key = MasterKey::new_random();
    let blob = wrap_blob(&key.0, machine_scope).map_err(|e| {
        std::io::Error::new(std::io::ErrorKind::Other, e.to_string())
    })?;
    std::fs::write(key_file, blob)?;
    Ok(key)
}

#[cfg(windows)]
fn wrap_blob(plain: &[u8], machine_scope: bool) -> Result<Vec<u8>, String> {
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Cryptography::{
        CryptProtectData, CRYPT_INTEGER_BLOB, CRYPTPROTECT_LOCAL_MACHINE, CRYPTPROTECT_UI_FORBIDDEN,
    };

    let mut in_blob = CRYPT_INTEGER_BLOB {
        cbData: plain.len() as u32,
        pbData: plain.as_ptr() as *mut u8,
    };
    let mut out_blob = CRYPT_INTEGER_BLOB {
        cbData: 0,
        pbData: std::ptr::null_mut(),
    };
    let mut flags = CRYPTPROTECT_UI_FORBIDDEN;
    if machine_scope {
        flags |= CRYPTPROTECT_LOCAL_MACHINE;
    }
    let ok = unsafe {
        CryptProtectData(
            &mut in_blob,
            std::ptr::null(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            flags,
            &mut out_blob,
        )
    };
    if ok == 0 {
        return Err(format!(
            "CryptProtectData failed: 0x{:08X}",
            unsafe { windows_sys::Win32::Foundation::GetLastError() }
        ));
    }
    let slice = unsafe {
        std::slice::from_raw_parts(out_blob.pbData, out_blob.cbData as usize).to_vec()
    };
    unsafe {
        LocalFree(out_blob.pbData as _);
    }
    Ok(slice)
}

#[cfg(windows)]
fn unwrap_blob(blob: &[u8]) -> Result<Vec<u8>, String> {
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Cryptography::{
        CryptUnprotectData, CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN,
    };
    let mut in_blob = CRYPT_INTEGER_BLOB {
        cbData: blob.len() as u32,
        pbData: blob.as_ptr() as *mut u8,
    };
    let mut out_blob = CRYPT_INTEGER_BLOB {
        cbData: 0,
        pbData: std::ptr::null_mut(),
    };
    let ok = unsafe {
        CryptUnprotectData(
            &mut in_blob,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut out_blob,
        )
    };
    if ok == 0 {
        return Err(format!(
            "CryptUnprotectData failed: 0x{:08X}",
            unsafe { windows_sys::Win32::Foundation::GetLastError() }
        ));
    }
    let slice = unsafe {
        std::slice::from_raw_parts(out_blob.pbData, out_blob.cbData as usize).to_vec()
    };
    unsafe {
        LocalFree(out_blob.pbData as _);
    }
    Ok(slice)
}

// 非 Windows fallback：仅用于在非 Windows 平台编译测试 crypto 模块。
#[cfg(not(windows))]
fn wrap_blob(plain: &[u8], _machine_scope: bool) -> Result<Vec<u8>, String> {
    let mut v = b"PLAIN".to_vec();
    v.extend_from_slice(plain);
    Ok(v)
}
#[cfg(not(windows))]
fn unwrap_blob(blob: &[u8]) -> Result<Vec<u8>, String> {
    if blob.starts_with(b"PLAIN") {
        Ok(blob[5..].to_vec())
    } else {
        Err("not a PLAIN blob".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn block_round_trip() {
        let key = MasterKey::new_random();
        let cipher = Cipher::new(&key);
        let aad = b"ctx";
        let pt = b"hello world record bytes";
        let wire = cipher.seal_block(pt, aad);
        let (back, total) = cipher.open_block(&wire, aad).unwrap();
        assert_eq!(back, pt);
        assert_eq!(total, wire.len());
    }

    #[test]
    fn bad_aad_fails() {
        let key = MasterKey::new_random();
        let cipher = Cipher::new(&key);
        let wire = cipher.seal_block(b"x", b"a");
        assert!(cipher.open_block(&wire, b"b").is_err());
    }
}
