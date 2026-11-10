//! FUSE transport over macFUSE's `MFMount` channel API (macOS).
//!
//! macFUSE 5 can serve a volume through Apple's FSKit instead of its kernel
//! extension (`backend=fskit`), which removes the kext and its Recovery mode
//! approval on macOS 26 and later. An FSKit mount has no `/dev/macfuse` device
//! and so no file descriptor: FUSE messages are exchanged through an
//! `MFChannelRef` from `MFMount.framework`, and the mount itself is performed
//! over XPC by `MFMount()`. The messages are the ordinary serialized FUSE
//! protocol, so only the transport differs from the device-backed connection.
//!
//! The framework is loaded with `dlopen` on first use rather than linked, so
//! a binary built with this module still starts on a Mac without macFUSE (or
//! with a macFUSE older than [`MINIMUM_MACFUSE`]) and only the FSKit mount
//! fails.
//!
//! API reference: `MFMount.framework/Headers/MFMount.h`. The signatures and
//! the `MFMountResult` values below were checked against the header shipped
//! with macFUSE 5.5.0 (MFMount.framework 2.4).
//!
//! The floor is macFUSE 5.3.3, the first release with `MFChannelInterrupt`,
//! which this transport needs to wake the INIT waiter. macFUSE 5.4.0 added
//! `MFMountResultCreatingMountPointFailed`; on 5.3.x that failure surfaces as
//! `MFMountResultUnexpectedFailure` instead.

use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::io::{self, IoSlice};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

use tracing::debug;

/// Where macFUSE installs the framework.
const MFMOUNT_PATH: &CStr =
    c"/Library/Filesystems/macfuse.fs/Contents/Frameworks/MFMount.framework/MFMount";

/// The oldest macFUSE whose `MFMount.framework` has every symbol used here.
pub(crate) const MINIMUM_MACFUSE: &str = "5.3.3";

type MfTypeRef = *mut c_void;

/// `MFMountResult` from `MFMount.h` (macFUSE 5.5.0). The header spells the
/// first failure `MFMountResultUnsupporteOSVersion`.
const MF_MOUNT_RESULT_SUCCESS: i32 = 0;
const MF_MOUNT_RESULT_UNSUPPORTED_OS_VERSION: i32 = 1;
const MF_MOUNT_RESULT_HELPER_TOOLS_INSTALLATION_FAILED: i32 = 2;
const MF_MOUNT_RESULT_EXTENSION_NOT_FOUND: i32 = 3;
const MF_MOUNT_RESULT_EXTENSION_REQUIRES_APPROVAL: i32 = 4;
/// macFUSE 5.4.0 and later.
const MF_MOUNT_RESULT_CREATING_MOUNT_POINT_FAILED: i32 = 5;
/// errno is meaningful only with this result: `EINVAL` (bad arguments) or
/// `EAGAIN` (anything else).
const MF_MOUNT_RESULT_UNEXPECTED_FAILURE: i32 = -1;

type ReleaseFn = unsafe extern "C" fn(MfTypeRef);
type GetBodyBuffersFn = unsafe extern "C" fn(MfTypeRef, *mut *const libc::iovec) -> libc::ssize_t;
type ChannelCreateFn = unsafe extern "C" fn() -> MfTypeRef;
type CopyNextMessageFn = unsafe extern "C" fn(MfTypeRef) -> MfTypeRef;
type SendMessageFn =
    unsafe extern "C" fn(MfTypeRef, *const libc::iovec, libc::size_t) -> libc::ssize_t;
type ChannelCloseFn = unsafe extern "C" fn(MfTypeRef) -> bool;
type WaitForNextMessageFn = unsafe extern "C" fn(MfTypeRef, i32) -> i32;
type InterruptFn = unsafe extern "C" fn(MfTypeRef) -> bool;
type MountFn = unsafe extern "C" fn(MfTypeRef, *const c_char, *const c_char, bool) -> i32;

/// The `MFMount` entry points this transport uses.
struct Api {
    release: ReleaseFn,
    message_get_body_buffers: GetBodyBuffersFn,
    channel_create: ChannelCreateFn,
    channel_copy_next_message: CopyNextMessageFn,
    channel_send_message: SendMessageFn,
    channel_close: ChannelCloseFn,
    channel_wait_for_next_message: WaitForNextMessageFn,
    channel_interrupt: InterruptFn,
    mount: MountFn,
}

// The table holds only function pointers into a library that is never
// unloaded.
unsafe impl Send for Api {}
unsafe impl Sync for Api {}

