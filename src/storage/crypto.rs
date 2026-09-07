//! AES-256-GCM block 加密 + DPAPI 主密钥包裹。
//!
//! - 主密钥：32 字节随机；DPAPI 包裹后保存到 `key.bin`。
//! - 每条加密 block：`[u32 plaintext_len][12 nonce][ciphertext..plaintext_len + 16 tag]`
//! - 跨 block / 跨文件的关联数据（AAD）：`[file_magic | date | block_index]`，用于绑定上下文。

use std::io::Write;
use std::path::Path;

use aes_gcm::aead::{Aead, AeadInPlace, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Nonce, Tag};
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
        let mut out = Vec::new();
        self.seal_block_into(plaintext, aad, &mut out);
        out
    }

    /// Reuse a caller-owned wire buffer, encrypting its payload in place.
    /// The nonce and on-disk v1 format remain identical to seal_block.
    pub(crate) fn seal_block_into(&self, plaintext: &[u8], aad: &[u8], out: &mut Vec<u8>) {
        self.seal_block_with(plaintext.len(), aad, out, |out| {
            out.extend_from_slice(plaintext);
        });
    }

    /// Encode directly into the reusable wire buffer, avoiding a plaintext copy.
    pub(crate) fn seal_block_with(
        &self,
        plaintext_len: usize,
        aad: &[u8],
        out: &mut Vec<u8>,
        encode: impl FnOnce(&mut Vec<u8>),
    ) {
        let length = u32::try_from(plaintext_len).expect("v1 block length fits u32");
        out.clear();
        out.reserve(4 + NONCE_LEN + plaintext_len + TAG_LEN);
        out.extend_from_slice(&length.to_le_bytes());
        out.resize(4 + NONCE_LEN, 0);
        encode(out);
        assert_eq!(out.len(), 4 + NONCE_LEN + plaintext_len);
        let mut nonce_bytes = [0u8; NONCE_LEN];
        rand::thread_rng().fill_bytes(&mut nonce_bytes);
        let nonce = Nonce::from_slice(&nonce_bytes);
        out[4..4 + NONCE_LEN].copy_from_slice(&nonce_bytes);
        let tag = self
            .inner
            .encrypt_in_place_detached(nonce, aad, &mut out[4 + NONCE_LEN..])
            .expect("aes-gcm encrypt never fails for valid key");
        out.extend_from_slice(&tag);
    }

    /// Authenticate before exposing any plaintext. On failure the caller must
    /// discard the buffer; its contents are deliberately not returned.
    pub(crate) fn open_block_in_place<'a>(
        &self,
        wire: &'a mut [u8],
        aad: &[u8],
    ) -> Result<&'a [u8], CryptoError> {
        if wire.len() < 4 + NONCE_LEN + TAG_LEN {
            return Err(CryptoError::Truncated);
        }
        let length = u32::from_le_bytes(wire[..4].try_into().unwrap()) as usize;
        let total = length
            .checked_add(4 + NONCE_LEN + TAG_LEN)
            .ok_or(CryptoError::Truncated)?;
        if wire.len() < total {
            return Err(CryptoError::Truncated);
        }
        let (header, encrypted) = wire[..total].split_at_mut(4 + NONCE_LEN);
        let (payload, tag) = encrypted.split_at_mut(length);
        self.inner
            .decrypt_in_place_detached(
                Nonce::from_slice(&header[4..]),
                aad,
                payload,
                Tag::from_slice(tag),
            )
            .map_err(|_| CryptoError::Auth)?;
        Ok(payload)
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
        let raw = unwrap_blob(&blob)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?;
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
        if existing_logs(&parent.join("data"))? {
            return Err(std::io::Error::new(std::io::ErrorKind::NotFound,
                "key.bin is missing while recorded logs exist; refusing to replace their encryption key"));
        }
        std::fs::create_dir_all(parent)?;
    }
    let key = MasterKey::new_random();
    let blob =
        wrap_blob(&key.0, machine_scope).map_err(|e| std::io::Error::other(e.to_string()))?;
    let mut file = std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(key_file)?;
    file.write_all(&blob)?;
    file.sync_all()?;
    Ok(key)
}

