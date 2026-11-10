#[cfg(target_os = "macos")]
use std::env;
#[cfg(all(target_os = "linux", feature = "unprivileged"))]
use std::ffi::OsString;
#[cfg(any(target_os = "linux", target_os = "macos"))]
use std::fs::File;
#[cfg(any(target_os = "linux", target_os = "freebsd"))]
use std::fs::OpenOptions;
use std::io;
#[cfg(any(target_os = "linux", target_os = "macos"))]
use std::io::Write;
use std::io::{IoSlice, IoSliceMut};
use std::ops::{Deref, DerefMut};
#[cfg(any(
    all(target_os = "linux", feature = "unprivileged"),
    target_os = "freebsd",
))]
use std::os::fd::OwnedFd;
#[cfg(any(target_os = "linux", target_os = "freebsd"))]
use std::os::fd::{AsFd, BorrowedFd};
#[cfg(target_os = "freebsd")]
use std::os::unix::fs::OpenOptionsExt;
#[cfg(any(
    all(target_os = "linux", feature = "unprivileged"),
    target_os = "macos"
))]
use std::os::unix::io::RawFd;
#[cfg(target_os = "macos")]
use std::os::unix::io::{AsRawFd, FromRawFd};
#[cfg(any(
    all(target_os = "linux", feature = "unprivileged"),
    target_os = "macos",
))]
use std::path::Path;
use std::pin::pin;
use std::sync::Arc;

use async_notify::Notify;
use futures_util::lock::Mutex;
use futures_util::{FutureExt, select};
#[cfg(all(target_os = "linux", feature = "unprivileged"))]
use nix::fcntl::{FcntlArg, OFlag};
#[cfg(any(
    all(target_os = "linux", feature = "unprivileged"),
    target_os = "macos",
))]
use nix::sys::socket::{self, AddressFamily, ControlMessageOwned, MsgFlags, SockFlag, SockType};
#[cfg(any(
    all(target_os = "linux", feature = "unprivileged"),
    target_os = "freebsd",
))]
use nix::sys::uio;
#[cfg(any(
    all(target_os = "linux", feature = "unprivileged"),
    target_os = "freebsd",
))]
use tokio::io::Interest;
#[cfg(any(
    all(target_os = "linux", feature = "unprivileged"),
    target_os = "freebsd",
))]
use tokio::io::unix::AsyncFd;
#[cfg(any(
    all(target_os = "linux", feature = "unprivileged"),
    target_os = "macos"
))]
use tokio::process::Command;
#[cfg(any(target_os = "linux", target_os = "macos"))]
use tokio::task;
#[cfg(any(
    all(target_os = "linux", feature = "unprivileged"),
    target_os = "macos"
))]
use tracing::debug;
#[cfg(any(target_os = "freebsd", target_os = "macos"))]
use tracing::warn;

use super::CompleteIoResult;
#[cfg(target_os = "macos")]
use super::mfmount;
#[cfg(any(
    all(target_os = "linux", feature = "unprivileged"),
    target_os = "macos"
))]
use crate::MountOptions;
#[cfg(all(target_os = "linux", feature = "unprivileged"))]
use crate::find_fusermount3;

#[derive(Debug)]
pub struct FuseConnection {
    unmount_notify: Arc<Notify>,
    mode: ConnectionMode,
}

impl FuseConnection {
    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    pub fn new(unmount_notify: Arc<Notify>) -> io::Result<Self> {
        #[cfg(target_os = "freebsd")]
        {
            let connection = NonBlockFuseConnection::new()?;

            Ok(Self {
                unmount_notify,
                mode: ConnectionMode::NonBlock(connection),
            })
        }

        #[cfg(target_os = "linux")]
        {
            let connection = BlockFuseConnection::new()?;

            Ok(Self {
                unmount_notify,
                mode: ConnectionMode::Block(connection),
            })
        }
    }

    #[cfg(all(target_os = "linux", feature = "unprivileged"))]
    pub async fn new_with_unprivileged(
        mount_options: MountOptions,
        mount_path: impl AsRef<Path>,
        unmount_notify: Arc<Notify>,
    ) -> io::Result<Self> {
        let connection =
            NonBlockFuseConnection::new_with_unprivileged(mount_options, mount_path).await?;

        Ok(Self {
            unmount_notify,
            mode: ConnectionMode::NonBlock(connection),
        })
    }

