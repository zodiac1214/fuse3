use std::ffi::OsString;
#[cfg(target_os = "linux")]
use std::os::unix::io::RawFd;

#[cfg(target_os = "freebsd")]
use nix::mount::Nmount;
#[cfg(target_os = "linux")]
use nix::unistd;

/// Which macFUSE backend serves a mount (macOS only).
#[cfg(target_os = "macos")]
#[derive(Debug, Clone, Copy, Default, Eq, PartialEq)]
pub enum MacFuseBackend {
    /// The macFUSE kernel extension, through `/dev/macfuse`. Works on every
    /// macOS macFUSE supports, but the kext needs Recovery mode approval on
    /// Apple Silicon.
    #[default]
    Kernel,
    /// macFUSE's FSKit backend, through its `MFMount` channel API (macFUSE
    /// 5.3.3+). No kernel extension; the user turns macFUSE on once under
    /// File System Extensions in System Settings.
    FsKit,
    /// FSKit when macFUSE can mount with it, the kernel extension otherwise
    /// (macFUSE missing or too old, extension not enabled, unsupported macOS).
    Auto,
}

/// mount options.
#[derive(Debug, Clone, Default, Eq, PartialEq)]
pub struct MountOptions {
    // Options implemented within fuse3
    pub(crate) nonempty: bool,

    // mount options
    pub(crate) allow_other: bool,
    pub(crate) allow_root: bool,
    pub(crate) custom_options: Option<OsString>,
    #[cfg(target_os = "linux")]
    pub(crate) dirsync: bool,
    pub(crate) default_permissions: bool,
    pub(crate) fs_name: Option<String>,
    pub(crate) gid: Option<u32>,
    #[cfg(any(target_os = "macos", target_os = "freebsd"))]
    pub(crate) intr: bool,
    #[cfg(target_os = "linux")]
    pub(crate) nodiratime: bool,
    pub(crate) noatime: bool,
    #[cfg(target_os = "linux")]
    pub(crate) nodev: bool,
    pub(crate) noexec: bool,
    pub(crate) nosuid: bool,
    pub(crate) read_only: bool,
    #[cfg(target_os = "freebsd")]
    pub(crate) suiddir: bool,
    pub(crate) sync: bool,
    pub(crate) uid: Option<u32>,

    // Optional FUSE features
    pub(crate) dont_mask: bool,
    pub(crate) no_open_support: bool,
    pub(crate) no_open_dir_support: bool,
    pub(crate) handle_killpriv: bool,
    pub(crate) write_back: bool,
    pub(crate) force_readdir_plus: bool,

    // Other FUSE mount options
    // default 40000
    pub(crate) rootmode: Option<u32>,

    // macFUSE backend: kernel extension, FSKit through MFMount, or either
    #[cfg(target_os = "macos")]
    pub(crate) macfuse_backend: MacFuseBackend,
}

impl MountOptions {
    /// set fuse filesystem mount `user_id`, default is current uid.
    pub fn uid(&mut self, uid: u32) -> &mut Self {
        self.uid.replace(uid);

        self
    }

    /// set fuse filesystem mount `group_id`, default is current gid.
    pub fn gid(&mut self, gid: u32) -> &mut Self {
        self.gid.replace(gid);

        self
    }

    /// set fuse filesystem name, default is **fuse**.
    pub fn fs_name(&mut self, name: impl Into<String>) -> &mut Self {
        self.fs_name.replace(name.into());

        self
    }

    /// set fuse filesystem `rootmode`, default is 40000.
    #[cfg(target_os = "linux")]
    pub fn rootmode(&mut self, rootmode: u32) -> &mut Self {
        self.rootmode.replace(rootmode);

        self
    }

    /// set fuse filesystem `allow_root` mount option, default is disable.
    pub fn allow_root(&mut self, allow_root: bool) -> &mut Self {
        self.allow_root = allow_root;

        self
    }

    /// set fuse filesystem `allow_other` mount option, default is disable.
    pub fn allow_other(&mut self, allow_other: bool) -> &mut Self {
        self.allow_other = allow_other;

        self
    }

    /// set fuse filesystem `ro` mount option, default is disable.
    pub fn read_only(&mut self, read_only: bool) -> &mut Self {
        self.read_only = read_only;

        self
    }

    /// allow fuse filesystem mount on a non-empty directory, default is not allowed.
    pub fn nonempty(&mut self, nonempty: bool) -> &mut Self {
        self.nonempty = nonempty;

        self
    }

    /// set fuse filesystem `default_permissions` mount option, default is disable.
    ///
    /// When `default_permissions` is set, the [`raw::access`] and [`path::access`] is useless.
    ///
    /// [`raw::access`]: crate::raw::Filesystem::access
    /// [`path::access`]: crate::path::PathFilesystem::access
    pub fn default_permissions(&mut self, default_permissions: bool) -> &mut Self {
        self.default_permissions = default_permissions;

        self
    }

