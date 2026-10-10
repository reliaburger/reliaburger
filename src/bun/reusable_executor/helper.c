/* Single-threaded container PID 1. Bun embeds this static executable.
 * All scheduling, authorisation, durable outcomes and ownership stay in Rust.
 * A host-fixture build exercises framing, never qualifies Linux isolation. */
#define _GNU_SOURCE
#include <arpa/inet.h>
#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/types.h>
#include <sys/un.h>
#include <sys/wait.h>
#include <unistd.h>
#if !defined(RB_EXECUTOR_HOST_FIXTURE) || defined(RB_EXECUTOR_HOST)
#include <grp.h>
#include <linux/capability.h>
#include <linux/sched.h>
#include <linux/securebits.h>
#include <sched.h>
#include <sys/mount.h>
#include <sys/prctl.h>
#include <sys/resource.h>
#include <sys/syscall.h>
#endif

#define FRAME_LIMIT 65536U
#define STRING_LIMIT 256U
#define DRAIN_AFTER_EXIT_LIMIT (16U << 20)
#ifdef RB_EXECUTOR_HOST
/* Match the protected host uid used by container helpers' user mapping. */
#define HELPER_UID 2100000000U
#else
#define HELPER_UID 65536U
#endif
static unsigned char frame[FRAME_LIMIT];
extern char **environ;
static int child_events[2];
static void child_changed(int signal_number) {
    (void)signal_number;
    int saved = errno;
    unsigned char marker = 1;
    ssize_t notified = write(child_events[1], &marker, 1);
    (void)notified;
    errno = saved;
}

static int all(int fd, void *buffer, size_t count, int writing) {
    unsigned char *at = buffer;
    while (count) {
        ssize_t result = writing ? write(fd, at, count) : read(fd, at, count);
        if (result < 0 && errno == EINTR) continue;
        if (result <= 0) return -1;
        at += result;
        count -= (size_t)result;
    }
    return 0;
}
static uint64_t decode64(const unsigned char *bytes) {
    uint64_t value = 0;
    for (unsigned i = 0; i < 8; ++i) value = (value << 8) | bytes[i];
    return value;
}
static int word(int fd, uint32_t *value) {
    uint32_t network;
    if (all(fd, &network, 4, 0)) return -1;
    *value = ntohl(network);
    return 0;
}
static int event(int fd, unsigned char kind, uint64_t sequence) {
    unsigned char header[9] = {kind};
    for (unsigned i = 0; i < 8; ++i) header[8 - i] = (unsigned char)(sequence >> (i * 8));
    return all(fd, header, sizeof(header), 1);
}
static int send_word(int fd, uint32_t value) {
    value = htonl(value);
    return all(fd, &value, 4, 1);
}
static int take32(size_t length, size_t *cursor, uint32_t *value) {
    if (*cursor > length || length - *cursor < 4) return -1;
    uint32_t network;
    memcpy(&network, frame + *cursor, 4);
    *value = ntohl(network);
    *cursor += 4;
    return 0;
}
/* Compact strings into already-consumed bytes. Every length prefix frees four
 * bytes, so the trailing NUL never overwrites a subsequent unread field. */