pub(crate) fn existing_logs(root: &Path) -> std::io::Result<bool> {
    let mut pending = vec![root.to_path_buf()];
    while let Some(directory) = pending.pop() {
        let entries = match checked_data_entries(&directory) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        for entry in entries {
            let entry = entry?;
            let kind = checked_data_entry_type(&entry)?;
            if kind.is_dir() {
                pending.push(entry.path());
            }
            if kind.is_file() && has_log_extension(&entry.path()) {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

pub(crate) fn has_log_extension(path: &Path) -> bool {
    path.extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("log"))
}

/// Data trees support ordinary directories and regular log files. Following
/// links would require cycle/alias handling; silently skipping them could let
/// startup reuse dictionary IDs or replace a key belonging to readable logs.
pub(crate) fn checked_data_entries(path: &Path) -> std::io::Result<std::fs::ReadDir> {
    reject_linked_data(path, &std::fs::symlink_metadata(path)?, true)?;
    std::fs::read_dir(path)
}

pub(crate) fn checked_data_entry_type(
    entry: &std::fs::DirEntry,
) -> std::io::Result<std::fs::FileType> {
    let metadata = entry.metadata()?;
    reject_linked_data(&entry.path(), &metadata, false)?;
    Ok(metadata.file_type())
}

fn reject_linked_data(
    path: &Path,
    metadata: &std::fs::Metadata,
    directory_context: bool,
) -> std::io::Result<()> {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        reject_windows_data_attributes(path, metadata.file_attributes(), directory_context)
    }
    #[cfg(not(windows))]
    {
        let linked = metadata.file_type().is_symlink();
        let directory = metadata.is_dir()
            || linked && std::fs::metadata(path).is_ok_and(|target| target.is_dir());
        reject_linked_layout(path, linked, directory, directory_context)
    }
}

#[cfg(windows)]
fn reject_windows_data_attributes(
    path: &Path,
    attributes: u32,
    directory_context: bool,
) -> std::io::Result<()> {
    // Include all reparse layouts, not only symbolic-link tags. A junction or
    // provider-managed placeholder must not disappear from preflight.
    reject_linked_layout(
        path,
        attributes & 0x400 != 0,
        attributes & 0x10 != 0,
        directory_context,
    )
}

fn reject_linked_layout(
    path: &Path,
    linked: bool,
    directory: bool,
    directory_context: bool,
) -> std::io::Result<()> {
    if linked && (directory_context || directory || has_log_extension(path)) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "{}: linked/reparse data directories or log files are unsupported; refusing startup without complete history verification",
                path.display()
            ),
        ));
    }
    Ok(())
}

#[cfg(windows)]
fn wrap_blob(plain: &[u8], machine_scope: bool) -> Result<Vec<u8>, String> {
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Cryptography::{
        CryptProtectData, CRYPTPROTECT_LOCAL_MACHINE, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB,
    };

    let in_blob = CRYPT_INTEGER_BLOB {
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
            &in_blob,
            std::ptr::null(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            flags,
            &mut out_blob,
        )
    };
    if ok == 0 {
        return Err(format!("CryptProtectData failed: 0x{:08X}", unsafe {
            windows_sys::Win32::Foundation::GetLastError()
        }));
    }
    let slice =
        unsafe { std::slice::from_raw_parts(out_blob.pbData, out_blob.cbData as usize).to_vec() };
    unsafe {
        LocalFree(out_blob.pbData as _);
    }
    Ok(slice)
}