static API: OnceLock<Result<Api, String>> = OnceLock::new();

fn api() -> io::Result<&'static Api> {
    API.get_or_init(load)
        .as_ref()
        .map_err(|message| io::Error::new(io::ErrorKind::NotFound, message.clone()))
}

fn load() -> Result<Api, String> {
    // Safety: dlopen/dlsym/dlerror are thread-safe on macOS; the handle is
    // intentionally leaked so the function pointers stay valid forever.
    unsafe {
        let handle = libc::dlopen(MFMOUNT_PATH.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL);
        if handle.is_null() {
            return Err(format!(
                "macFUSE MFMount.framework could not be loaded ({}). The FSKit backend needs \
                 macFUSE {MINIMUM_MACFUSE} or later.",
                last_dl_error()
            ));
        }

        macro_rules! symbol {
            ($name:literal, $ty:ty) => {{
                let ptr = libc::dlsym(handle, concat!($name, "\0").as_ptr().cast());
                if ptr.is_null() {
                    return Err(format!(
                        "MFMount.framework has no {} ({}). The FSKit backend needs \
                         macFUSE {MINIMUM_MACFUSE} or later.",
                        $name,
                        last_dl_error()
                    ));
                }
                std::mem::transmute::<*mut c_void, $ty>(ptr)
            }};
        }

        Ok(Api {
            release: symbol!("MFRelease", ReleaseFn),
            message_get_body_buffers: symbol!("MFMessageGetBodyBuffers", GetBodyBuffersFn),
            channel_create: symbol!("MFChannelCreate", ChannelCreateFn),
            channel_copy_next_message: symbol!("MFChannelCopyNextMessage", CopyNextMessageFn),
            channel_send_message: symbol!("MFChannelSendMessage", SendMessageFn),
            channel_close: symbol!("MFChannelClose", ChannelCloseFn),
            channel_wait_for_next_message: symbol!(
                "MFChannelWaitForNextMessage",
                WaitForNextMessageFn
            ),
            channel_interrupt: symbol!("MFChannelInterrupt", InterruptFn),
            mount: symbol!("MFMount", MountFn),
        })
    }
}

unsafe fn last_dl_error() -> String {
    let err = unsafe { libc::dlerror() };
    if err.is_null() {
        "unknown error".into()
    } else {
        unsafe { CStr::from_ptr(err) }
            .to_string_lossy()
            .into_owned()
    }
}

/// Why `MFMount()` refused to mount, as an `io::Error` a caller can show.
///
/// `errno` is read only for `MFMountResultUnexpectedFailure`; the header says
/// it is not meaningful for any other result.
fn mount_error(result: i32, errno: c_int) -> io::Error {
    let (kind, message) = match result {
        MF_MOUNT_RESULT_UNSUPPORTED_OS_VERSION => (
            io::ErrorKind::Unsupported,
            "this version of macOS does not support the macFUSE FSKit backend",
        ),
        MF_MOUNT_RESULT_HELPER_TOOLS_INSTALLATION_FAILED => (
            io::ErrorKind::Other,
            "macFUSE could not install its helper tools; reinstall macFUSE",
        ),
        MF_MOUNT_RESULT_EXTENSION_NOT_FOUND => (
            io::ErrorKind::NotFound,
            "the macFUSE file system extension was not found; reinstall macFUSE",
        ),
        MF_MOUNT_RESULT_EXTENSION_REQUIRES_APPROVAL => (
            io::ErrorKind::PermissionDenied,
            "the macFUSE file system extension needs approval: turn on macFUSE in System \
             Settings > General > Login Items & Extensions > File System Extensions",
        ),
        MF_MOUNT_RESULT_CREATING_MOUNT_POINT_FAILED => (
            io::ErrorKind::Other,
            "macFUSE could not create the mount point",
        ),
        MF_MOUNT_RESULT_UNEXPECTED_FAILURE => {
            let cause = io::Error::from_raw_os_error(errno);
            return io::Error::new(
                cause.kind(),
                format!("MFMount failed unexpectedly: {cause}"),
            );
        }
        _ => {
            return io::Error::other(format!(
                "MFMount failed with a result this build does not know ({result}); \
                 macFUSE may be newer than fuse3"
            ));
        }
    };
    io::Error::new(kind, message)
}

/// An `MFChannelRef`, closed and released on drop.
#[derive(Debug)]
pub(crate) struct Channel {
    raw: MfTypeRef,
    closed: AtomicBool,
}

