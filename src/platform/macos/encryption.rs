// API key storage (macOS) ... not yet implemented.
//
// The real version stores the key in the login Keychain (#61), which needs a
// stable code signature or every rebuild re-prompts for access. Until then
// `encrypt_api_key` fails, so `Config::save` keeps its existing "saving in
// plain text" path and the file is written mode 0600.
//
// `enc:` values come from a Windows DPAPI envelope and cannot be opened here.

use thiserror::Error;

const ENC_PREFIX: &str = "enc:";

#[derive(Debug, Error)]
pub enum EncryptionError {
    #[error("API key encryption is not implemented on macOS yet (#61)")]
    Unsupported,
}

/// Returns true if the value is an encrypted string (`enc:...`).
pub fn is_encrypted(value: &str) -> bool {
    value.starts_with(ENC_PREFIX)
}

pub fn encrypt_api_key(_plaintext: &str) -> Result<String, EncryptionError> {
    Err(EncryptionError::Unsupported)
}

pub fn decrypt_api_key(_encrypted: &str) -> Result<String, EncryptionError> {
    Err(EncryptionError::Unsupported)
}
