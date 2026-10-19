// macOS implementations of the platform seam.
//
// Each submodule mirrors the public surface of its Windows sibling one
// directory up, so callers stay OS-agnostic. Pieces that need a signed
// `.app` bundle (login item, Keychain, notifications) are stubs that say
// which issue replaces them.

pub mod autostart;
pub mod encryption;
pub mod event_loop;
pub mod install;
pub mod known_folders;
pub mod notify;
pub mod shutdown;
pub mod single_instance;
