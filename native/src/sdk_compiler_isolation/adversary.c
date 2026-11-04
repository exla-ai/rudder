/* Real-kernel boundary probe, not an SDK/compiler or Root qualification. */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

static void require(int ok, const char *what) {
    if (!ok) { fprintf(stderr, "%s errno=%d\n", what, errno); exit(91); }
}
static void absent(const char *path) {
    errno = 0; int fd = open(path, O_RDONLY);
    require(fd < 0 && errno == ENOENT, path);
}
int main(int argc, char **argv) {
#if defined(__x86_64__)
    if (argc == 2 && strcmp(argv[1], "foreign-abi") == 0) {
        /* Harmless i386 getpid must be killed by AUDIT_ARCH refusal. */
        long value;
        __asm__ volatile("int $0x80" : "=a"(value) : "a"(20) : "memory");
        (void)value;
        return 92;
    }
#else
    (void)argc; (void)argv;
#endif
    char b[4096];
    require(read(0, b, 1) == 0, "stdin EOF survives actual exec");
    for (int fd = 3; fd < 1024; fd++) {
        errno = 0;
        require(fcntl(fd, F_GETFD) == -1 && errno == EBADF, "inherited descriptor");
    }
    errno = 0;
    require(fcntl(8192, F_GETFD) == -1 && errno == EBADF, "high observer proc alias");
    absent("/proc/self/fd/0"); absent("/proc/1/root/workspace/job.json");
    absent("/dev/fd/0"); absent("/observer-proc/self/fd/0");
    absent("/workspace/job.json"); absent("/excluded-secret");
    require(chdir("/workspace/src") == 0, "source cwd");
    absent("../../../../excluded-secret");
    int source = open("input", O_RDONLY);
    require(source >= 0 && read(source, b, sizeof(b)) == 15, "declared input");
    close(source);
    errno = 0;
    require(open("input", O_RDWR) == -1 && errno == EROFS, "source readonly");
    errno = 0;
    require(open("/workspace/forbidden", O_CREAT | O_WRONLY, 0600) == -1 &&
            errno == EROFS, "workspace parent readonly");
    errno = 0;
    require(open("/forbidden", O_CREAT | O_WRONLY, 0600) == -1 &&
            errno == EROFS, "SDK root readonly");
    errno = 0;
    require(link("input", "/workspace/out/hardlink") == -1 &&
            (errno == EXDEV || errno == EPERM || errno == EROFS), "source hardlink escape");
    for (int i = 0; i < 2; i++) {
        const char *path = i ? "/tmp/native" : "/workspace/out/native";
        int out = open(path, O_CREAT | O_WRONLY, 0700);
        require(out >= 0, "private writable output");
        int in = open("/probe", O_RDONLY);
        require(in >= 0, "readonly SDK executable");
        ssize_t size;
        while ((size = read(in, b, sizeof(b))) > 0)
            require(write(out, b, size) == size, "copy executable bytes");
        require(size == 0, "read executable bytes"); close(in); close(out);
        char *args[] = {(char *)path, NULL};
        errno = 0; execve(path, args, NULL);
        require(errno == EACCES, "writable native exec must fail");
        in = open(path, O_RDONLY); require(in >= 0, "output FD");
        errno = 0; syscall(SYS_execveat, in, "", args, NULL, AT_EMPTY_PATH);
        require(errno == EPERM, "FD native exec must fail");
        errno = 0;
        require(mmap(NULL, 4096, PROT_READ | PROT_EXEC, MAP_PRIVATE, in, 0) == MAP_FAILED &&
                errno == EPERM, "writable file executable mmap must fail");
        close(in);
    }
    require(symlink("/excluded-secret", "/tmp/excluded") == 0, "private symlink");
    absent("/tmp/excluded");
    require(symlink("/proc/self/fd/0", "/workspace/out/proc-fd") == 0, "proc symlink");
    absent("/workspace/out/proc-fd");
    errno = 0;
    require(syscall(SYS_memfd_create, "native", 0) == -1 && errno == EPERM, "memfd refused");
    errno = 0;
    require(syscall(SYS_io_uring_setup, 1, NULL) == -1 && errno == EPERM, "io_uring refused");
    errno = 0;
    require(syscall(SYS_ptrace, 0, 0, 0, 0) == -1 && errno == EPERM, "ptrace refused");
    errno = 0;
    require(syscall(SYS_process_vm_readv, 1, NULL, 0, NULL, 0, 0) == -1 && errno == EPERM,
            "observer process memory refused");
    puts("{\"actual_native_exec\":true,\"stdio_only\":true,\"excluded_paths\":true,"
         "\"readonly_source\":true,\"private_outputs\":true,\"noexec_file_and_fd\":true,"
         "\"symlink_and_hardlink_routes\":true,\"memfd_and_uring_denied\":true}");
    return 0;
}