// MFMount channels are documented as usable from multiple threads (see
// MFChannelWaitForNextMessage); the session serialises reads and writes
// separately as it does for the device-backed connection.
unsafe impl Send for Channel {}
unsafe impl Sync for Channel {}

impl Channel {
    /// Create a channel not yet associated with a volume.
    pub(crate) fn new() -> io::Result<Self> {
        let api = api()?;
        let raw = unsafe { (api.channel_create)() };
        if raw.is_null() {
            return Err(io::Error::other("MFChannelCreate failed"));
        }
        Ok(Self {
            raw,
            closed: AtomicBool::new(false),
        })
    }

    /// Mount `mount_point` and associate it with this channel.
    ///
    /// Blocks until the volume is mounted or refused, which includes the
    /// FUSE `INIT` exchange: something must be answering messages on this
    /// channel meanwhile, or the mount times out. Call it on its own thread.
    pub(crate) fn mount(&self, mount_point: &CStr, options: &CStr) -> io::Result<()> {
        let api = api()?;
        debug!(?mount_point, ?options, "MFMount");
        // quiet = true: errors are reported to the caller, which can explain
        // them; a daemon should not pop up dialogs on its own.
        let result = unsafe { (api.mount)(self.raw, mount_point.as_ptr(), options.as_ptr(), true) };
        if result == MF_MOUNT_RESULT_SUCCESS {
            Ok(())
        } else {
            Err(mount_error(
                result,
                io::Error::last_os_error().raw_os_error().unwrap_or(0),
            ))
        }
    }

    /// Receive the next FUSE message, scattering it into `bufs` in order.
    ///
    /// Blocks until a message arrives. Returns the message length, like
    /// `readv(2)` on the FUSE device. A message larger than `bufs` is an
    /// `EIO` error rather than a silent truncation.
    pub(crate) fn receive(&self, bufs: &mut [&mut [u8]]) -> io::Result<usize> {
        let api = api()?;
        let message = unsafe { (api.channel_copy_next_message)(self.raw) };
        if message.is_null() {
            return Err(io::Error::last_os_error());
        }
        let _release = Released(message, api.release);

        let mut iovs: *const libc::iovec = std::ptr::null();
        let count = unsafe { (api.message_get_body_buffers)(message, &mut iovs) };
        if count < 0 {
            return Err(io::Error::last_os_error());
        }
        // Safety: MFMessageGetBodyBuffers returns `count` iovecs borrowed from
        // `message`, which stays alive until `_release` drops.
        let iovs = unsafe { std::slice::from_raw_parts(iovs, count as usize) };
        let parts = iovs.iter().map(|iov| unsafe {
            std::slice::from_raw_parts(iov.iov_base as *const u8, iov.iov_len)
        });
        scatter(parts, bufs)
    }

    /// Block until a message is ready to receive (`Ok(true)`), the wait is
    /// interrupted (`Ok(false)`), or the channel fails.
    pub(crate) fn wait_for_message(&self) -> io::Result<bool> {
        let api = api()?;
        match unsafe { (api.channel_wait_for_next_message)(self.raw, -1) } {
            n if n > 0 => Ok(true),
            0 => Ok(false),
            _ => {
                let err = io::Error::last_os_error();
                if err.raw_os_error() == Some(libc::EINTR) {
                    Ok(false)
                } else {
                    Err(err)
                }
            }
        }
    }

    /// Wake a thread blocked in [`wait_for_message`](Self::wait_for_message)
    /// or [`receive`](Self::receive).
    pub(crate) fn interrupt(&self) {
        if let Ok(api) = api() {
            unsafe { (api.channel_interrupt)(self.raw) };
        }
    }

    /// Close the channel, once. Blocked receives then fail with `ENODEV`,
    /// which the session treats as the end of the mount.
    pub(crate) fn close(&self) {
        if self.closed.swap(true, Ordering::AcqRel) {
            return;
        }
        if let Ok(api) = api() {
            unsafe { (api.channel_close)(self.raw) };
        }
    }

    /// Send one complete FUSE message made of `bufs`.
    ///
    /// May block: `MFMount.h` (macFUSE 5.5.0) does not say the send returns
    /// without waiting, and for an FSKit channel it is an XPC message to
    /// macFUSE's extension. Call it from the blocking pool, not a runtime
    /// worker.
    pub(crate) fn send(&self, bufs: &[IoSlice<'_>]) -> io::Result<usize> {
        let api = api()?;
        // IoSlice is ABI-compatible with iovec on Unix.
        let sent = unsafe {
            (api.channel_send_message)(self.raw, bufs.as_ptr().cast::<libc::iovec>(), bufs.len())
        };
        if sent < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(sent as usize)
        }
    }
}

