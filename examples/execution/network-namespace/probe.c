#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <inttypes.h>
#include <linux/capability.h>
#include <linux/if_tun.h>
#include <linux/nsfs.h>
#include <net/if.h>
#include <sched.h>
#include <signal.h>
#include <stdbool.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/prctl.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/sysmacros.h>
#include <sys/types.h>
#include <sys/utsname.h>
#include <sys/wait.h>
#include <unistd.h>

/* Qualification only. No connect(), bind(), listen(), setns(), persistent TUN,
 * host interface enumeration, credential reads, or external namespace target. */
static void require(bool condition, const char *stage) {
    if (!condition) {
        printf("{\"kind\":\"probe-failure\",\"stage\":\"%s\",\"errno\":%d}\n", stage, errno);
        exit(2);
    }
}

static void status(const char *stage) {
    FILE *input = fopen("/proc/self/status", "re");
    require(input != NULL, "self-status-open");
    char line[512];
    printf("{\"kind\":\"process-status\",\"stage\":\"%s\",\"uid\":%u,\"euid\":%u,\"gid\":%u,\"egid\":%u,\"fields\":{",
           stage, getuid(), geteuid(), getgid(), getegid());
    bool first = true;
    while (fgets(line, sizeof(line), input)) {
        const char *keys[] = {"CapEff", "CapPrm", "CapInh", "CapAmb", "CapBnd", "NoNewPrivs"};
        for (unsigned i = 0; i < sizeof(keys) / sizeof(keys[0]); i++) {
            size_t length = strlen(keys[i]);
            if (strncmp(line, keys[i], length) != 0 || line[length] != ':') continue;
            char *value = line + length + 1;
            while (*value == ' ' || *value == '\t') value++;
            value[strcspn(value, "\r\n")] = 0;
            require(strspn(value, "0123456789abcdefABCDEF") == strlen(value), "self-status-value");
            printf("%s\"%s\":\"%s\"", first ? "" : ",", keys[i], value);
            first = false;
        }
    }
    require(!ferror(input) && fclose(input) == 0, "self-status-close");
    puts("}}");
}

static int namespace_fd(const char *name, const char *stage, struct stat *identity) {
    char path[64];
    require(snprintf(path, sizeof(path), "/proc/self/ns/%s", name) < (int)sizeof(path), "namespace-path");
    int fd = open(path, O_RDONLY | O_CLOEXEC);
    require(fd >= 0 && fstat(fd, identity) == 0, "namespace-fd");
    int type = ioctl(fd, NS_GET_NSTYPE);
    require(type >= 0, "namespace-type");
    printf("{\"kind\":\"owned-namespace-fd\",\"stage\":\"%s\",\"namespace\":\"%s\",\"device\":\"%ju\",\"inode\":\"%ju\",\"type\":%d,\"cloexec\":true}\n",
           stage, name, (uintmax_t)identity->st_dev, (uintmax_t)identity->st_ino, type);
    return fd;
}

static bool same(const struct stat *left, const struct stat *right) {
    return left->st_dev == right->st_dev && left->st_ino == right->st_ino;
}

static void mapping(const char *path, const char *bytes) {
    int fd = open(path, O_WRONLY | O_CLOEXEC);
    require(fd >= 0, "mapping-open");
    size_t length = strlen(bytes);
    ssize_t written = write(fd, bytes, length);
    int saved = errno;
    require(close(fd) == 0, "mapping-close");
    errno = saved;
    require(written == (ssize_t)length, "mapping-write");
}

static int loopback_socket(void) {
    int fd = socket(AF_INET, SOCK_DGRAM | SOCK_CLOEXEC, 0);
    require(fd >= 0, "newnet-control-socket");
    return fd;
}

static short loopback_flags(int fd) {
    struct ifreq request = {0};
    strcpy(request.ifr_name, "lo");
    require(ioctl(fd, SIOCGIFFLAGS, &request) == 0, "newnet-loopback-flags");
    return request.ifr_flags;
}

static int set_loopback(int fd, short flags) {
    struct ifreq request = {0};
    strcpy(request.ifr_name, "lo");
    request.ifr_flags = flags;
    return ioctl(fd, SIOCSIFFLAGS, &request);
}