    #[cfg(target_os = "macos")]
    pub async fn new_with_unprivileged(
        mount_options: MountOptions,
        mount_path: impl AsRef<Path>,
        unmount_notify: Arc<Notify>,
    ) -> io::Result<Self> {
        use crate::MacFuseBackend;

        match mount_options.macfuse_backend {
            MacFuseBackend::Kernel => {}
            MacFuseBackend::FsKit => {
                let connection =
                    ChannelFuseConnection::new(&mount_options, mount_path.as_ref()).await?;

                return Ok(Self {
                    unmount_notify,
                    mode: ConnectionMode::Channel(connection),
                });
            }
            MacFuseBackend::Auto => {
                match ChannelFuseConnection::new(&mount_options, mount_path.as_ref()).await {
                    Ok(connection) => {
                        return Ok(Self {
                            unmount_notify,
                            mode: ConnectionMode::Channel(connection),
                        });
                    }
                    Err(err) => {
                        warn!(
                            "macFUSE FSKit backend unavailable ({err}); using the kernel backend"
                        );
                    }
                }
            }
        }

        let connection =
            BlockFuseConnection::new_with_unprivileged(mount_options, mount_path).await?;

        Ok(Self {
            unmount_notify,
            mode: ConnectionMode::Block(connection),
        })
    }

    /// Wait until the volume is mounted. Only an FSKit mount can still be
    /// in progress when the session starts; every other kind is complete.
    #[cfg(target_os = "macos")]
    pub async fn wait_mounted(&self) -> io::Result<()> {
        match &self.mode {
            ConnectionMode::Channel(connection) => connection.wait_mounted().await,
            _ => Ok(()),
        }
    }

    pub async fn read_vectored<T: DerefMut<Target = [u8]> + Send + 'static>(
        &self,
        header_buf: Vec<u8>,
        data_buf: T,
    ) -> Option<CompleteIoResult<(Vec<u8>, T), usize>> {
        let mut unmount_fut = pin!(self.unmount_notify.notified().fuse());
        let mut read_fut = pin!(self.inner_read_vectored(header_buf, data_buf).fuse());

        select! {
            _ = unmount_fut => None,
            res = read_fut => Some(res)
        }
    }

    async fn inner_read_vectored<T: DerefMut<Target = [u8]> + Send + 'static>(
        &self,
        header_buf: Vec<u8>,
        data_buf: T,
    ) -> CompleteIoResult<(Vec<u8>, T), usize> {
        match &self.mode {
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            ConnectionMode::Block(connection) => {
                connection.read_vectored(header_buf, data_buf).await
            }
            #[cfg(any(
                all(target_os = "linux", feature = "unprivileged"),
                target_os = "freebsd",
            ))]
            ConnectionMode::NonBlock(connection) => {
                connection.read_vectored(header_buf, data_buf).await
            }
            #[cfg(target_os = "macos")]
            ConnectionMode::Channel(connection) => {
                connection.read_vectored(header_buf, data_buf).await
            }
        }
    }

    pub async fn write_vectored<T: Deref<Target = [u8]> + Send, U: Deref<Target = [u8]> + Send>(
        &self,
        data: T,
        body_extend_data: Option<U>,
    ) -> CompleteIoResult<(T, Option<U>), usize> {
        match &self.mode {
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            ConnectionMode::Block(connection) => {
                connection.write_vectored(data, body_extend_data).await
            }
            #[cfg(any(
                all(target_os = "linux", feature = "unprivileged"),
                target_os = "freebsd",
            ))]
            ConnectionMode::NonBlock(connection) => {
                connection.write_vectored(data, body_extend_data).await
            }
            #[cfg(target_os = "macos")]
            ConnectionMode::Channel(connection) => {
                connection.write_vectored(data, body_extend_data).await
            }
        }
    }
}

#[derive(Debug)]
enum ConnectionMode {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    Block(BlockFuseConnection),
    #[cfg(any(
        all(target_os = "linux", feature = "unprivileged"),
        target_os = "freebsd",
    ))]
    NonBlock(NonBlockFuseConnection),
    /// macFUSE's FSKit backend: messages over an `MFMount` channel, no fd.
    #[cfg(target_os = "macos")]
    Channel(ChannelFuseConnection),
}

