//! An SFTP filesystem that outlives its connection. Panels and transfer jobs
//! hold the mount; when the connection drops, the mount says so, and a
//! reconnect swaps a new session in under the same id, so they carry on.

use super::{sftp::SftpFs, Capabilities, Entry, Meta, Root, VPath, VRead, VWrite, Vfs, WriteMode};
use async_trait::async_trait;
use std::{
    io,
    sync::{Arc, Mutex, RwLock, Weak},
    time::SystemTime,
};
use tokio::sync::watch;

pub struct SftpMount {
    current: RwLock<Arc<SftpFs>>,
    connected: watch::Sender<bool>,
    /// Why the last session ended.
    lost_reason: Mutex<Option<String>>,
    /// Held while reconnecting, so two reconnects don't race.
    reconnecting: tokio::sync::Mutex<()>,
}

impl SftpMount {
    pub fn new(fs: SftpFs) -> Arc<Self> {
        let fs = Arc::new(fs);
        let mount = Arc::new(Self {
            current: RwLock::new(fs.clone()),
            connected: watch::channel(true).0,
            lost_reason: Mutex::new(None),
            reconnecting: tokio::sync::Mutex::new(()),
        });
        mount.watch(&fs);
        mount
    }

    fn current(&self) -> Arc<SftpFs> {
        self.current
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Carry on over a new session (after a reconnect).
    pub fn replace(self: &Arc<Self>, fs: SftpFs) {
        let fs = Arc::new(fs);
        {
            // Under the lock the watcher takes too, so a session ending just
            // now can't mark the new one disconnected.
            let mut current = self.current.write().unwrap_or_else(|e| e.into_inner());
            *current = fs.clone();
            *self.lost_reason.lock().unwrap_or_else(|e| e.into_inner()) = None;
            self.connected.send_replace(true);
        }
        self.watch(&fs);
    }

    /// Hold this while reconnecting: one reconnect at a time.
    pub async fn reconnecting(&self) -> tokio::sync::MutexGuard<'_, ()> {
        self.reconnecting.lock().await
    }

    pub fn lost_reason(&self) -> Option<String> {
        self.lost_reason
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// When `fs`'s session ends while it's still the current one, the mount is
    /// disconnected. Holds neither: a replaced session can go away.
    fn watch(self: &Arc<Self>, fs: &Arc<SftpFs>) {
        let ended = fs.ended();
        let mount: Weak<Self> = Arc::downgrade(self);
        let watched = Arc::downgrade(fs);
        tokio::spawn(async move {
            let reason = ended.await;
            let (Some(mount), Some(watched)) = (mount.upgrade(), watched.upgrade()) else {
                return;
            };
            let current = mount.current.read().unwrap_or_else(|e| e.into_inner());
            if Arc::ptr_eq(&current, &watched) {
                *mount.lost_reason.lock().unwrap_or_else(|e| e.into_inner()) = Some(reason);
                mount.connected.send_replace(false);
            }
        });
    }

    #[cfg(test)]
    pub fn session(&self) -> Arc<SftpFs> {
        self.current()
    }
}

#[async_trait]
impl Vfs for SftpMount {
    fn capabilities(&self) -> Capabilities {
        self.current().capabilities()
    }

    async fn home(&self) -> io::Result<VPath> {
        self.current().home().await
    }

    async fn roots(&self) -> io::Result<Vec<Root>> {
        self.current().roots().await
    }

    async fn resolve(&self, input: &str, base: Option<&VPath>) -> io::Result<VPath> {
        self.current().resolve(input, base).await
    }

    async fn canonicalize(&self, path: &VPath) -> io::Result<VPath> {
        self.current().canonicalize(path).await
    }

    async fn list(&self, dir: &VPath) -> io::Result<Vec<Entry>> {
        self.current().list(dir).await
    }

    async fn stat(&self, path: &VPath) -> io::Result<Meta> {
        self.current().stat(path).await
    }

    async fn lstat(&self, path: &VPath) -> io::Result<Meta> {
        self.current().lstat(path).await
    }

    async fn mkdir(&self, path: &VPath) -> io::Result<()> {
        self.current().mkdir(path).await
    }

    async fn rename(&self, from: &VPath, to: &VPath, overwrite: bool) -> io::Result<()> {
        self.current().rename(from, to, overwrite).await
    }

    async fn remove(&self, path: &VPath, recursive: bool) -> io::Result<()> {
        self.current().remove(path, recursive).await
    }

    async fn open_read(&self, path: &VPath, offset: u64) -> io::Result<VRead> {
        self.current().open_read(path, offset).await
    }

    async fn open_read_until(&self, path: &VPath, offset: u64, end: u64) -> io::Result<VRead> {
        self.current().open_read_until(path, offset, end).await
    }

    async fn read_range(&self, path: &VPath, offset: u64, length: usize) -> io::Result<Vec<u8>> {
        self.current().read_range(path, offset, length).await
    }

    async fn open_write(&self, path: &VPath, mode: WriteMode) -> io::Result<VWrite> {
        self.current().open_write(path, mode).await
    }

    async fn set_times(
        &self,
        path: &VPath,
        modified: SystemTime,
        accessed: Option<SystemTime>,
    ) -> io::Result<()> {
        self.current().set_times(path, modified, accessed).await
    }

    async fn set_mode(&self, path: &VPath, mode: u32) -> io::Result<()> {
        self.current().set_mode(path, mode).await
    }

    async fn read_link(&self, path: &VPath) -> io::Result<VPath> {
        self.current().read_link(path).await
    }

    async fn symlink(&self, target: &VPath, link: &VPath, target_is_dir: bool) -> io::Result<()> {
        self.current().symlink(target, link, target_is_dir).await
    }

    fn is_connected(&self) -> bool {
        // The session knows at once; the flag follows a moment later.
        *self.connected.borrow() && !self.current().has_ended()
    }

    fn connection(&self) -> Option<watch::Receiver<bool>> {
        Some(self.connected.subscribe())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sftp::testing::local_sftp;
    use std::time::Duration;

    #[tokio::test]
    async fn a_dropped_session_disconnects_the_mount_and_a_new_one_carries_on() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("file.txt"), b"x").unwrap();
        let Some((first, mut server)) = local_sftp(dir.path()).await else {
            return;
        };
        let mount = SftpMount::new(first);
        let mut connection = mount.connection().unwrap();
        let home = mount.home().await.unwrap();
        assert!(mount.is_connected());
        assert_eq!(mount.list(&home).await.unwrap().len(), 1);

        // The server goes away (as when the network drops).
        server.kill().await.unwrap();
        tokio::time::timeout(Duration::from_secs(5), connection.wait_for(|up| !*up))
            .await
            .expect("the mount notices")
            .unwrap();
        assert!(!mount.is_connected());
        assert!(mount.lost_reason().is_some());
        assert!(mount.list(&home).await.is_err());

        // Reconnect: same mount, new session.
        let (second, _server) = local_sftp(dir.path()).await.unwrap();
        mount.replace(second);
        assert!(*connection.borrow_and_update());
        assert!(mount.is_connected());
        assert_eq!(mount.list(&home).await.unwrap().len(), 1);
        assert!(!mount.session().has_ended());
    }
}