static int after_exec(void) {
    status("after-external-exec");
    struct stat net;
    int ns = namespace_fd("net", "after-external-exec", &net);
    int fd = loopback_socket();
    short flags = loopback_flags(fd);
    require((flags & IFF_UP) != 0, "after-exec-loopback-inherited-up");
    errno = 0;
    int result = set_loopback(fd, flags & ~IFF_UP);
    int saved = errno;
    printf("{\"kind\":\"after-exec-loopback-down\",\"result\":%d,\"errno\":%d,\"expectedEperm\":%s}\n",
           result, saved, result == -1 && saved == EPERM ? "true" : "false");
    require(close(fd) == 0 && close(ns) == 0, "after-exec-close");
    return result == -1 && saved == EPERM ? 0 : 3;
}

int main(int argc, char **argv) {
    setvbuf(stdout, NULL, _IOLBF, 0);
    alarm(20);
    require(getuid() == 1500 && geteuid() == 1500 && getgid() == 1500 && getegid() == 1500, "uid1500-required");
    if (argc == 2 && strcmp(argv[1], "--after-exec") == 0) return after_exec();
    require(argc == 1, "arguments");
    status("fresh-exec-before-userns");
    struct utsname kernel;
    require(uname(&kernel) == 0, "kernel");
    require(strspn(kernel.release, "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789.-_+") == strlen(kernel.release), "kernel-value");
    printf("{\"kind\":\"kernel\",\"release\":\"%s\"}\n", kernel.release);
    struct stat outer_user, outer_net, inner_user, inner_net;
    int outer_user_fd = namespace_fd("user", "before-userns", &outer_user);
    int outer_net_fd = namespace_fd("net", "before-userns", &outer_net);
    errno = 0;
    if (unshare(CLONE_NEWUSER) != 0) {
        printf("{\"kind\":\"qualification-result\",\"completed\":true,\"usernsAvailable\":false,\"newnetAvailable\":false,\"stage\":\"unshare-user\",\"errno\":%d}\n", errno);
        require(close(outer_user_fd) == 0 && close(outer_net_fd) == 0, "unsupported-close");
        return 0;
    }
    mapping("/proc/self/setgroups", "deny\n");
    mapping("/proc/self/uid_map", "1500 1500 1\n");
    mapping("/proc/self/gid_map", "1500 1500 1\n");
    require(getuid() == 1500 && getgid() == 1500, "same-uid-mapping");
    status("in-process-after-userns");
    int inner_user_fd = namespace_fd("user", "after-userns", &inner_user);
    require(!same(&outer_user, &inner_user), "fresh-userns-witness");
    errno = 0;
    if (unshare(CLONE_NEWNET) != 0) {
        printf("{\"kind\":\"qualification-result\",\"completed\":true,\"usernsAvailable\":true,\"newnetAvailable\":false,\"stage\":\"unshare-net\",\"errno\":%d}\n", errno);
        require(close(outer_user_fd) == 0 && close(outer_net_fd) == 0 && close(inner_user_fd) == 0, "unsupported-close");
        return 0;
    }
    int inner_net_fd = namespace_fd("net", "after-newnet", &inner_net);
    require(!same(&outer_net, &inner_net), "fresh-netns-witness");
    require(ioctl(inner_net_fd, NS_GET_NSTYPE) == CLONE_NEWNET, "owned-newnet-type");
    int owner_fd = ioctl(inner_net_fd, NS_GET_USERNS);
    struct stat owner;
    require(owner_fd >= 0 && fstat(owner_fd, &owner) == 0 && same(&owner, &inner_user), "newnet-owned-by-created-userns");
    require(fcntl(owner_fd, F_SETFD, FD_CLOEXEC) == 0, "owner-fd-cloexec");
    printf("{\"kind\":\"netns-owner-witness\",\"device\":\"%ju\",\"inode\":\"%ju\",\"matchesCreatedUserns\":true}\n",
           (uintmax_t)owner.st_dev, (uintmax_t)owner.st_ino);
    status("in-process-after-newnet");
    int control = loopback_socket();
    short original = loopback_flags(control);
    require(set_loopback(control, original | IFF_UP) == 0, "in-process-loopback-up");
    require((loopback_flags(control) & IFF_UP) != 0, "in-process-loopback-up-verified");
    puts("{\"kind\":\"in-process-loopback-up\",\"ok\":true,\"onlyOwnedNewnetModified\":true}");

    struct stat device;
    errno = 0;
    int stat_result = lstat("/dev/net/tun", &device);
    int stat_errno = stat_result < 0 ? errno : 0;
    if (stat_result == 0) {
        printf("{\"kind\":\"tun-device\",\"statResult\":0,\"errno\":0,\"character\":%s,\"uid\":%u,\"gid\":%u,\"mode\":\"%04o\",\"major\":%u,\"minor\":%u}\n",
               S_ISCHR(device.st_mode) ? "true" : "false", device.st_uid, device.st_gid, device.st_mode & 07777,
               major(device.st_rdev), minor(device.st_rdev));
        require(S_ISCHR(device.st_mode) && major(device.st_rdev) == 10 && minor(device.st_rdev) == 200, "safe-tun-shape");
    } else {
        printf("{\"kind\":\"tun-device\",\"statResult\":-1,\"errno\":%d}\n", stat_errno);
    }
    errno = 0;
    int tun_fd = open("/dev/net/tun", O_RDWR | O_CLOEXEC | O_NOFOLLOW);
    int open_errno = tun_fd < 0 ? errno : 0;
    int tap_result = -2, tap_errno = 0;
    bool tap_removed = false;
    printf("{\"kind\":\"tun-open\",\"result\":%d,\"errno\":%d}\n", tun_fd < 0 ? -1 : 0, open_errno);
    if (tun_fd >= 0) {
        require(stat_result == 0, "tun-open-matches-stat");
        struct stat opened;
        require(fstat(tun_fd, &opened) == 0 && same(&opened, &device), "tun-open-identity");
        struct ifreq request = {0};
        strcpy(request.ifr_name, "rnetprobe0");
        request.ifr_flags = IFF_TAP | IFF_NO_PI;
        require(if_nametoindex(request.ifr_name) == 0, "private-tap-name-fresh");
        errno = 0;
        tap_result = ioctl(tun_fd, TUNSETIFF, &request);
        tap_errno = tap_result < 0 ? errno : 0;
        if (tap_result == 0) require(if_nametoindex(request.ifr_name) > 0, "private-tap-created");
        require(close(tun_fd) == 0, "private-tap-close");
        tap_removed = if_nametoindex("rnetprobe0") == 0;
        require(tap_removed, "private-tap-removed-after-close");
    }
    printf("{\"kind\":\"private-tap\",\"attempted\":%s,\"result\":%d,\"errno\":%d,\"removedAfterClose\":%s,\"persistRequested\":false}\n",
           tun_fd >= 0 ? "true" : "false", tap_result, tap_errno, tap_removed ? "true" : "false");

    pid_t child = fork();
    require(child >= 0, "exec-comparison-fork");
    if (child == 0) {
        execl(argv[0], argv[0], "--after-exec", (char *)NULL);
        _exit(126);
    }
    int child_status;
    require(waitpid(child, &child_status, 0) == child && WIFEXITED(child_status) && WEXITSTATUS(child_status) == 0,
            "external-exec-capability-comparison");
    require((loopback_flags(control) & IFF_UP) != 0, "after-exec-parent-loopback-still-up");
    require(set_loopback(control, original) == 0, "restore-owned-loopback");
    require((loopback_flags(control) & IFF_UP) == (original & IFF_UP), "restored-owned-loopback");
    struct __user_cap_header_struct header = {_LINUX_CAPABILITY_VERSION_3, 0};
    struct __user_cap_data_struct capabilities[2] = {{0}, {0}};
    require(prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) == 0 && syscall(SYS_capset, &header, capabilities) == 0,
            "drop-local-capabilities");
    status("in-process-after-explicit-cap-drop");
    errno = 0;
    int dropped = set_loopback(control, original | IFF_UP);
    int dropped_errno = errno;
    printf("{\"kind\":\"after-cap-drop-loopback-up\",\"result\":%d,\"errno\":%d,\"expectedEperm\":%s}\n",
           dropped, dropped_errno, dropped == -1 && dropped_errno == EPERM ? "true" : "false");
    require(dropped == -1 && dropped_errno == EPERM, "explicit-cap-drop-enforced");
    require(close(control) == 0 && close(owner_fd) == 0 && close(inner_net_fd) == 0 && close(inner_user_fd) == 0
            && close(outer_net_fd) == 0 && close(outer_user_fd) == 0, "close-owned-witnesses");
    printf("{\"kind\":\"qualification-result\",\"completed\":true,\"usernsAvailable\":true,\"newnetAvailable\":true,\"loopbackInProcess\":true,\"afterExecEperm\":true,\"afterCapDropEperm\":true,\"tunStatErrno\":%d,\"tunOpenErrno\":%d,\"privateTapCreated\":%s,\"privateTapRemoved\":%s,\"ownedFdsClosed\":true,\"loopbackRestored\":true}\n",
           stat_errno, open_errno, tap_result == 0 ? "true" : "false", tap_removed ? "true" : "false");
    return 0;
}