#[cfg(windows)]
fn unwrap_blob(blob: &[u8]) -> Result<Vec<u8>, String> {
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Cryptography::{
        CryptUnprotectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB,
    };
    let in_blob = CRYPT_INTEGER_BLOB {
        cbData: blob.len() as u32,
        pbData: blob.as_ptr() as *mut u8,
    };
    let mut out_blob = CRYPT_INTEGER_BLOB {
        cbData: 0,
        pbData: std::ptr::null_mut(),
    };
    let ok = unsafe {
        CryptUnprotectData(
            &in_blob,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut out_blob,
        )
    };
    if ok == 0 {
        return Err(format!("CryptUnprotectData failed: 0x{:08X}", unsafe {
            windows_sys::Win32::Foundation::GetLastError()
        }));
    }
    let slice =
        unsafe { std::slice::from_raw_parts(out_blob.pbData, out_blob.cbData as usize).to_vec() };
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

    #[cfg(windows)]
    #[test]
    fn windows_reparse_policy_rejects_only_data_directories_and_logs() {
        for (name, attributes, directory_context, rejected) in [
            ("2026", 0x10, false, false),
            ("2026-09-05.LOG", 0x80, false, false),
            ("notes.txt", 0x400, false, false),
            ("2026-09-05.log", 0x400, false, true),
            ("2026-09-05.LOG", 0x400, false, true),
            ("2026", 0x410, false, true),
            ("data", 0x400, true, true),
            ("data", 0x10, true, false),
        ] {
            let result =
                reject_windows_data_attributes(Path::new(name), attributes, directory_context);
            assert_eq!(
                result.is_err(),
                rejected,
                "{name}: attributes={attributes:#x}"
            );
            if let Err(error) = result {
                assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn linked_history_cannot_bypass_key_or_dictionary_preflight() {
        use crate::storage::log::referenced_dictionary_ids;
        for layout in ["root", "directory", "log"] {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path().join("root");
            let data = root.join("data");
            let archive = dir.path().join("archive");
            std::fs::create_dir_all(&root).unwrap();
            std::fs::create_dir_all(&archive).unwrap();
            let original = archive.join("2026-09-05.LOG");
            std::fs::write(&original, b"preserved history").unwrap();
            if layout == "root" {
                std::os::unix::fs::symlink(&archive, &data).unwrap();
            } else {
                std::fs::create_dir_all(&data).unwrap();
                let (target, link) = if layout == "directory" {
                    (&archive, data.join("2026"))
                } else {
                    (&original, data.join("2026-09-05.LOG"))
                };
                std::os::unix::fs::symlink(target, link).unwrap();
            }
            assert_eq!(
                existing_logs(&data).unwrap_err().kind(),
                std::io::ErrorKind::InvalidData
            );
            assert_eq!(
                referenced_dictionary_ids(&data, &Cipher::new(&MasterKey::new_random()))
                    .unwrap_err()
                    .kind(),
                std::io::ErrorKind::InvalidData
            );
            let key = root.join("key.bin");
            assert_eq!(
                load_or_create_master_key(&key, false).err().unwrap().kind(),
                std::io::ErrorKind::InvalidData
            );
            assert!(!key.exists());
            assert_eq!(std::fs::read(&original).unwrap(), b"preserved history");
        }

        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("data");
        std::fs::create_dir_all(&data).unwrap();
        let notes = dir.path().join("notes.txt");
        std::fs::write(&notes, b"unrelated").unwrap();
        std::os::unix::fs::symlink(notes, data.join("notes.txt")).unwrap();
        assert!(!existing_logs(&data).unwrap());
        assert_eq!(
            referenced_dictionary_ids(&data, &Cipher::new(&MasterKey::new_random())).unwrap(),
            (0, 0)
        );
    }

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

    #[test]
    fn reused_wire_buffer_keeps_v1_lengths_and_authentication() {
        let cipher = Cipher::new(&MasterKey::new_random());
        let mut wire = Vec::new();
        for length in [4096, 3, 0, 17, 2048, 1] {
            let plaintext = vec![42; length];
            cipher.seal_block_into(&plaintext, b"same-v1-context", &mut wire);
            assert_eq!(wire.len(), 4 + NONCE_LEN + length + TAG_LEN);
            let (decoded, consumed) = cipher.open_block(&wire, b"same-v1-context").unwrap();
            assert_eq!(decoded, plaintext);
            assert_eq!(consumed, wire.len());
        }
    }

    #[test]
    fn inplace_reader_accepts_legacy_blocks_and_rejects_tampering() {
        let cipher = Cipher::new(&MasterKey::new_random());
        let aad = b"legacy-v1-context";
        for length in [0, 1, 17, 4096 * 17] {
            let plaintext = vec![42; length];
            // Construct the original allocating Aead format independently of
            // the new in-place encoder, including the nonce and tag layout.
            let nonce_bytes = [19; NONCE_LEN];
            let encrypted = cipher
                .inner
                .encrypt(
                    Nonce::from_slice(&nonce_bytes),
                    Payload {
                        msg: &plaintext,
                        aad,
                    },
                )
                .unwrap();
            let mut wire = (length as u32).to_le_bytes().to_vec();
            wire.extend_from_slice(&nonce_bytes);
            wire.extend_from_slice(&encrypted);
            assert_eq!(
                cipher.open_block_in_place(&mut wire.clone(), aad).unwrap(),
                plaintext
            );
            assert!(cipher
                .open_block_in_place(&mut wire.clone(), b"wrong-context")
                .is_err());
            for index in [0, 4, wire.len() - 1] {
                let mut damaged = wire.clone();
                damaged[index] ^= 1;
                assert!(cipher.open_block_in_place(&mut damaged, aad).is_err());
            }
            wire.pop();
            assert!(cipher.open_block_in_place(&mut wire, aad).is_err());
        }
    }

    #[test]
    fn existing_recording_never_gets_a_replacement_key() {
        for name in [
            "2026-09-05.part-000001.log",
            "2026-09-05.LOG",
            "2026-09-05.PART-000001.LOG",
        ] {
            let dir = tempfile::tempdir().unwrap();
            let data = dir.path().join("data").join("2026").join("09");
            std::fs::create_dir_all(&data).unwrap();
            let path = data.join(name);
            std::fs::write(&path, b"preserve encrypted data").unwrap();
            let key_file = dir.path().join("key.bin");
            assert!(load_or_create_master_key(&key_file, false).is_err());
            assert!(!key_file.exists());
            assert_eq!(std::fs::read(&path).unwrap(), b"preserve encrypted data");
        }
    }

    #[test]
    fn created_key_is_reused_and_invalid_key_is_preserved() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("key.bin");
        let key = load_or_create_master_key(&path, false).unwrap();
        assert_eq!(load_or_create_master_key(&path, false).unwrap().0, key.0);
        std::fs::write(&path, b"incomplete key").unwrap();
        assert!(load_or_create_master_key(&path, false).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"incomplete key");
    }
}