    /// don't apply umask to file mode on create operations, default is disable.
    pub fn dont_mask(&mut self, dont_mask: bool) -> &mut Self {
        self.dont_mask = dont_mask;

        self
    }

    /// make kernel support zero-message opens, default is disable
    pub fn no_open_support(&mut self, no_open_support: bool) -> &mut Self {
        self.no_open_support = no_open_support;

        self
    }

    /// make kernel support zero-message opendir, default is disable
    pub fn no_open_dir_support(&mut self, no_open_dir_support: bool) -> &mut Self {
        self.no_open_dir_support = no_open_dir_support;

        self
    }

    /// fs handle killing `suid`/`sgid`/`cap` on `write`/`chown`/`trunc`, default is disable.
    pub fn handle_killpriv(&mut self, handle_killpriv: bool) -> &mut Self {
        self.handle_killpriv = handle_killpriv;

        self
    }

    /// try to set the `FUSE_WRITEBACK_CACHE` enable write back cache for buffered writes, default
    /// is disable.
    ///
    /// # Notes:
    ///
    /// if enable this feature, when write flags has `FUSE_WRITE_CACHE`, file handle is guessed.
    pub fn write_back(&mut self, write_back: bool) -> &mut Self {
        self.write_back = write_back;

        self
    }

    /// force filesystem use readdirplus only, when kernel use readdir will return `ENOSYS`,
    /// default is disable.
    ///
    /// # Notes:
    /// this may don't work with some old Linux Kernel.
    pub fn force_readdir_plus(&mut self, force_readdir_plus: bool) -> &mut Self {
        self.force_readdir_plus = force_readdir_plus;

        self
    }

    /// set custom options for fuse filesystem, the custom options will be used in mount
    /// Choose the macFUSE backend (macOS only). Default is
    /// [`MacFuseBackend::Kernel`].
    ///
    /// The FSKit backend needs no kernel extension, so none of the Recovery
    /// mode approval the kext requires on Apple Silicon. FUSE messages travel
    /// over macFUSE's `MFMount` channel API instead of `/dev/macfuse`, so the
    /// filesystem itself is unchanged.
    #[cfg(target_os = "macos")]
    pub fn macfuse_backend(&mut self, backend: MacFuseBackend) -> &mut Self {
        self.macfuse_backend = backend;

        self
    }

    pub fn custom_options(&mut self, custom_options: impl Into<OsString>) -> &mut Self {
        self.custom_options = Some(custom_options.into());

        self
    }

