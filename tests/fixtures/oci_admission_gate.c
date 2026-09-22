/* Qualification-only LD_PRELOAD shim. Pause Bun's blocking worker before it
 * opens the initialiser's intent lock. This leaves the actual Rust admission
 * path untouched and creates no runtime intent or resources on its behalf. */
#define _GNU_SOURCE
#include <dlfcn.h>
#include <fcntl.h>
#include <stdarg.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/syscall.h>
#include <unistd.h>

static void admission_gate(const char *path) {
    if (!strstr(path, "/bundles/.intents/locks/") || !strstr(path, "__init-0")) return;
    const char *root = getenv("OCI_CRASH_ROOT");
    if (!root) return;
    char name[4096], phase[32] = {0};
    if (snprintf(name, sizeof(name), "%s/phase", root) >= (int)sizeof(name)) _exit(91);
    int file = syscall(SYS_openat, AT_FDCWD, name, O_RDONLY, 0);
    if (file < 0) _exit(92);
    ssize_t count = read(file, phase, sizeof(phase)-1);
    close(file);
    if (count < 0) _exit(93);
    if (strcmp(phase, "admission")) return;
    snprintf(name, sizeof(name), "%s/armed", root);
    if (access(name, F_OK)) return;
    snprintf(name, sizeof(name), "%s/ready", root);
    file = syscall(SYS_openat, AT_FDCWD, name, O_WRONLY | O_CREAT | O_TRUNC, 0600);
    if (file < 0) _exit(94);
    close(file);
    snprintf(name, sizeof(name), "%s/release", root);
    for (int i = 0; i < 3000; ++i) {
        if (!access(name, F_OK)) return;
        usleep(20000);
    }
    _exit(95);
}

#define INTERPOSE(function) \
int function(const char *path, int flags, ...) { \
    mode_t mode = 0; \
    if ((flags & O_CREAT) || (flags & O_TMPFILE) == O_TMPFILE) { \
        va_list arguments; va_start(arguments, flags); mode = va_arg(arguments, mode_t); va_end(arguments); \
    } \
    int (*original)(const char *, int, ...) = (int (*)(const char *, int, ...))dlsym(RTLD_NEXT, #function); \
    if (!original) _exit(96); \
    admission_gate(path); \
    return original(path, flags, mode); \
}
INTERPOSE(open)
INTERPOSE(open64)
