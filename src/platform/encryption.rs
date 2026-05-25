// API key encryption: DPAPI + AES-256-GCM envelope encryption.
//
// The API key is encrypted with a random AES-256-GCM key. That AES key is
// then protected via Windows DPAPI (CryptProtectData), which ties it to the
// current user account. The combined encrypted payload is stored as
// `enc:<base64>` in config.toml.
//
// Format of the base64 payload:
//   [4 bytes: DPAPI blob length (u32 LE)]
//   [DPAPI blob: encrypted AES key + nonce]
//   [AES-GCM ciphertext + tag of the plaintext API key]
//
// Backward compatibility: plain API keys (not prefixed with `enc:`) are
// loaded as-is and automatically encrypted on the next save.

use aes_gcm::aead::{Aead, KeyInit, OsRng};
use aes_gcm::{Aes256Gcm, Nonce};
use thiserror::Error;
use tracing::debug;
use windows::Win32::Security::Cryptography::{
    CryptProtectData, CryptUnprotectData, CRYPT_INTEGER_BLOB,
};

// LocalFree is needed to free DPAPI-allocated buffers but isn't always
// exposed by the windows crate. Import it directly.
extern "system" {
    fn LocalFree(hmem: *mut std::ffi::c_void) -> *mut std::ffi::c_void;
}

const ENC_PREFIX: &str = "enc:";
const NONCE_LEN: usize = 12;
const KEY_LEN: usize = 32;

#[derive(Debug, Error)]
pub enum EncryptionError {
    #[error("DPAPI CryptProtectData failed")]
    DpapiProtect,

    #[error("DPAPI CryptUnprotectData failed")]
    DpapiUnprotect,

    #[error("AES-GCM encryption failed")]
    AesEncrypt,

    #[error("AES-GCM decryption failed")]
    AesDecrypt,

    #[error("base64 decode failed: {0}")]
    Base64(#[from] base64::DecodeError),

    #[error("encrypted payload is malformed")]
    Malformed,
}

/// Returns true if the value is an encrypted string (`enc:...`).
pub fn is_encrypted(value: &str) -> bool {
    value.starts_with(ENC_PREFIX)
}

/// Encrypt a plaintext API key. Returns an `enc:<base64>` string.
pub fn encrypt_api_key(plaintext: &str) -> Result<String, EncryptionError> {
    use aes_gcm::aead::rand_core::RngCore;
    use base64::Engine;

    // Generate random AES-256 key and nonce.
    let aes_key = Aes256Gcm::generate_key(OsRng);
    let mut nonce_bytes = [0u8; NONCE_LEN];
    OsRng.fill_bytes(&mut nonce_bytes);
    let nonce = Nonce::from_slice(&nonce_bytes);

    // Encrypt the plaintext with AES-GCM.
    let cipher = Aes256Gcm::new(&aes_key);
    let ciphertext = cipher
        .encrypt(nonce, plaintext.as_bytes())
        .map_err(|_| EncryptionError::AesEncrypt)?;

    // Pack AES key + nonce together, then protect with DPAPI.
    let mut key_material = Vec::with_capacity(KEY_LEN + NONCE_LEN);
    key_material.extend_from_slice(&aes_key);
    key_material.extend_from_slice(&nonce_bytes);

    let dpapi_blob = dpapi_protect(&key_material)?;

    // Build the final payload: [dpapi_blob_len:u32le][dpapi_blob][ciphertext]
    let dpapi_len = dpapi_blob.len() as u32;
    let mut payload = Vec::new();
    payload.extend_from_slice(&dpapi_len.to_le_bytes());
    payload.extend_from_slice(&dpapi_blob);
    payload.extend_from_slice(&ciphertext);

    let encoded = base64::engine::general_purpose::STANDARD.encode(&payload);

    debug!("API key encrypted ({} bytes)", payload.len());
    Ok(format!("{ENC_PREFIX}{encoded}"))
}

/// Decrypt an `enc:<base64>` API key back to plaintext.
pub fn decrypt_api_key(encrypted: &str) -> Result<String, EncryptionError> {
    use base64::Engine;

    let b64 = encrypted
        .strip_prefix(ENC_PREFIX)
        .ok_or(EncryptionError::Malformed)?;

    let payload = base64::engine::general_purpose::STANDARD.decode(b64)?;

    if payload.len() < 4 {
        return Err(EncryptionError::Malformed);
    }

    // Read DPAPI blob length.
    let dpapi_len = u32::from_le_bytes(
        payload[0..4]
            .try_into()
            .map_err(|_| EncryptionError::Malformed)?,
    ) as usize;

    if payload.len() < 4 + dpapi_len {
        return Err(EncryptionError::Malformed);
    }

    let dpapi_blob = &payload[4..4 + dpapi_len];
    let ciphertext = &payload[4 + dpapi_len..];

    // Unprotect the DPAPI blob to recover AES key + nonce.
    let key_material = dpapi_unprotect(dpapi_blob)?;

    if key_material.len() != KEY_LEN + NONCE_LEN {
        return Err(EncryptionError::Malformed);
    }

    let aes_key = aes_gcm::Key::<Aes256Gcm>::from_slice(&key_material[..KEY_LEN]);
    let nonce = Nonce::from_slice(&key_material[KEY_LEN..]);

    // Decrypt the API key.
    let cipher = Aes256Gcm::new(aes_key);
    let plaintext = cipher
        .decrypt(nonce, ciphertext)
        .map_err(|_| EncryptionError::AesDecrypt)?;

    String::from_utf8(plaintext).map_err(|_| EncryptionError::AesDecrypt)
}

// ── DPAPI wrappers ──────────────────────────────────────────────────────────

fn dpapi_protect(data: &[u8]) -> Result<Vec<u8>, EncryptionError> {
    let input_blob = CRYPT_INTEGER_BLOB {
        cbData: data.len() as u32,
        pbData: data.as_ptr() as *mut u8,
    };

    let mut output_blob = CRYPT_INTEGER_BLOB::default();

    unsafe {
        CryptProtectData(
            &input_blob,
            None, // description
            None, // optional entropy
            None, // reserved
            None, // prompt struct
            0,    // flags
            &mut output_blob,
        )
        .map_err(|_| EncryptionError::DpapiProtect)?;
    }

    let result = unsafe {
        std::slice::from_raw_parts(output_blob.pbData, output_blob.cbData as usize).to_vec()
    };

    // Free the DPAPI-allocated buffer.
    unsafe {
        LocalFree(output_blob.pbData as *mut std::ffi::c_void);
    }

    Ok(result)
}

fn dpapi_unprotect(data: &[u8]) -> Result<Vec<u8>, EncryptionError> {
    let input_blob = CRYPT_INTEGER_BLOB {
        cbData: data.len() as u32,
        pbData: data.as_ptr() as *mut u8,
    };

    let mut output_blob = CRYPT_INTEGER_BLOB::default();

    unsafe {
        CryptUnprotectData(
            &input_blob,
            None, // description out
            None, // optional entropy
            None, // reserved
            None, // prompt struct
            0,    // flags
            &mut output_blob,
        )
        .map_err(|_| EncryptionError::DpapiUnprotect)?;
    }

    let result = unsafe {
        std::slice::from_raw_parts(output_blob.pbData, output_blob.cbData as usize).to_vec()
    };

    // Free the DPAPI-allocated buffer.
    unsafe {
        LocalFree(output_blob.pbData as *mut std::ffi::c_void);
    }

    Ok(result)
}