/// A connection over a macFUSE `MFMount` channel (FSKit backend).
///
/// Mirrors [`BlockFuseConnection`]: one blocking receive at a time on the
/// blocking pool, sends serialised by a lock.
#[cfg(target_os = "macos")]
#[derive(Debug)]
struct ChannelFuseConnection {
    channel: Arc<mfmount::Channel>,
    /// `MFMount()` still running when the connection was handed over: it
    /// returns once the volume has finished activating, which needs the
    /// session serving INIT and the first lookups meanwhile.
    mounting: Mutex<Option<task::JoinHandle<io::Result<()>>>>,
    read: Mutex<()>,
    write: Mutex<()>,
}

/// Close the channel when the session lets go of the connection.
///
/// A read the session gave up on (it stops on the unmount notification, not
/// on the read finishing) is still parked in `MFChannelCopyNextMessage` on a
/// blocking thread, and that thread holds its own reference to the channel,
/// so the channel's own drop never runs. Closing here wakes it with `ENODEV`;
/// otherwise it sits there until macFUSE gives up on the volume, about a
/// minute, and a runtime shutdown waits for it. The device-backed connection
/// gets the same effect from its file descriptor closing on drop.
#[cfg(target_os = "macos")]
impl Drop for ChannelFuseConnection {
    fn drop(&mut self) {
        self.channel.close();
    }
}

#[cfg(target_os = "macos")]
impl ChannelFuseConnection {
    async fn new(mount_options: &MountOptions, mount_path: &Path) -> io::Result<Self> {
        let options = mount_options.build_mfmount();
        let (mount_point, options) = mfmount::mount_args(mount_path, &options.to_string_lossy())?;
        let channel = Arc::new(mfmount::Channel::new()?);

        // MFMount() returns only after the volume has activated, which takes
        // the FUSE INIT exchange and the first requests, so it needs the
        // session already reading this channel. The mount runs on its own
        // thread and the connection is handed to the session as soon as the
        // first message (INIT) is waiting; the session then waits for the
        // mount with `wait_mounted`. A failed mount closes the channel, which
        // ends the session.
        debug!("mounting through the macFUSE FSKit backend");
        let mounting = Arc::clone(&channel);
        let mut mount = task::spawn_blocking(move || {
            let result = mounting.mount(&mount_point, &options);
            if result.is_err() {
                mounting.close();
            }
            result
        });
        let waiting = Arc::clone(&channel);
        let mut first_message = task::spawn_blocking(move || waiting.wait_for_message());

        let mounting = tokio::select! {
            mounted = &mut mount => {
                // Finished (or failed) before INIT arrived; wake the waiter.
                channel.interrupt();
                let _ = first_message.await;
                mounted.map_err(io::Error::other)??;
                None
            }
            ready = &mut first_message => {
                match ready.map_err(io::Error::other)? {
                    Ok(true) => Some(mount),
                    // Interrupted or failed waiting: the mount decides.
                    Ok(false) | Err(_) => {
                        mount.await.map_err(io::Error::other)??;
                        None
                    }
                }
            }
        };

        Ok(Self {
            channel,
            mounting: Mutex::new(mounting),
            read: Mutex::new(()),
            write: Mutex::new(()),
        })
    }

    /// Wait for `MFMount()` to finish, once the session is serving.
    async fn wait_mounted(&self) -> io::Result<()> {
        let Some(mount) = self.mounting.lock().await.take() else {
            return Ok(());
        };
        mount.await.map_err(io::Error::other)??;
        debug!("macFUSE FSKit mount complete");
        Ok(())
    }

