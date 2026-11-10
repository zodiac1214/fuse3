//! Live mount through macFUSE's FSKit backend (the MFMount channel transport).
//!
//! Ignored by default: needs macOS with macFUSE 5.3.3+ and its file system
//! extension turned on in System Settings. Run with
//! `cargo test --features tokio-runtime,unprivileged --test macos_fskit -- --ignored --nocapture`.

#![cfg(all(target_os = "macos", feature = "tokio-runtime"))]

use std::ffi::{OsStr, OsString};
use std::num::NonZeroU32;
use std::time::{Duration, SystemTime};

use bytes::Bytes;
use fuse3::raw::prelude::*;
use fuse3::{MacFuseBackend, MountOptions, Result};
use futures_util::stream::{self, Stream};

const CONTENT: &[u8] = b"hello through FSKit\n";
const FILE_INO: u64 = 2;
const TTL: Duration = Duration::from_secs(1);

struct Hello;

fn attr(ino: u64) -> FileAttr {
    let now = SystemTime::now().into();
    FileAttr {
        ino,
        size: if ino == FILE_INO {
            CONTENT.len() as u64
        } else {
            0
        },
        blocks: 1,
        atime: now,
        mtime: now,
        ctime: now,
        crtime: now,
        kind: if ino == 1 {
            FileType::Directory
        } else {
            FileType::RegularFile
        },
        perm: if ino == 1 { 0o755 } else { 0o444 },
        nlink: if ino == 1 { 2 } else { 1 },
        uid: unsafe { libc::getuid() },
        gid: unsafe { libc::getgid() },
        rdev: 0,
        flags: 0,
        blksize: 4096,
    }
}

impl Filesystem for Hello {
    async fn init(&self, _req: Request) -> Result<ReplyInit> {
        Ok(ReplyInit::new(NonZeroU32::new(1 << 20).unwrap()))
    }

    async fn destroy(&self, _req: Request) {}

    async fn lookup(&self, _req: Request, parent: u64, name: &OsStr) -> Result<ReplyEntry> {
        if parent == 1 && name == "hello.txt" {
            Ok(ReplyEntry {
                ttl: TTL,
                attr: attr(FILE_INO),
                generation: 0,
            })
        } else {
            Err(libc::ENOENT.into())
        }
    }

    async fn getattr(
        &self,
        _req: Request,
        ino: u64,
        _fh: Option<u64>,
        _flags: u32,
    ) -> Result<ReplyAttr> {
        match ino {
            1 | FILE_INO => Ok(ReplyAttr {
                ttl: TTL,
                attr: attr(ino),
            }),
            _ => Err(libc::ENOENT.into()),
        }
    }

    async fn open(&self, _req: Request, _ino: u64, _flags: u32) -> Result<ReplyOpen> {
        Ok(ReplyOpen { fh: 1, flags: 0 })
    }

    async fn opendir(&self, _req: Request, _ino: u64, _flags: u32) -> Result<ReplyOpen> {
        Ok(ReplyOpen { fh: 1, flags: 0 })
    }

    async fn read(
        &self,
        _req: Request,
        _ino: u64,
        _fh: u64,
        offset: u64,
        size: u32,
    ) -> Result<ReplyData> {
        let start = (offset as usize).min(CONTENT.len());
        let end = (start + size as usize).min(CONTENT.len());
        Ok(ReplyData {
            data: Bytes::copy_from_slice(&CONTENT[start..end]),
        })
    }

    async fn release(
        &self,
        _req: Request,
        _ino: u64,
        _fh: u64,
        _flags: u32,
        _lock_owner: u64,
        _flush: bool,
    ) -> Result<()> {
        Ok(())
    }

    async fn releasedir(&self, _req: Request, _ino: u64, _fh: u64, _flags: u32) -> Result<()> {
        Ok(())
    }

    async fn getxattr(
        &self,
        _req: Request,
        ino: u64,
        name: &OsStr,
        _size: u32,
    ) -> Result<ReplyXAttr> {
        println!("getxattr {ino} {name:?}");
        Err(libc::ENOATTR.into())
    }

    async fn listxattr(&self, _req: Request, _ino: u64, size: u32) -> Result<ReplyXAttr> {
        Ok(if size == 0 {
            ReplyXAttr::Size(0)
        } else {
            ReplyXAttr::Data(Bytes::new())
        })
    }