impl Drop for Channel {
    fn drop(&mut self) {
        self.close();
        if let Ok(api) = api() {
            unsafe { (api.release)(self.raw) };
        }
    }
}

/// Releases an MFMount object when dropped.
struct Released(MfTypeRef, ReleaseFn);

impl Drop for Released {
    fn drop(&mut self) {
        unsafe { (self.1)(self.0) }
    }
}

/// Copy the message `parts`, in order, across the destination `bufs`.
///
/// Mirrors what `readv(2)` does with one FUSE message from the device: the
/// header buffer fills first, the rest spills into the data buffer.
fn scatter<'a>(parts: impl Iterator<Item = &'a [u8]>, bufs: &mut [&mut [u8]]) -> io::Result<usize> {
    let mut written = 0;
    let mut buf_index = 0;
    let mut buf_offset = 0;
    for mut part in parts {
        while !part.is_empty() {
            let Some(buf) = bufs.get_mut(buf_index) else {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "FUSE message larger than the read buffer",
                ));
            };
            let room = &mut buf[buf_offset..];
            let n = room.len().min(part.len());
            room[..n].copy_from_slice(&part[..n]);
            part = &part[n..];
            written += n;
            buf_offset += n;
            if buf_offset == buf.len() {
                buf_index += 1;
                buf_offset = 0;
            }
        }
    }
    Ok(written)
}

/// Build the C strings `MFMount()` takes.
pub(crate) fn mount_args(
    mount_point: &std::path::Path,
    options: &str,
) -> io::Result<(CString, CString)> {
    use std::os::unix::ffi::OsStrExt;
    let invalid = |what| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{what} contains a NUL byte"),
        )
    };
    Ok((
        CString::new(mount_point.as_os_str().as_bytes()).map_err(|_| invalid("mount point"))?,
        CString::new(options).map_err(|_| invalid("mount options"))?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scatter_fills_the_header_then_the_data_buffer() {
        let mut header = [0u8; 4];
        let mut data = [0u8; 8];
        let parts: [&[u8]; 2] = [b"abcdef", b"gh"];
        let n = scatter(parts.into_iter(), &mut [&mut header, &mut data]).unwrap();
        assert_eq!(n, 8);
        assert_eq!(&header, b"abcd");
        assert_eq!(&data[..4], b"efgh");
    }

    #[test]
    fn scatter_rejects_a_message_larger_than_the_buffers() {
        let mut header = [0u8; 2];
        let mut data = [0u8; 2];
        let parts: [&[u8]; 1] = [b"abcde"];
        let err = scatter(parts.into_iter(), &mut [&mut header, &mut data]).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn scatter_handles_a_message_that_exactly_fills_both() {
        let mut header = [0u8; 2];
        let mut data = [0u8; 2];
        let parts: [&[u8]; 2] = [b"ab", b"cd"];
        assert_eq!(
            scatter(parts.into_iter(), &mut [&mut header, &mut data]).unwrap(),
            4
        );
        assert_eq!((&header, &data), (b"ab", b"cd"));
    }

    #[test]
    fn approval_is_reported_as_permission_denied() {
        let err = mount_error(MF_MOUNT_RESULT_EXTENSION_REQUIRES_APPROVAL, 0);
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
        assert!(err.to_string().contains("File System Extensions"));
    }

    #[test]
    fn an_unexpected_failure_carries_errno() {
        let err = mount_error(MF_MOUNT_RESULT_UNEXPECTED_FAILURE, libc::EINVAL);
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        assert!(err.to_string().contains("unexpectedly"), "{err}");
    }

    #[test]
    fn an_unknown_result_ignores_errno_and_names_the_value() {
        // errno is not meaningful outside MFMountResultUnexpectedFailure.
        let err = mount_error(42, libc::ENOENT);
        assert_eq!(err.kind(), io::ErrorKind::Other);
        assert!(err.to_string().contains("(42)"), "{err}");
    }

    #[test]
    fn the_mount_point_result_is_mapped() {
        let err = mount_error(MF_MOUNT_RESULT_CREATING_MOUNT_POINT_FAILED, 0);
        assert!(err.to_string().contains("mount point"), "{err}");
    }

    #[test]
    fn mount_args_reject_nul() {
        assert!(mount_args(std::path::Path::new("/tmp/x"), "a\0b").is_err());
        assert!(mount_args(std::path::Path::new("/tmp/x"), "backend=fskit").is_ok());
    }
}
