"""Run the operation one Linux capability guards, and say whether it worked.

Used by `runc_capability_knobs_work_only_where_the_container_owns_the_check`
in src/grill/runc.rs (#1474). The test runs it as root on the host, where
every check passes, and inside a runc container's user namespace with every
capability there, to find which capabilities the kernel checks against a
namespace the container owns.

    python3 capability_probe.py CAP_NAME [--drop] [--file PATH] [--dir PATH]

Prints one line: "allowed: ..." when the capability check passed, "denied:
..." when it failed. `--drop` clears the capability from the effective set
first, to show the probe needs it. `--file` is a regular file owned by
someone other than the caller; `--dir` is a directory the caller may write.
"""

import ctypes
import errno
import mmap
import os
import platform
import resource
import select
import socket
import struct
import sys
import time

LIBC = ctypes.CDLL(None, use_errno=True)

CAPABILITY_NUMBERS = {
    "CAP_CHOWN": 0,
    "CAP_DAC_READ_SEARCH": 2,
    "CAP_IPC_LOCK": 14,
    "CAP_IPC_OWNER": 15,
    "CAP_LEASE": 28,
    "CAP_LINUX_IMMUTABLE": 9,
    "CAP_MKNOD": 27,
    "CAP_SYS_ADMIN": 21,
    "CAP_SYS_BOOT": 22,
    "CAP_SYS_MODULE": 16,
    "CAP_SYS_NICE": 23,
    "CAP_SYS_PACCT": 20,
    "CAP_SYS_PTRACE": 19,
    "CAP_SYS_RAWIO": 17,
    "CAP_SYS_RESOURCE": 24,
    "CAP_SYS_TIME": 25,
    "CAP_SYS_TTY_CONFIG": 26,
    "CAP_AUDIT_WRITE": 29,
    "CAP_AUDIT_CONTROL": 30,
    "CAP_SETFCAP": 31,
    "CAP_MAC_ADMIN": 33,
    "CAP_SYSLOG": 34,
    "CAP_WAKE_ALARM": 35,
    "CAP_BLOCK_SUSPEND": 36,
    "CAP_AUDIT_READ": 37,
    "CAP_PERFMON": 38,
    "CAP_BPF": 39,
    "CAP_CHECKPOINT_RESTORE": 40,
}

# System calls glibc doesn't wrap, by machine.
SYSCALLS = {
    "x86_64": {"init_module": 175, "bpf": 321, "perf_event_open": 298,
               "name_to_handle_at": 303, "open_by_handle_at": 304},
    "aarch64": {"init_module": 105, "bpf": 280, "perf_event_open": 241,
                "name_to_handle_at": 264, "open_by_handle_at": 265},
}[platform.machine()]

# The errors a refused capability check returns.
DENIALS = {errno.EPERM, errno.EACCES, errno.ECONNREFUSED}


class Denied(Exception):
    pass


def check(result):
    if result == -1:
        number = ctypes.get_errno()
        raise OSError(number, os.strerror(number))
    return result


def syscall(name, *arguments):
    LIBC.syscall.restype = ctypes.c_long
    return check(LIBC.syscall(ctypes.c_long(SYSCALLS[name]), *arguments))


# Capabilities the kernel also accepts in place of the one probed.
ALSO_GRANTED_BY = {"CAP_CHECKPOINT_RESTORE": ["CAP_SYS_ADMIN"]}