    async fn read_vectored<T: DerefMut<Target = [u8]> + Send + 'static>(
        &self,
        mut header_buf: Vec<u8>,
        mut data_buf: T,
    ) -> CompleteIoResult<(Vec<u8>, T), usize> {
        let _guard = self.read.lock().await;
        let channel = Arc::clone(&self.channel);

        task::spawn_blocking(move || {
            let res = channel.receive(&mut [&mut header_buf, &mut data_buf]);
            ((header_buf, data_buf), res)
        })
        .await
        .unwrap()
    }

    async fn write_vectored<T: Deref<Target = [u8]> + Send, U: Deref<Target = [u8]> + Send>(
        &self,
        data: T,
        body_extend_data: Option<U>,
    ) -> CompleteIoResult<(T, Option<U>), usize> {
        let _guard = self.write.lock().await;

        // MFChannelSendMessage is not documented as non-blocking (it is an
        // XPC send to macFUSE's extension), so it runs on the blocking pool
        // like the receive: a burst of 1 MiB read replies must not park the
        // runtime's workers. The session lends `data` only for this call, so
        // it is copied into owned buffers for the blocking thread; that copy
        // is small beside the XPC transfer itself.
        //
        // The header and the payload must stay separate iovecs: the FSKit
        // transport reads the first buffer as the structured reply and the
        // second as its payload, as `MFMessageGetBodyBuffers` lays out a
        // request. Joining them into one buffer makes every reply with a
        // body fail with EIO (seen in `tests/macos_fskit.rs`).
        let header = data.to_vec();
        let payload = body_extend_data.as_deref().map(<[u8]>::to_vec);
        let channel = Arc::clone(&self.channel);
        let res = task::spawn_blocking(move || match &payload {
            None => channel.send(&[IoSlice::new(&header)]),
            Some(payload) => channel.send(&[IoSlice::new(&header), IoSlice::new(payload)]),
        })
        .await
        .unwrap_or_else(|err| Err(io::Error::other(err)));

        ((data, body_extend_data), res)
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[derive(Debug)]
struct BlockFuseConnection {
    file: File,
    read: Mutex<()>,
    write: Mutex<()>,
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
impl BlockFuseConnection {
    #[cfg(target_os = "linux")]
    pub fn new() -> io::Result<Self> {
        const DEV_FUSE: &str = "/dev/fuse";

        let file = OpenOptions::new().write(true).read(true).open(DEV_FUSE)?;

        Ok(Self {
            file,
            read: Mutex::new(()),
            write: Mutex::new(()),
        })
    }

    #[cfg(target_os = "macos")]
    async fn new_with_unprivileged(
        mount_options: MountOptions,
        mount_path: impl AsRef<Path>,
    ) -> io::Result<Self> {
        use std::{thread, time::Duration};

        use crate::find_macfuse_mount;

        let (sock0, sock1) = match socket::socketpair(
            AddressFamily::Unix,
            SockType::Stream,
            None,
            SockFlag::empty(),
        ) {
            Err(err) => return Err(err.into()),

            Ok((sock0, sock1)) => (sock0, sock1),
        };

        let binary_path = find_macfuse_mount()?;

        const ENV: &str = "_FUSE_COMMFD";

        let options = mount_options.build();

        debug!("mount options {:?}", options);

        let exec_path = match env::current_exe() {
            Ok(path) => path,
            Err(err) => return Err(err),
        };

        let mount_path = mount_path.as_ref().as_os_str().to_os_string();
        // macfuse_mound will block until fuse init done, so we can not join it in the current function
        tokio::spawn(async move {
            debug!("mount_thread start");
            let fd0 = sock0.as_raw_fd();
            let mut binding = Command::new(binary_path);
            let child = binding
                .env(ENV, fd0.to_string())
                .env("_FUSE_CALL_BY_LIB", "1")
                .env("_FUSE_COMMVERS", "2")
                .env("_FUSE_DAEMON_PATH", exec_path)
                .args(vec![options, mount_path]);
            let status = child.spawn()?.wait().await?;

            if status.success() {
                Ok(())
            } else {
                Err(io::Error::other("fusermount run failed"))
            }
        });

        let fd1 = sock1.as_raw_fd();
        // wait for macfuse mount
        let fd = task::spawn_blocking(move || {
            debug!("wait_thread start");
            // wait for macfuse mount command start
            // it seems that socket::recvmsg will not block to wait for the message
            // so we need to sleep for a while
            thread::sleep(Duration::from_secs(1));
            // let mut buf = vec![0; 10000]; // buf should large enough
            let mut buf = vec![]; // it seems 0 len still works well

            let mut cmsg_buf = nix::cmsg_space!([RawFd; 1]);

            let mut bufs = [IoSliceMut::new(&mut buf)];

            let msg = match socket::recvmsg::<()>(
                fd1,
                &mut bufs[..],
                Some(&mut cmsg_buf),
                MsgFlags::empty(),
            ) {
                Err(err) => return Err(err.into()),

                Ok(msg) => msg,
            };

            let mut cmsgs = match msg.cmsgs() {
                Err(err) => return Err(err.into()),
                Ok(cmsgs) => cmsgs,
            };
            let fd = if let Some(ControlMessageOwned::ScmRights(fds)) = cmsgs.next() {
                if fds.is_empty() {
                    return Err(io::Error::other("no fuse fd"));
                }

                fds[0]
            } else {
                return Err(io::Error::other("get fuse fd failed"));
            };

            Ok(fd)
        })
        .await
        .unwrap()?;

        let file = unsafe { File::from_raw_fd(fd) };
        Ok(Self {
            file,
            read: Mutex::new(()),
            write: Mutex::new(()),
        })
    }

    async fn read_vectored<T: DerefMut<Target = [u8]> + Send + 'static>(
        &self,
        mut header_buf: Vec<u8>,
        mut data_buf: T,
    ) -> CompleteIoResult<(Vec<u8>, T), usize> {
        use std::io::Read;
        use std::mem::ManuallyDrop;
        use std::os::fd::{AsRawFd, FromRawFd};

        let _guard = self.read.lock().await;
        let fd = self.file.as_raw_fd();

        let ((header_buf, data_buf), res) = task::spawn_blocking(move || {
            // Safety: when we call read, the fd is still valid, when fd is closed and file is
            // dropped, the read operation will return error
            let file = unsafe { File::from_raw_fd(fd) };
            // avoid close the file
            let mut file = ManuallyDrop::new(file);

            let res = file.read_vectored(&mut [
                IoSliceMut::new(&mut header_buf),
                IoSliceMut::new(&mut data_buf),
            ]);

            ((header_buf, data_buf), res)
        })
        .await
        .unwrap();

        ((header_buf, data_buf), res)
    }

    async fn write_vectored<T: Deref<Target = [u8]> + Send, U: Deref<Target = [u8]> + Send>(
        &self,
        data: T,
        body_extend_data: Option<U>,
    ) -> CompleteIoResult<(T, Option<U>), usize> {
        let _guard = self.write.lock().await;

        let res = {
            let body_extend_data = body_extend_data.as_deref();

            match body_extend_data {
                None => (&self.file).write_vectored(&[IoSlice::new(data.deref())]),

                Some(body_extend_data) => (&self.file)
                    .write_vectored(&[IoSlice::new(data.deref()), IoSlice::new(body_extend_data)]),
            }
        };

        match res {
            Err(err) => ((data, body_extend_data), Err(err)),
            Ok(n) => ((data, body_extend_data), Ok(n)),
        }
    }
}

#[cfg(any(
    all(target_os = "linux", feature = "unprivileged"),
    target_os = "freebsd",
))]
#[derive(Debug)]
struct NonBlockFuseConnection {
    fd: AsyncFd<OwnedFd>,
    read: Mutex<()>,
    write: Mutex<()>,
}