static char *string(size_t length, size_t *cursor, size_t *destination) {
    uint32_t size;
    if (take32(length, cursor, &size) || size > length - *cursor ||
        memchr(frame + *cursor, 0, size)) return NULL;
    char *value = (char *)frame + *destination;
    memmove(value, frame + *cursor, size);
    value[size] = 0;
    *cursor += size;
    *destination += (size_t)size + 1;
    return value;
}
static void erase(size_t length) {
    volatile unsigned char *bytes = frame;
    while (length--) *bytes++ = 0;
}
static int receive_directory(int fd) {
    unsigned char marker;
    struct iovec io = {.iov_base = &marker, .iov_len = 1};
    unsigned char ancillary[CMSG_SPACE(sizeof(int))];
    memset(ancillary, 0, sizeof(ancillary));
    struct msghdr message = {.msg_iov = &io, .msg_iovlen = 1,
        .msg_control = ancillary, .msg_controllen = sizeof(ancillary)};
    if (recvmsg(fd, &message, 0) != 1 || marker != 'F' ||
        message.msg_flags & (MSG_CTRUNC | MSG_TRUNC)) return -1;
    struct cmsghdr *control = CMSG_FIRSTHDR(&message);
    if (!control || control->cmsg_level != SOL_SOCKET || control->cmsg_type != SCM_RIGHTS ||
        control->cmsg_len != CMSG_LEN(sizeof(int)) || CMSG_NXTHDR(&message, control)) return -1;
    int directory;
    memcpy(&directory, CMSG_DATA(control), sizeof(directory));
    if (fcntl(directory, F_SETFD, FD_CLOEXEC)) { close(directory); return -1; }
    return directory;
}
static int pipes(int descriptors[2]) {
    if (pipe(descriptors)) return -1;
    if (fcntl(descriptors[0], F_SETFD, FD_CLOEXEC) ||
        fcntl(descriptors[1], F_SETFD, FD_CLOEXEC) ||
        fcntl(descriptors[0], F_SETFL, O_NONBLOCK)) {
        close(descriptors[0]); close(descriptors[1]); return -1;
    }
    return 0;
}
#ifndef RB_EXECUTOR_HOST_FIXTURE
static void child_setup(uint32_t uid, uint32_t gid) {
#ifdef RB_EXECUTOR_HOST
    /* Host commands keep the existing Bun-user privilege contract. They are
     * allowlisted trusted processes, not an OCI security boundary. */
    if (setgroups(0, NULL) || setgid(gid) || setuid(uid) || prctl(PR_SET_PDEATHSIG, SIGKILL)) {
        perror("host task credentials"); _exit(126);
    }
#else
    /* These capabilities exist only in this container's user namespace. A
     * command never keeps them, the helper's descriptors or delegation uid. */
#ifndef RB_EXECUTOR_HOST
    if (unshare(CLONE_NEWNS | CLONE_NEWIPC) || mount(NULL, "/", NULL, MS_REC | MS_PRIVATE, NULL) ||
        mount("tmpfs", "/tmp", "tmpfs", MS_NOSUID | MS_NODEV, "mode=1777,size=16m") ||
        mount("tmpfs", "/dev/shm", "tmpfs", MS_NOSUID | MS_NODEV | MS_NOEXEC, "mode=1777,size=16m") ||
        mount(NULL, "/dev", NULL, MS_REMOUNT | MS_BIND | MS_RDONLY | MS_NOSUID | MS_NOEXEC, NULL)) {
        perror("executor private scratch"); _exit(126);
    }
#endif
    struct rlimit file_size = {.rlim_cur = 1U << 20, .rlim_max = 1U << 20};
    if (setrlimit(RLIMIT_FSIZE, &file_size) || setgroups(0, NULL) ||
        prctl(PR_SET_SECUREBITS, SECBIT_NOROOT | SECBIT_NOROOT_LOCKED |
              SECBIT_NO_SETUID_FIXUP | SECBIT_NO_SETUID_FIXUP_LOCKED) ||
        prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0)) {
        perror("executor task security"); _exit(126);
    }
    for (unsigned capability = 0; capability <= CAP_LAST_CAP; ++capability) {
        if (prctl(PR_CAPBSET_DROP, capability, 0, 0, 0) && errno != EINVAL) {
            perror("executor capability retirement"); _exit(126);
        }
    }
    if (setgid(gid) || setuid(uid)) { perror("executor task credentials"); _exit(126); }
    struct __user_cap_header_struct header = {.version = _LINUX_CAPABILITY_VERSION_3};
    struct __user_cap_data_struct capabilities[2] = {{0}, {0}};
    if (syscall(SYS_capset, &header, capabilities) || prctl(PR_SET_PDEATHSIG, SIGKILL)) {
        perror("executor task capabilities"); _exit(126);
    }
#endif
}
#endif
static pid_t launch(int directory) {
#ifdef RB_EXECUTOR_HOST_FIXTURE
    (void)directory;
    return fork();
#else
    struct clone_args arguments = {.flags = CLONE_INTO_CGROUP, .exit_signal = SIGCHLD,
                                   .cgroup = (uint64_t)directory};
    return (pid_t)syscall(SYS_clone3, &arguments, sizeof(arguments));
#endif
}
#ifdef RB_EXECUTOR_HOST
/* This single-threaded subreaper never reaps a child between reading its owned
 * children list and opening its pidfd. Even a dead child retains its identity.
 * Descendants are adopted when their parents exit; repeat until ECHILD. */
