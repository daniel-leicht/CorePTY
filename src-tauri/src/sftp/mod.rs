//! SFTP: a byte-exact protocol client ([`client`]), pipelined remote files
//! ([`file`]), and the IPC commands that open SFTP filesystems on SSH
//! connections ([`commands`]). The [`crate::vfs::sftp::SftpFs`] provider
//! builds on these.

pub mod client;
pub mod commands;
pub mod file;
pub mod protocol;

#[cfg(test)]
pub(crate) mod testing;

#[cfg(test)]
mod bench;