#[cfg(any(
    all(target_os = "linux", feature = "unprivileged"),
    target_os = "freebsd",
))]
impl NonBlockFuseConnection {
    #[cfg(target_os = "freebsd")]
    fn new() -> io::Result<Self> {
        #[cfg(target_os = "freebsd")]
        const DEV_FUSE: &str = "/dev/fuse";

        match OpenOptions::new()
            .write(true)
            .read(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(DEV_FUSE)
        {
            Err(e) => {
                if e.kind() == io::ErrorKind::NotFound {
                    warn!("Cannot open {}.  Is the module loaded?", DEV_FUSE);
                }
                warn!("Cannot open {}.  err: {:?}", DEV_FUSE, e);
                Err(e)
            }
            Ok(file) => Ok(Self {
                fd: AsyncFd::new(file.into())?,
                read: Mutex::new(()),
                write: Mutex::new(()),
            }),
        }
    }

    #[cfg(all(target_os = "linux", feature = "unprivileged"))]
    async fn new_with_unprivileged(
        mount_options: MountOptions,
        mount_path: impl AsRef<Path>,
    ) -> io::Result<Self> {
        use std::os::fd::{AsRawFd, FromRawFd};

        let (sock0, sock1) = match socket::socketpair(
            AddressFamily::Unix,
            SockType::SeqPacket,
            None,
            SockFlag::empty(),
        ) {
            Err(err) => return Err(err.into()),

            Ok((sock0, sock1)) => (sock0, sock1),
        };

        let binary_path = find_fusermount3()?;

        const ENV: &str = "_FUSE_COMMFD";

        let options = mount_options.build_with_unprivileged();

        debug!("mount options {:?}", options);

        let mount_path = mount_path.as_ref().as_os_str().to_os_string();

        let fd0 = sock0.as_raw_fd();
        let mut child = Command::new(binary_path)
            .env(ENV, fd0.to_string())
            .args(vec![OsString::from("-o"), options, mount_path])
            .spawn()?;

        if !child.wait().await?.success() {
            return Err(io::Error::other("fusermount run failed"));
        }

        let fd1 = sock1.as_raw_fd();
        let fd = task::spawn_blocking(move || {
            // let mut buf = vec![0; 10000]; // buf should large enough
            let mut buf = vec![]; // it seems 0 len still works well

            let mut cmsg_buf = nix::cmsg_space!([RawFd; 1]);

            let mut bufs = [IoSliceMut::new(&mut buf)];

            let msg = match socket::recvmsg::<()>(
                fd1,
                &mut bufs[..],
                Some(&mut cmsg_buf),
                MsgFlags::empty(),
            ) {
                Err(err) => return Err(err.into()),

                Ok(msg) => msg,
            };

            let fd = if let Some(ControlMessageOwned::ScmRights(fds)) = msg.cmsgs()?.next() {
                if fds.is_empty() {
                    return Err(io::Error::other("no fuse fd"));
                }

                fds[0]
            } else {
                return Err(io::Error::other("get fuse fd failed"));
            };

            Ok(fd)
        })
        .await
        .unwrap()?;

        // Safety: fd is valid
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };

        Self::set_fd_non_blocking(fd.as_fd())?;

        Ok(Self {
            fd: AsyncFd::new(fd)?,
            read: Mutex::new(()),
            write: Mutex::new(()),
        })
    }

