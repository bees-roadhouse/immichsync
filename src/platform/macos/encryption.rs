// API key storage (macOS): the login Keychain.
//
// The Windows build seals the key with DPAPI and keeps the blob in
// `config.toml` as `enc:<base64>`. On macOS the key itself goes into the
// login Keychain as a generic password, and `config.toml` holds only the
// marker `keychain:<account>`. `Config::save` already routes every plaintext
// key through `encrypt_api_key`, so a key from the tracer era (plaintext in
// the file) moves into the Keychain on the next save.
//
// Keychain access control is tied to the code signature that created the
// item. A Developer-ID-signed bundle keeps one identity across updates and is
// never prompted. An ad-hoc dev build gets a new identity on every rebuild,
// and macOS asks once per rebuild ("immichsync wants to use your
// confidential information"). Dev runs can avoid that with
// IMMICHSYNC_API_KEY.

use security_framework::passwords::{
    delete_generic_password, get_generic_password, set_generic_password,
};
use thiserror::Error;

/// Keychain service name for every ImmichSync item.
pub const KEYCHAIN_SERVICE: &str = "com.beesroadhouse.immichsync";
/// Account under which the server API key is stored.
const API_KEY_ACCOUNT: &str = "api-key";

const ENC_PREFIX: &str = "enc:";
const KEYCHAIN_PREFIX: &str = "keychain:";

#[derive(Debug, Error)]
pub enum EncryptionError {
    #[error("Keychain error: {0}")]
    Keychain(#[from] security_framework::base::Error),

    #[error("Keychain item is not valid UTF-8")]
    NotUtf8,

    #[error("`enc:` values are Windows DPAPI blobs and cannot be opened on macOS")]
    WindowsBlob,
}

/// True for any stored form that isn't the plaintext key: a Keychain
/// marker, or a Windows DPAPI blob copied over from a PC.
pub fn is_encrypted(value: &str) -> bool {
    value.starts_with(KEYCHAIN_PREFIX) || value.starts_with(ENC_PREFIX)
}

/// Store the key in the Keychain and return the marker for `config.toml`.
pub fn encrypt_api_key(plaintext: &str) -> Result<String, EncryptionError> {
    store(KEYCHAIN_SERVICE, API_KEY_ACCOUNT, plaintext)?;
    Ok(format!("{KEYCHAIN_PREFIX}{API_KEY_ACCOUNT}"))
}

/// Resolve a stored value from `config.toml` back to the key.
pub fn decrypt_api_key(stored: &str) -> Result<String, EncryptionError> {
    if let Some(account) = stored.strip_prefix(KEYCHAIN_PREFIX) {
        return load(KEYCHAIN_SERVICE, account);
    }
    Err(EncryptionError::WindowsBlob)
}

/// Remove the stored key (not called by the app today; for a future
/// "forget server" action and for tests).
#[allow(dead_code)]
pub fn delete_api_key() -> Result<(), EncryptionError> {
    remove(KEYCHAIN_SERVICE, API_KEY_ACCOUNT)
}

fn store(service: &str, account: &str, secret: &str) -> Result<(), EncryptionError> {
    // set_generic_password updates the item in place when it already exists.
    set_generic_password(service, account, secret.as_bytes())?;
    Ok(())
}

fn load(service: &str, account: &str) -> Result<String, EncryptionError> {
    let bytes = get_generic_password(service, account)?;
    String::from_utf8(bytes).map_err(|_| EncryptionError::NotUtf8)
}

fn remove(service: &str, account: &str) -> Result<(), EncryptionError> {
    match delete_generic_password(service, account) {
        Ok(()) => Ok(()),
        // errSecItemNotFound: nothing to delete.
        Err(e) if e.code() == -25300 => Ok(()),
        Err(e) => Err(e.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markers_count_as_encrypted() {
        assert!(is_encrypted("keychain:api-key"));
        assert!(is_encrypted("enc:AAAA"));
        assert!(!is_encrypted("plain-key-123"));
        assert!(!is_encrypted(""));
    }

    #[test]
    fn windows_blob_is_refused_not_misread() {
        assert!(matches!(
            decrypt_api_key("enc:AAAA"),
            Err(EncryptionError::WindowsBlob)
        ));
    }

    /// Touches the real login Keychain, so it is opt-in:
    /// `cargo test -- --ignored keychain_round_trip`. Uses its own service
    /// name and deletes the item afterwards.
    #[test]
    #[ignore]
    fn keychain_round_trip() {
        let service = format!("{KEYCHAIN_SERVICE}.test-{}", std::process::id());
        store(&service, "api-key", "first").unwrap();
        assert_eq!(load(&service, "api-key").unwrap(), "first");
        store(&service, "api-key", "second").unwrap();
        assert_eq!(load(&service, "api-key").unwrap(), "second");
        remove(&service, "api-key").unwrap();
        assert!(load(&service, "api-key").is_err());
        remove(&service, "api-key").unwrap();
    }
}
