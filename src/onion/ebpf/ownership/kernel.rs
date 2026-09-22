//! Small Linux BPF operations absent from Aya's public cgroup-link API.

use std::ffi::CString;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

// Linux uapi linux/bpf.h. Keep the input layouts explicit and zero padded.
const BPF_OBJ_GET: i32 = 7;
const BPF_OBJ_GET_INFO_BY_FD: i32 = 15;
const BPF_LINK_UPDATE: i32 = 29;
const BPF_F_REPLACE: u32 = 4;
const BPF_LINK_TYPE_CGROUP: u32 = 3;

#[derive(Default)]
#[repr(C)]
struct ObjectAttributes {
    pathname: u64,
    bpf_fd: u32,
    file_flags: u32,
}

#[repr(C)]
struct InfoAttributes {
    bpf_fd: u32,
    info_len: u32,
    info: u64,
}

#[derive(Debug, Default)]
#[repr(C)]
pub(super) struct LinkInfo {
    link_type: u32,
    pub id: u32,
    pub program_id: u32,
    _pad: u32,
    pub cgroup_id: u64,
    pub attach_type: u32,
    _tail_pad: u32,
}

#[repr(C)]
struct UpdateAttributes {
    link_fd: u32,
    new_program_fd: u32,
    flags: u32,
    old_program_fd: u32,
}

pub(super) fn open_link(path: &Path) -> io::Result<OwnedFd> {
    let path = CString::new(path.as_os_str().as_bytes()).map_err(io::Error::other)?;
    let input = ObjectAttributes {
        pathname: path.as_ptr() as u64,
        ..Default::default()
    };
    // SAFETY: input has the Linux uapi layout and references a live NUL-terminated
    // pathname. The kernel reads only the given initialised input size.
    let result = unsafe {
        nix::libc::syscall(
            nix::libc::SYS_bpf,
            BPF_OBJ_GET,
            &input,
            std::mem::size_of_val(&input),
        )
    };
    if result < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: successful BPF_OBJ_GET returns a new, owned close-on-exec descriptor.
    Ok(unsafe { OwnedFd::from_raw_fd(result as RawFd) })
}

pub(super) fn link_info(fd: &OwnedFd) -> io::Result<LinkInfo> {
    let mut info = LinkInfo::default();
    let mut input = InfoAttributes {
        bpf_fd: fd.as_raw_fd() as u32,
        info_len: std::mem::size_of_val(&info) as u32,
        info: (&mut info as *mut LinkInfo) as u64,
    };
    // SAFETY: the writable output buffer and initialised uapi input remain live
    // for the syscall. The requested prefix fits the LinkInfo allocation.
    let result = unsafe {
        nix::libc::syscall(
            nix::libc::SYS_bpf,
            BPF_OBJ_GET_INFO_BY_FD,
            &mut input,
            std::mem::size_of::<InfoAttributes>(),
        )
    };
    if result < 0 {
        return Err(io::Error::last_os_error());
    }
    if input.info_len < 28 {
        return Err(io::Error::other(
            "kernel cgroup link information is incomplete",
        ));
    }
    if info.link_type != BPF_LINK_TYPE_CGROUP {
        return Err(io::Error::other("pinned object is not a cgroup link"));
    }
    Ok(info)
}

pub(super) fn replace_link(fd: &OwnedFd, new_program: RawFd, old_program: RawFd) -> io::Result<()> {
    let input = UpdateAttributes {
        link_fd: fd.as_raw_fd() as u32,
        new_program_fd: new_program as u32,
        flags: BPF_F_REPLACE,
        old_program_fd: old_program as u32,
    };
    // SAFETY: all three descriptors are held by the caller for this syscall.
    // The initialised repr(C) input matches Linux's link_update attributes.
    let result = unsafe {
        nix::libc::syscall(
            nix::libc::SYS_bpf,
            BPF_LINK_UPDATE,
            &input,
            std::mem::size_of_val(&input),
        )
    };
    if result < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[repr(C)]
struct CreateAttributes {
    program_fd: u32,
    target_fd: u32,
    attach_type: u32,
    flags: u32,
}

pub(super) fn create_link(program: RawFd, cgroup: RawFd, attach_type: u32) -> io::Result<OwnedFd> {
    let input = CreateAttributes {
        program_fd: program as u32,
        target_fd: cgroup as u32,
        attach_type,
        flags: 0,
    };
    // SAFETY: the caller holds the program and cgroup descriptors. This is the
    // initialised cgroup prefix of Linux's BPF_LINK_CREATE (28) attributes.
    let result = unsafe {
        nix::libc::syscall(
            nix::libc::SYS_bpf,
            28,
            &input,
            std::mem::size_of_val(&input),
        )
    };
    if result < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: successful BPF_LINK_CREATE returns a new owned descriptor.
    Ok(unsafe { OwnedFd::from_raw_fd(result as RawFd) })
}

pub(super) fn pin_link(fd: &OwnedFd, path: &Path) -> io::Result<()> {
    let path = CString::new(path.as_os_str().as_bytes()).map_err(io::Error::other)?;
    let input = ObjectAttributes {
        pathname: path.as_ptr() as u64,
        bpf_fd: fd.as_raw_fd() as u32,
        file_flags: 0,
    };
    // SAFETY: the descriptor and NUL-terminated pathname remain live throughout
    // BPF_OBJ_PIN (6). ObjectAttributes is the initialised Linux uapi layout.
    let result =
        unsafe { nix::libc::syscall(nix::libc::SYS_bpf, 6, &input, std::mem::size_of_val(&input)) };
    if result < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}