    #[cfg(all(target_os = "linux", feature = "unprivileged"))]
    fn set_fd_non_blocking(fd: BorrowedFd) -> io::Result<()> {
        let flags = nix::fcntl::fcntl(fd, FcntlArg::F_GETFL).map_err(io::Error::from)?;
        debug!(
            "set fd {:?} to non-blocking",
            OFlag::from_bits_truncate(flags)
        );
        let flags = OFlag::from_bits_truncate(flags) | OFlag::O_NONBLOCK;

        debug!("set fd {:?} to non-blocking", flags);
        nix::fcntl::fcntl(fd, FcntlArg::F_SETFL(flags)).map_err(io::Error::from)?;

        Ok(())
    }

    async fn read_vectored<T: DerefMut<Target = [u8]> + Send>(
        &self,
        mut header_buf: Vec<u8>,
        mut data_buf: T,
    ) -> CompleteIoResult<(Vec<u8>, T), usize> {
        let _guard = self.read.lock().await;

        loop {
            let mut read_guard = match self.fd.ready(Interest::READABLE | Interest::ERROR).await {
                Err(err) => return ((header_buf, data_buf), Err(err)),
                Ok(read_guard) => read_guard,
            };

            if let Ok(result) = read_guard.try_io(|fd| {
                uio::readv(
                    fd,
                    &mut [
                        IoSliceMut::new(&mut header_buf),
                        IoSliceMut::new(&mut data_buf),
                    ],
                )
                .map_err(io::Error::from)
            }) {
                return ((header_buf, data_buf), result);
            } else {
                continue;
            }
        }
    }

    async fn write_vectored<T: Deref<Target = [u8]> + Send, U: Deref<Target = [u8]> + Send>(
        &self,
        data: T,
        body_extend_data: Option<U>,
    ) -> CompleteIoResult<(T, Option<U>), usize> {
        let _guard = self.write.lock().await;

        let res = {
            let body_extend_data = body_extend_data.as_deref();

            match body_extend_data {
                None => uio::writev(&self.fd, &[IoSlice::new(data.deref())]),

                Some(body_extend_data) => uio::writev(
                    &self.fd,
                    &[IoSlice::new(data.deref()), IoSlice::new(body_extend_data)],
                ),
            }
        };

        match res {
            Err(err) => ((data, body_extend_data), Err(err.into())),
            Ok(n) => ((data, body_extend_data), Ok(n)),
        }
    }
}

// Only the root mounts on Linux and FreeBSD need the device fd (to pass it
// in the mount options). On macOS the connection may be an MFMount channel,
// which has no fd at all.
#[cfg(any(target_os = "linux", target_os = "freebsd"))]
impl AsFd for FuseConnection {
    fn as_fd(&self) -> BorrowedFd<'_> {
        match &self.mode {
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            ConnectionMode::Block(connection) => connection.file.as_fd(),

            #[cfg(any(
                all(target_os = "linux", feature = "unprivileged"),
                target_os = "freebsd",
            ))]
            ConnectionMode::NonBlock(connection) => connection.fd.as_fd(),
        }
    }
}
