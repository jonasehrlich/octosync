//! Actor backends implementing the platform user manager contract.
//!
//! Each backend handles the full message set of [`super::messages`], so any of them
//! can back a [`super::UserManager`].

#[cfg(target_os = "linux")]
pub mod linux;
pub mod mock;
#[cfg(test)]
pub mod testing;