    #[cfg(target_os = "freebsd")]
    pub(crate) fn build(&self) -> Nmount<'_> {
        let mut nmount = Nmount::new();
        nmount
            .str_opt(c"fstype", c"fusefs")
            .str_opt(c"from", c"/dev/fuse");
        if self.allow_other {
            nmount.null_opt(c"allow_other");
        }
        if self.allow_root {
            nmount.null_opt(c"allow_root");
        }
        if self.default_permissions {
            nmount.null_opt(c"default_permissions");
        }
        if let Some(fs_name) = &self.fs_name {
            nmount.str_opt_owned(c"subtype=", fs_name.as_str());
        }
        if self.intr {
            nmount.null_opt(c"intr");
        }
        if let Some(custom_options) = self.custom_options.as_ref() {
            nmount.null_opt_owned(custom_options.as_os_str());
        }
        // TODO: additional options: push_symlinks_in, max_read=, timeout=
        nmount
    }

    #[cfg(target_os = "linux")]
    pub(crate) fn build(&self, fd: RawFd) -> OsString {
        let mut opts = vec![
            format!("fd={fd}"),
            format!(
                "user_id={}",
                self.uid.unwrap_or_else(|| unistd::getuid().as_raw())
            ),
            format!(
                "group_id={}",
                self.gid.unwrap_or_else(|| unistd::getgid().as_raw())
            ),
            format!("rootmode={}", self.rootmode.unwrap_or(40000)),
        ];

        if self.allow_root {
            opts.push("allow_root".to_string());
        }

        if self.allow_other {
            opts.push("allow_other".to_string());
        }

        if self.default_permissions {
            opts.push("default_permissions".to_string());
        }

        let mut options = OsString::from(opts.join(","));

        if let Some(custom_options) = &self.custom_options {
            options.push(",");
            options.push(custom_options);
        }

        options
    }

    /// The `-o` argument for macFUSE's `mount_macfuse` helper (kernel backend).
    #[cfg(target_os = "macos")]
    pub(crate) fn build(&self) -> OsString {
        // macFUSE expects a single `-o` followed by a comma-separated option
        // list (like the Linux builder), NOT repeated space-separated `-o`
        // flags. The old form (`-o fsname=ofs -o allow_root`) was passed to the
        // mount helper as one argument, so every option after the first was
        // silently dropped — e.g. a caller-supplied `nobrowse` never took effect.
        let mut options = OsString::from("-o ");
        options.push(self.macos_option_list());
        options
    }

    /// The option string for `MFMount()` (FSKit backend): the same list the
    /// kernel backend gets, without `-o`, selecting the FSKit backend.
    #[cfg(all(target_os = "macos", feature = "tokio-runtime"))]
    pub(crate) fn build_mfmount(&self) -> OsString {
        let mut options = self.macos_option_list();
        options.push(",backend=fskit");
        options
    }

    #[cfg(target_os = "macos")]
    fn macos_option_list(&self) -> OsString {
        let mut opts = vec![String::from("fsname=ofs")];

        if self.allow_root {
            opts.push("allow_root".to_string());
        }

        if self.allow_other {
            opts.push("allow_other".to_string());
        }

        let mut options = OsString::from(opts.join(","));

        if let Some(custom_options) = &self.custom_options {
            options.push(",");
            options.push(custom_options);
        }

        options
    }

    #[cfg(all(target_os = "linux", feature = "unprivileged"))]
    pub(crate) fn build_with_unprivileged(&self) -> OsString {
        let mut opts = vec![
            format!(
                "user_id={}",
                self.uid.unwrap_or_else(|| unistd::getuid().as_raw())
            ),
            format!(
                "group_id={}",
                self.gid.unwrap_or_else(|| unistd::getgid().as_raw())
            ),
            format!("rootmode={}", self.rootmode.unwrap_or(40000)),
            format!(
                "fsname={}",
                self.fs_name.as_ref().unwrap_or(&"fuse".to_string())
            ),
        ];

        if self.allow_root {
            opts.push("allow_root".to_string());
        }

        if self.allow_other {
            opts.push("allow_other".to_string());
        }

        if self.read_only {
            opts.push("ro".to_string());
        }

        if self.default_permissions {
            opts.push("default_permissions".to_string());
        }

        let mut options = OsString::from(opts.join(","));

        if let Some(custom_options) = &self.custom_options {
            options.push(",");
            options.push(custom_options);
        }

        options
    }

    #[cfg(target_os = "freebsd")]
    pub(crate) fn flags(&self) -> nix::mount::MntFlags {
        use nix::mount::MntFlags;

        let mut flags = MntFlags::empty();
        if self.noatime {
            flags.insert(MntFlags::MNT_NOATIME);
        }
        if self.noexec {
            flags.insert(MntFlags::MNT_NOEXEC);
        }
        if self.nosuid {
            flags.insert(MntFlags::MNT_NOSUID);
        }
        if self.read_only {
            flags.insert(MntFlags::MNT_RDONLY);
        }
        if self.suiddir {
            flags.insert(MntFlags::MNT_SUIDDIR);
        }
        if self.sync {
            flags.insert(MntFlags::MNT_SYNCHRONOUS);
        }
        flags
    }

    #[cfg(target_os = "linux")]
    pub(crate) fn flags(&self) -> nix::mount::MsFlags {
        use nix::mount::MsFlags;

        let mut flags = MsFlags::empty();
        if self.dirsync {
            flags.insert(MsFlags::MS_DIRSYNC);
        }
        if self.noatime {
            flags.insert(MsFlags::MS_NOATIME);
        }
        if self.nodev {
            flags.insert(MsFlags::MS_NODEV);
        }
        if self.nodiratime {
            flags.insert(MsFlags::MS_NODIRATIME);
        }
        if self.noexec {
            flags.insert(MsFlags::MS_NOEXEC);
        }
        if self.nosuid {
            flags.insert(MsFlags::MS_NOSUID);
        }
        if self.read_only {
            flags.insert(MsFlags::MS_RDONLY);
        }
        if self.sync {
            flags.insert(MsFlags::MS_SYNCHRONOUS);
        }
        flags
    }
}

#[cfg(all(test, target_os = "macos"))]
mod macos_tests {
    use super::*;

    fn options() -> MountOptions {
        let mut options = MountOptions::default();
        options
            .allow_root(true)
            .custom_options("volname=v,nobrowse");
        options
    }

    #[test]
    fn the_kernel_backend_gets_one_comma_joined_dash_o() {
        assert_eq!(
            options().build(),
            OsString::from("-o fsname=ofs,allow_root,volname=v,nobrowse")
        );
    }

    #[cfg(feature = "tokio-runtime")]
    #[test]
    fn mfmount_gets_the_same_list_without_dash_o_and_selects_fskit() {
        assert_eq!(
            options().build_mfmount(),
            OsString::from("fsname=ofs,allow_root,volname=v,nobrowse,backend=fskit")
        );
    }

    #[test]
    fn the_kernel_backend_is_the_default() {
        let mut options = MountOptions::default();
        assert_eq!(options.macfuse_backend, MacFuseBackend::Kernel);
        options.macfuse_backend(MacFuseBackend::Auto);
        assert_eq!(options.macfuse_backend, MacFuseBackend::Auto);
    }
}