static int retire_owned_children(void) {
    for (;;) {
        char path[96];
        snprintf(path, sizeof(path), "/proc/self/task/%ld/children", (long)getpid());
        FILE *children = fopen(path, "r");
        if (!children) return -1;
        long child;
        while (fscanf(children, "%ld", &child) == 1) {
            int pidfd = (int)syscall(SYS_pidfd_open, child, 0);
            if (pidfd < 0) { if (errno == ESRCH) continue; fclose(children); return -1; }
            int result = (int)syscall(SYS_pidfd_send_signal, pidfd, SIGKILL, NULL, 0);
            int saved = errno;
            close(pidfd);
            if (result && saved != ESRCH) { fclose(children); errno = saved; return -1; }
        }
        fclose(children);
        int status;
        pid_t waited;
        do { waited = waitpid(-1, &status, WNOHANG); } while (waited > 0 || (waited < 0 && errno == EINTR));
        if (waited < 0) return errno == ECHILD ? 0 : -1;
        struct pollfd notification = {child_events[0], POLLIN, 0};
        if (poll(&notification, 1, -1) < 0 && errno != EINTR) return -1;
        unsigned char markers[128];
        while (read(child_events[0], markers, sizeof(markers)) > 0) {}
    }
}
#endif
static int cleanup(int fd, uint64_t sequence) {
    unsigned char receipt[9];
    if (all(fd, receipt, sizeof(receipt), 0) || receipt[0] != 'C' ||
        decode64(receipt + 1) != sequence) return -1;
    /* Only production PID 1 calls kill-all: the kernel confines signalling to
     * this PID namespace, excluding PID 1 itself. Its namespace-local CAP_KILL
     * covers descendant users. Never use this in the host protocol fixture. */
#ifdef RB_EXECUTOR_HOST
    if (retire_owned_children()) return -1;
#elif !defined(RB_EXECUTOR_HOST_FIXTURE)
    if (kill(-1, SIGKILL) && errno != ESRCH) return -1;
#endif
    /* Reap every descendant before authorising another command. Bun checks
     * the task cgroup is empty after this receipt. Normal cleanup deliberately
     * avoids cgroup.kill: older kernels snapshot the source kill_seq during
     * CLONE_INTO_CGROUP and spuriously SIGKILL the next child after a write. */
    int status;
    while (waitpid(-1, &status, 0) > 0 || errno == EINTR) errno = 0;
    if (errno != ECHILD) return -1;
    return event(fd, 4, sequence);
}
static int run(int control, int directory, uint64_t sequence, uint32_t argc,
               char **argv, char **env, char *cwd, uint32_t uid, uint32_t gid) {
    int outputs[2][2];
    if (pipes(outputs[0])) return -1;
    if (pipes(outputs[1])) { close(outputs[0][0]); close(outputs[0][1]); return -1; }
    pid_t child = launch(directory);
    if (child < 0) {
        int error = errno;
        for (unsigned stream = 0; stream < 2; ++stream) {
            close(outputs[stream][0]); close(outputs[stream][1]);
        }
        if (event(control, 5, sequence) || send_word(control, (uint32_t)error)) return -1;
        return cleanup(control, sequence);
    }
    if (!child) {
        close(control); close(directory);
        close(child_events[0]); close(child_events[1]);
        int input = open("/dev/null", O_RDONLY);
        if (input < 0 || dup2(input, STDIN_FILENO) < 0 ||
            dup2(outputs[0][1], STDOUT_FILENO) < 0 || dup2(outputs[1][1], STDERR_FILENO) < 0) _exit(126);
        if (input > STDERR_FILENO) close(input);
        for (unsigned stream = 0; stream < 2; ++stream) {
            close(outputs[stream][0]); close(outputs[stream][1]);
        }
#ifndef RB_EXECUTOR_HOST_FIXTURE
        child_setup(uid, gid);
#else
        (void)uid; (void)gid;
#endif
        environ = env;
        if (chdir(cwd)) { perror("executor working directory"); _exit(126); }
        if (!argc) _exit(126);
        execvp(argv[0], argv);
        perror("executor command"); _exit(127);
    }
    for (unsigned stream = 0; stream < 2; ++stream) close(outputs[stream][1]);
    int status = 0;
    int failed = event(control, 1, sequence) != 0;
    int exited = 0;
    /* After exit, drain to EOF: a command may enlarge its pipe (F_SETPIPE_SZ)
     * and exit with far more than one pass buffered. EOF is immediate unless
     * a descendant still holds the pipe; then stop once it stays silent for
     * 10 ms, or at a cap, saying so in the output rather than dropping bytes
     * silently. */
    size_t drained_after_exit = 0;
    int truncated = 0;
    for (;;) {
        if (failed) break;
        /* Both streams already at EOF when the exit is seen is the normal
         * case: stop now rather than wait out another poll. */
        if (exited && outputs[0][0] < 0 && outputs[1][0] < 0) break;
        struct pollfd observed[4] = {{control, 0, 0},
            {outputs[0][0], POLLIN, 0}, {outputs[1][0], POLLIN, 0},
            {child_events[0], POLLIN, 0}};
        int polled = poll(observed, 4, exited ? 10 : -1);
        if (polled < 0 && errno == EINTR) continue;
        if (polled < 0 || observed[0].revents & (POLLHUP | POLLERR | POLLNVAL)) { failed = 1; break; }
        unsigned char notifications[128];
        while (read(child_events[0], notifications, sizeof(notifications)) > 0) {}
        int progressed = 0;
        for (unsigned stream = 0; stream < 2; ++stream) {
            if (outputs[stream][0] < 0) continue;
            unsigned char bytes[4096];
            ssize_t size = -1;
            /* Bound a pass so a flooding descendant cannot delay timeout or
             * its parent's exit forever. Drain again after observing exit. */
            for (unsigned reads = 0; reads < 16; ++reads) {
                size = read(outputs[stream][0], bytes, sizeof(bytes));
                if (size <= 0) break;
                progressed = 1;
                if (exited) drained_after_exit += (size_t)size;
                unsigned char source = (unsigned char)stream + 1;
                if (event(control, 2, sequence) || all(control, &source, 1, 1) ||
                    send_word(control, (uint32_t)size) || all(control, bytes, (size_t)size, 1)) {
                    failed = 1; break;
                }
            }
            if (!size) { close(outputs[stream][0]); outputs[stream][0] = -1; progressed = 1; }
            else if (size < 0 && errno != EAGAIN && errno != EINTR) failed = 1;
        }
        if (failed) break;
        if (exited) {
            if (!progressed) break;
            if (drained_after_exit >= DRAIN_AFTER_EXIT_LIMIT) { truncated = 1; break; }
            continue;
        }
#ifdef RB_EXECUTOR_HOST
        siginfo_t information = {0};
        int waited = waitid(P_PID, (id_t)child, &information, WEXITED | WNOHANG | WNOWAIT);
        if (!waited && information.si_pid) {
            status = information.si_code == CLD_EXITED ? information.si_status << 8 : information.si_status;
            exited = 1;
        } else if (waited < 0 && errno != EINTR) { failed = 1; break; }
#else
        pid_t waited = waitpid(child, &status, WNOHANG);
        if (waited == child) exited = 1;
        else if (waited < 0 && errno != EINTR) { failed = 1; break; }
#endif
    }
    for (unsigned stream = 0; stream < 2; ++stream)
        if (outputs[stream][0] >= 0) close(outputs[stream][0]);
    if (truncated && !failed) {
        char marker[] = "\n[reliaburger: output written after exit truncated]\n";
        unsigned char source = 2;
        uint32_t length = sizeof(marker) - 1;
        failed = event(control, 2, sequence) || all(control, &source, 1, 1) ||
                 send_word(control, length) || all(control, marker, length, 1);
    }
    if (failed) {
#ifdef RB_EXECUTOR_HOST
        (void)retire_owned_children();
#elif defined(RB_EXECUTOR_HOST_FIXTURE)
        (void)kill(child, SIGKILL);
        while (waitpid(child, &status, 0) < 0 && errno == EINTR) {}
#endif
        return -1;
    }
    int32_t code = WIFEXITED(status) ? WEXITSTATUS(status) : -WTERMSIG(status);
    if (event(control, 3, sequence) || send_word(control, (uint32_t)code)) return -1;
    return cleanup(control, sequence);
}
int main(int argc, char **argv) {
#if defined(RB_EXECUTOR_HOST) && !defined(RB_EXECUTOR_HOST_FIXTURE)
    if (argc != 3) return 125;
    int placement = open(argv[2], O_WRONLY | O_CLOEXEC);
    char identity[32];
    int size = snprintf(identity, sizeof(identity), "%ld", (long)getpid());
    if (placement < 0 || all(placement, identity, (size_t)size, 1)) return 125;
    close(placement);
#else
    if (argc != 2) return 125;
#endif
#ifdef RB_EXECUTOR_HOST
    if (prctl(PR_SET_CHILD_SUBREAPER, 1)) return 125;
#endif
    if (strlen(argv[1]) >= sizeof(((struct sockaddr_un *)0)->sun_path)) return 125;
#ifndef RB_EXECUTOR_HOST_FIXTURE
#ifndef RB_EXECUTOR_HOST
    if (getpid() != 1) return 125;
#endif
    if (getuid() != 0 || getgid() != 0 ||
        prctl(PR_SET_KEEPCAPS, 1) || setgroups(0, NULL) ||
        setgid(HELPER_UID) || setuid(HELPER_UID)) return 125;
    struct __user_cap_header_struct header = {.version = _LINUX_CAPABILITY_VERSION_3};
    struct __user_cap_data_struct capabilities[2] = {{0}, {0}};
    uint32_t mask = (1U << CAP_KILL) | (1U << CAP_SETUID) | (1U << CAP_SETGID);
#ifndef RB_EXECUTOR_HOST
    /* Only the container child mounts its scratch and drops its bounding set.
     * On the host these would be real initial-namespace privileges, and
     * CLONE_INTO_CGROUP needs only the cgroup.procs files Bun chowned to us. */
    mask |= (1U << CAP_SETPCAP) | (1U << CAP_SYS_ADMIN);
#endif
    capabilities[0].effective = capabilities[0].permitted = mask;
    if (syscall(SYS_capset, &header, capabilities) || prctl(PR_SET_DUMPABLE, 0)) return 125;
#endif
    if (pipes(child_events) || fcntl(child_events[1], F_SETFL, O_NONBLOCK)) return 125;
    struct sigaction notification = {.sa_handler = child_changed, .sa_flags = SA_NOCLDSTOP};
    sigemptyset(&notification.sa_mask);
    if (sigaction(SIGCHLD, &notification, NULL)) return 125;
    int control = socket(AF_UNIX, SOCK_STREAM, 0);
    if (control < 0 || fcntl(control, F_SETFD, FD_CLOEXEC)) return 125;
    struct sockaddr_un address = {.sun_family = AF_UNIX};
    memcpy(address.sun_path, argv[1], strlen(argv[1]) + 1);
    if (connect(control, (struct sockaddr *)&address, sizeof(address)) ||
        all(control, "RBEX0001", 8, 1)) return 125;
    int directory = receive_directory(control);
    if (directory < 0) return 125;
    uint64_t previous = 0;
    for (;;) {
        uint32_t length;
        if (word(control, &length)) break;
        if (length < 24 || length > FRAME_LIMIT || all(control, frame, length, 0)) break;
        uint64_t sequence = decode64(frame);
        uint32_t arguments, variables, uid, gid;
        size_t cursor = 8, destination = 0;
        if (previous == UINT64_MAX || sequence != previous + 1 ||
            take32(length, &cursor, &arguments) || take32(length, &cursor, &variables) ||
            take32(length, &cursor, &uid) || take32(length, &cursor, &gid) ||
            !arguments || arguments > STRING_LIMIT || variables > STRING_LIMIT ||
            uid >= HELPER_UID || gid >= HELPER_UID) break;
        char *cwd = string(length, &cursor, &destination);
        if (!cwd || !*cwd) break;
        char *command[STRING_LIMIT + 1], *environment[STRING_LIMIT + 1];
        unsigned i;
        for (i = 0; i < arguments; ++i)
            if (!(command[i] = string(length, &cursor, &destination))) break;
        if (i != arguments || !*command[0]) break;
        command[arguments] = NULL;
        for (i = 0; i < variables; ++i) {
            if (!(environment[i] = string(length, &cursor, &destination))) break;
            char *equals = strchr(environment[i], '=');
            if (!equals || equals == environment[i]) break;
        }
        if (i != variables || cursor != length) break;
        environment[variables] = NULL;
        if (run(control, directory, sequence, arguments, command, environment, cwd, uid, gid)) break;
        previous = sequence;
        erase(length);
    }
    erase(sizeof(frame));
#ifdef RB_EXECUTOR_HOST
    if (retire_owned_children()) return 125;
#endif
    close(directory); close(control);
    return 125;
}