    async fn access(&self, _req: Request, _ino: u64, _mask: u32) -> Result<()> {
        Ok(())
    }

    async fn statfs(&self, _req: Request, _ino: u64) -> Result<ReplyStatFs> {
        Ok(ReplyStatFs {
            blocks: 1,
            bfree: 0,
            bavail: 0,
            files: 1,
            ffree: 0,
            bsize: 4096,
            namelen: 255,
            frsize: 4096,
        })
    }

    async fn readdir<'a>(
        &'a self,
        _req: Request,
        _parent: u64,
        _fh: u64,
        offset: i64,
    ) -> Result<ReplyDirectory<impl Stream<Item = Result<DirectoryEntry>> + Send + 'a>> {
        let entries = [
            (1, FileType::Directory, "."),
            (1, FileType::Directory, ".."),
            (FILE_INO, FileType::RegularFile, "hello.txt"),
        ]
        .into_iter()
        .enumerate()
        .skip(offset as usize)
        .map(|(i, (inode, kind, name))| {
            Ok(DirectoryEntry {
                inode,
                kind,
                name: OsString::from(name),
                offset: i as i64 + 1,
            })
        })
        .collect::<Vec<_>>();
        Ok(ReplyDirectory {
            entries: stream::iter(entries),
        })
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires macFUSE 5.3.3+ with its FSKit extension enabled"]
async fn a_volume_mounts_and_serves_through_the_fskit_backend() {
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::DEBUG)
        .with_test_writer()
        .try_init();

    let dir = std::env::temp_dir().join(format!("fuse3-fskit-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();

    let mut options = MountOptions::default();
    options
        .macfuse_backend(MacFuseBackend::FsKit)
        .read_only(true)
        .custom_options("volname=fuse3-fskit");

    let started = std::time::Instant::now();
    let handle = Session::new(options)
        .mount_with_unprivileged(Hello, &dir)
        .await
        .expect("FSKit mount");
    println!(
        "mount_with_unprivileged returned after {:?}",
        started.elapsed()
    );

    let mount_table = String::from_utf8(
        std::process::Command::new("/sbin/mount")
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap();
    let line = mount_table
        .lines()
        .find(|l| l.contains(dir.to_str().unwrap()))
        .map(str::to_owned);
    println!("mount table: {line:?}");

    // Read through child processes: the volume serves other processes, which
    // is how it is used (X-Plane reads, XEarthLayer serves).
    let run = |program: &'static str, arg: std::path::PathBuf| async move {
        let child = tokio::process::Command::new(program)
            .arg(arg)
            .kill_on_drop(true)
            .output();
        tokio::time::timeout(Duration::from_secs(20), child)
            .await
            .unwrap_or_else(|_| panic!("{program} on the volume timed out"))
            .unwrap()
    };
    let ls = run("/bin/ls", dir.clone()).await;
    println!(
        "ls: {:?} {:?}",
        ls.status,
        String::from_utf8_lossy(&ls.stdout)
    );
    let stat = run("/usr/bin/stat", dir.join("hello.txt")).await;
    println!(
        "stat: {:?} {:?}",
        stat.status,
        String::from_utf8_lossy(&stat.stdout)
    );
    let cat = run("/bin/cat", dir.join("hello.txt")).await;
    let content = cat.stdout;
    let names: Vec<String> = String::from_utf8_lossy(&ls.stdout)
        .lines()
        .map(str::to_owned)
        .collect();
    println!(
        "cat: {:?} {:?}",
        cat.status,
        String::from_utf8_lossy(&cat.stderr)
    );

    handle.unmount().await.expect("unmount");
    let _ = std::fs::remove_dir(&dir);

    // An FSKit volume is listed as (macFUSE 5.5.0, macOS 26.6):
    //   macfuse://<UUID> on <dir> (macfuse, nodev, nosuid, noowners, noatime,
    //   fskit, mounted by <user>)
    // The `fskit` flag is what tells it from a kernel-extension mount.
    let line = line.expect("volume must be in the mount table");
    let flags = line
        .rsplit_once(" (")
        .map(|(_, flags)| flags.trim_end_matches(')'))
        .unwrap_or_default();
    assert!(
        line.starts_with("macfuse://") && flags.split(", ").any(|flag| flag == "fskit"),
        "not an FSKit mount: {line}"
    );
    assert_eq!(content, CONTENT);
    assert_eq!(names, ["hello.txt"]);
}