def drop(capability):
    """Clear a capability, and any the kernel accepts instead, from the
    effective set."""
    header = struct.pack("=Ii", 0x20080522, 0)
    data = ctypes.create_string_buffer(24)
    check(LIBC.capget(header, data))
    sets = list(struct.unpack("=6I", data.raw))
    for name in [capability, *ALSO_GRANTED_BY.get(capability, [])]:
        number = CAPABILITY_NUMBERS[name]
        sets[(number // 32) * 3] &= ~(1 << (number % 32))
    check(LIBC.capset(header, struct.pack("=6I", *sets)))


def audit(groups=0):
    """Ask the audit subsystem for its status (or join its log multicast)."""
    sock = socket.socket(socket.AF_NETLINK, socket.SOCK_RAW, 9)
    with sock:
        sock.bind((0, groups))
        if groups:
            return "joined the audit log multicast group"
        # AUDIT_GET, NLM_F_REQUEST | NLM_F_ACK
        sock.send(struct.pack("=IHHII", 16, 1000, 1 | 4, 1, 0))
        reply = sock.recv(8192)
        kind = struct.unpack("=H", reply[4:6])[0]
        if kind == 2:
            code = struct.unpack("=i", reply[16:20])[0]
            if code < 0:
                raise OSError(-code, os.strerror(-code))
        return "audit answered AUDIT_GET"


def block_suspend():
    epoll = select.epoll()
    event = os.eventfd(0)
    try:
        epoll.register(event, select.EPOLLIN | (1 << 29))
        with open(f"/proc/self/fdinfo/{epoll.fileno()}") as info:
            line = next(line for line in info if line.startswith("tfd:"))
        mask = int(line.split("events:")[1].split()[0], 16)
        if not mask & (1 << 29):
            raise Denied("the kernel dropped EPOLLWAKEUP")
        return "EPOLLWAKEUP kept"
    finally:
        os.close(event)
        epoll.close()


def bpf():
    # BPF_MAP_CREATE of a stack-trace map, a type only CAP_BPF may create.
    attributes = ctypes.create_string_buffer(struct.pack("=IIII", 7, 4, 8, 1), 128)
    os.close(syscall("bpf", 0, attributes, 128))
    return "created a stack-trace map"


def checkpoint_restore():
    path = "/proc/sys/kernel/ns_last_pid"
    with open(path) as current:
        value = current.read()
    with open(path, "w") as last:
        last.write(value)
    return "set this pid namespace's ns_last_pid"


def dac_read_search(file):
    handle = ctypes.create_string_buffer(struct.pack("=Ii", 128, 0), 136)
    mount_id = ctypes.c_int()
    syscall("name_to_handle_at", -100, file.encode(), handle, ctypes.byref(mount_id), 0)
    mount = os.open(os.path.dirname(file), os.O_RDONLY)
    try:
        os.close(syscall("open_by_handle_at", mount, handle, os.O_RDONLY))
    finally:
        os.close(mount)
    return "opened a file by handle"


def ipc_lock():
    soft, hard = resource.getrlimit(resource.RLIMIT_MEMLOCK)
    resource.setrlimit(resource.RLIMIT_MEMLOCK, (64 * 1024, hard))
    size = 1024 * 1024
    region = mmap.mmap(-1, size)
    address = ctypes.addressof(ctypes.c_char.from_buffer(region))
    try:
        check(LIBC.mlock(ctypes.c_void_p(address), ctypes.c_size_t(size)))
        LIBC.munlock(ctypes.c_void_p(address), ctypes.c_size_t(size))
    except OSError as error:
        if error.errno == errno.ENOMEM:
            raise Denied("ENOMEM: locked memory is limited to RLIMIT_MEMLOCK")
        raise
    return "locked 1 MiB beyond a 64 KiB RLIMIT_MEMLOCK"


def ipc_owner():
    LIBC.shmat.restype = ctypes.c_void_p
    segment = check(LIBC.shmget(0, 4096, 0o1000))  # IPC_PRIVATE, IPC_CREAT, mode 0
    try:
        address = LIBC.shmat(segment, None, 0o10000)  # SHM_RDONLY
        if address == ctypes.c_void_p(-1).value:
            check(-1)
        LIBC.shmdt(ctypes.c_void_p(address))
    finally:
        LIBC.shmctl(segment, 0, None)  # IPC_RMID
    return "attached a mode-0 shared memory segment"


def lease(file):
    descriptor = os.open(file, os.O_RDONLY)
    try:
        import fcntl
        fcntl.fcntl(descriptor, 1024, 0)  # F_SETLEASE, F_RDLCK
        fcntl.fcntl(descriptor, 1024, 2)  # F_UNLCK
    finally:
        os.close(descriptor)
    return "took a lease on another user's file"


def linux_immutable(file):
    import fcntl
    get_flags, set_flags, immutable = 0x80086601, 0x40086602, 0x10
    descriptor = os.open(file, os.O_RDONLY)
    try:
        flags = struct.unpack("=i", fcntl.ioctl(descriptor, get_flags, b"\0" * 4))[0]
        fcntl.ioctl(descriptor, set_flags, struct.pack("=i", flags | immutable))
        fcntl.ioctl(descriptor, set_flags, struct.pack("=i", flags))
    finally:
        os.close(descriptor)
    return "set and cleared the immutable flag"


def mac_admin():
    loader = os.open("/sys/kernel/security/apparmor/.load", os.O_WRONLY)
    try:
        os.write(loader, b"not a policy")
    except OSError as error:
        if error.errno in DENIALS:
            raise
        return f"passed AppArmor's policy-admin check ({errno.errorcode[error.errno]})"
    finally:
        os.close(loader)
    return "loaded an AppArmor policy"


def mknod(directory):
    path = os.path.join(directory, "null")
    os.mknod(path, 0o20600, os.makedev(1, 3))
    os.unlink(path)
    return "made a character device node"


def perfmon():
    attributes = ctypes.create_string_buffer(struct.pack("=II", 1, 128), 128)  # software
    os.close(syscall("perf_event_open", attributes, -1, 0, -1, 0))
    return "opened a CPU-wide perf event"


def syslog():
    with open("/proc/sys/kernel/printk") as printk:
        level = int(printk.read().split()[0])
    check(LIBC.klogctl(8, None, level))  # SYSLOG_ACTION_CONSOLE_LEVEL
    return "set the console log level"


def sys_admin():
    socket.sethostname(socket.gethostname())
    return "set the hostname"


def sys_boot():
    try:
        check(LIBC.reboot(0))  # LINUX_REBOOT_CMD_CAD_OFF
    except OSError as error:
        # In a child pid namespace only restart and halt mean anything (they
        # end the namespace); EINVAL for anything else comes after the
        # capability check passed.
        if error.errno == errno.EINVAL:
            return "passed reboot's capability check (EINVAL for CAD_OFF)"
        raise
    return "turned ctrl-alt-del off"


def sys_module():
    try:
        syscall("init_module", b"", 0, b"")
    except OSError as error:
        if error.errno in DENIALS:
            raise
        return f"passed init_module's capability check ({errno.errorcode[error.errno]})"
    return "loaded an empty module"


def sys_nice():
    os.setpriority(os.PRIO_PROCESS, 0, -1)
    return "lowered its nice value below zero"


def sys_pacct():
    check(LIBC.acct(None))
    return "switched process accounting off"


def sys_ptrace():
    ready_read, ready_write = os.pipe()
    child = os.fork()
    if child == 0:
        try:
            os.close(ready_read)
            os.setgid(65534)
            os.setuid(65534)
            os.write(ready_write, b"x")
            time.sleep(30)
        finally:
            os._exit(0)
    os.close(ready_write)
    try:
        if os.read(ready_read, 1) != b"x":
            raise RuntimeError("the child couldn't become uid 65534")
        LIBC.ptrace.restype = ctypes.c_long
        check(LIBC.ptrace(ctypes.c_long(0x4206), ctypes.c_long(child), None, None))
    finally:
        os.kill(child, 9)
        os.waitpid(child, 0)
    return "attached to another user's process"


def sys_rawio(file):
    import fcntl
    descriptor = os.open(file, os.O_RDONLY)
    try:
        fcntl.ioctl(descriptor, 1, struct.pack("=i", 0))  # FIBMAP
    except OSError as error:
        if error.errno in DENIALS:
            raise
        return f"passed FIBMAP's capability check ({errno.errorcode[error.errno]})"
    finally:
        os.close(descriptor)
    return "mapped a file block with FIBMAP"


def sys_resource():
    soft, hard = resource.getrlimit(resource.RLIMIT_NOFILE)
    resource.setrlimit(resource.RLIMIT_NOFILE, (min(soft, hard - 1), hard - 1))
    try:
        resource.setrlimit(resource.RLIMIT_NOFILE, (soft, hard))
    except ValueError as error:  # Python's EPERM here
        raise Denied(f"EPERM: {error}")
    return "raised its hard RLIMIT_NOFILE"


def sys_time():
    time.clock_settime(time.CLOCK_REALTIME, time.clock_gettime(time.CLOCK_REALTIME))
    return "set the clock"


def sys_tty_config():
    try:
        os.setsid()  # no controlling terminal, so vhangup hangs nothing up
    except OSError:
        pass
    check(LIBC.vhangup())
    return "called vhangup"


def wake_alarm():
    os.close(check(LIBC.timerfd_create(8, 0)))  # CLOCK_REALTIME_ALARM
    return "created an alarm timer"


def probe(capability, file, directory):
    probes = {
        "CAP_AUDIT_CONTROL": audit,
        "CAP_AUDIT_READ": lambda: audit(groups=1),
        "CAP_AUDIT_WRITE": audit,
        "CAP_BLOCK_SUSPEND": block_suspend,
        "CAP_BPF": bpf,
        "CAP_CHECKPOINT_RESTORE": checkpoint_restore,
        "CAP_DAC_READ_SEARCH": lambda: dac_read_search(file),
        "CAP_IPC_LOCK": ipc_lock,
        "CAP_IPC_OWNER": ipc_owner,
        "CAP_LEASE": lambda: lease(file),
        "CAP_LINUX_IMMUTABLE": lambda: linux_immutable(file),
        "CAP_MAC_ADMIN": mac_admin,
        "CAP_MKNOD": lambda: mknod(directory),
        "CAP_PERFMON": perfmon,
        "CAP_SYSLOG": syslog,
        "CAP_SYS_ADMIN": sys_admin,
        "CAP_SYS_BOOT": sys_boot,
        "CAP_SYS_MODULE": sys_module,
        "CAP_SYS_NICE": sys_nice,
        "CAP_SYS_PACCT": sys_pacct,
        "CAP_SYS_PTRACE": sys_ptrace,
        "CAP_SYS_RAWIO": lambda: sys_rawio(file),
        "CAP_SYS_RESOURCE": sys_resource,
        "CAP_SYS_TIME": sys_time,
        "CAP_SYS_TTY_CONFIG": sys_tty_config,
        "CAP_WAKE_ALARM": wake_alarm,
    }
    try:
        return "allowed: " + probes[capability]()
    except Denied as reason:
        return f"denied: {reason}"
    except OSError as error:
        if error.errno in DENIALS:
            return f"denied: {errno.errorcode[error.errno]}"
        raise


def main():
    arguments = sys.argv[1:]
    capability = arguments[0]
    option = lambda name: arguments[arguments.index(name) + 1] if name in arguments else ""
    if "--drop" in arguments:
        drop(capability)
    print(probe(capability, option("--file"), option("--dir")))


if __name__ == "__main__":
    main()
