/* Actual libc calls, including i386 old readlink and readlinkat ABIs. */
static int proc_probe(int argc, char **argv, char **environment) {
    char link[4096];
    memset(link, 0x5a, sizeof(link));
    ssize_t length = readlink("/proc/self/exe", link, sizeof(link));
    if (!check(argc == 1 && length == (ssize_t)strlen(argv[0]) &&
        !memcmp(link, argv[0], length) && link[length] == 0x5a, 190)) return 0;
    char short_link[4] = {0x5a, 0x5a, 0x5a, 0x5a};
    if (!check(readlink("/proc/self/exe", short_link, 2) == 2 &&
        !memcmp(short_link, argv[0], 2) && short_link[2] == 0x5a, 191)) return 0;
    if (!check(syscall(SYS_readlinkat, -1, "/proc/self/exe", link, sizeof(link)) == length, 192)) return 0;
    char numeric[80];
    snprintf(numeric, sizeof(numeric), "/proc/%ld/exe", (long)getpid());
    if (!check(readlink(numeric, link, sizeof(link)) == length &&
        !memcmp(link, argv[0], length), 193)) return 0;
    errno = 0;
    // musl's wrapper deliberately uses a one-byte temporary for a zero
    // count. Check the kernel ABI directly for its EINVAL requirement.
    if (!check(syscall(SYS_readlink, "/proc/self/exe", link, 0) == -1 && errno == EINVAL, 194)) return 0;
    errno = 0;
    if (!check(syscall(SYS_readlink, "/proc/self/exe", (void *)1, 32) == -1 && errno == EFAULT, 195)) return 0;
    unsigned char *readonly = mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (!check(readonly != MAP_FAILED && mprotect(readonly, 4096, PROT_READ) == 0, 196)) return 0;
    errno = 0;
    if (!check(readlink("/proc/self/exe", (char *)readonly, 32) == -1 && errno == EFAULT, 197)) return 0;
    errno = 0;
    if (!check(readlink("/usr/bin/does-not-exist", link, sizeof(link)) == -1 && errno == ENOENT, 198)) return 0;
    errno = 0;
    if (!check(readlink(argv[0], link, sizeof(link)) == -1 && errno == EINVAL, 199)) return 0;
    int executable = open("/proc/self/exe", O_RDONLY);
    unsigned char header[4];
    struct stat actual, followed, symbolic;
    if (!check(executable >= 3 && pread(executable, header, 4, 0) == 4 && !memcmp(header, "\177ELF", 4) &&
        fstat(executable, &actual) == 0 && stat(argv[0], &followed) == 0 &&
        actual.st_dev == followed.st_dev && actual.st_ino == followed.st_ino &&
        lstat("/proc/self/exe", &symbolic) == 0 && S_ISLNK(symbolic.st_mode) && symbolic.st_size == length, 200)) return 0;
    close(executable);
    DIR *directory = opendir("/usr/bin");
    if (!check(directory != NULL, 218)) return 0;
    errno = 0;
    if (!check(syscall(SYS_getdents64, dirfd(directory), readonly, 4096) == -1 && errno == EFAULT, 223)) return 0;
    int found = 0;
    const char *basename = strrchr(argv[0], '/') + 1;
    struct dirent *item;
    errno = 0;
    while ((item = readdir(directory)) != NULL) {
        if (!strcmp(item->d_name, basename)) {
            if (!check(item->d_ino == actual.st_ino && item->d_type == DT_REG, 219)) return 0;
            found++;
        }
    }
    if (!check(found == 1 && errno == 0 && closedir(directory) == 0, 220)) return 0;
    puts(PROBE_NAME ": PASS real directory enumeration and executable inode");
    errno = 0;
    if (!check(open("/proc/self/exe", O_RDONLY | O_NOFOLLOW) == -1 && errno == ELOOP, 201)) return 0;
    int arguments = open("/proc/self/cmdline", O_RDONLY | O_CLOEXEC);
    int duplicate = dup(arguments);
    char bytes[4096];
    struct stat metadata;
    struct statx extended;
    if (!check(arguments >= 3 && duplicate >= 3 && fstat(arguments, &metadata) == 0 &&
        S_ISREG(metadata.st_mode) && metadata.st_size == 0 &&
        syscall(SYS_statx, arguments, "", AT_EMPTY_PATH, STATX_BASIC_STATS, &extended) == 0 &&
        extended.stx_ino == metadata.st_ino && extended.stx_dev_minor == minor(metadata.st_dev) &&
        fcntl(arguments, F_GETFD) == FD_CLOEXEC && fcntl(duplicate, F_GETFD) == 0 &&
        (fcntl(arguments, F_GETFL) & O_ACCMODE) == O_RDONLY, 202)) return 0;
    errno = 0;
    if (!check(syscall(SYS_statx, arguments, "", AT_EMPTY_PATH, STATX_BASIC_STATS, readonly) == -1 && errno == EFAULT, 221)) return 0;
    errno = 0;
    if (!check(open("/proc/self/cmdline", O_RDWR) == -1 && errno == EACCES, 222)) return 0;
    if (!check(read(arguments, bytes, 2) == 2 && !memcmp(bytes, argv[0], 2) &&
        read(duplicate, bytes, 3) == 3 && !memcmp(bytes, argv[0] + 2, 3) &&
        lseek(arguments, 0, SEEK_CUR) == 5 && pread(arguments, bytes, length + 1, 0) == length + 1 &&
        !memcmp(bytes, argv[0], length + 1) && lseek(arguments, 0, SEEK_CUR) == 5, 203)) return 0;
    if (!check(fcntl(duplicate, F_SETFL, O_NONBLOCK) == 0 && (fcntl(arguments, F_GETFL) & O_NONBLOCK), 204)) return 0;
    char original = argv[0][0];
    argv[0][0] = 'X';
    if (!check(pread(arguments, bytes, length + 1, 0) == length + 1 && bytes[0] == 'X' &&
        readlink("/proc/self/exe", link, sizeof(link)) == length && link[0] == original, 205)) return 0;
    argv[0][0] = original;
    // Changing argv must not change the actual executable's identity.
    if (!check(readlink("/proc/self/exe", link, sizeof(link)) == length && !memcmp(link, argv[0], length), 206)) return 0;
    errno = 0;
    if (!check(read(arguments, readonly, 16) == -1 && errno == EFAULT && lseek(arguments, 0, SEEK_CUR) == 5, 207)) return 0;
    if (!check(lseek(arguments, 0, SEEK_SET) == 0 && read(arguments, bytes, sizeof(bytes)) == length + 1 &&
        bytes[length] == 0 && read(arguments, bytes, 1) == 0 &&
        syscall(SYS_read, arguments, (void *)1, 1) == 0, 208)) return 0;
    errno = 0;
    if (!check(write(arguments, "x", 1) == -1 && errno == EBADF, 209)) return 0;
    close(arguments);
    if (!check(lseek(duplicate, 0, SEEK_SET) == 0 && read(duplicate, bytes, 1) == 1 && bytes[0] == original, 210)) return 0;
    close(duplicate);
    int variables = open("/proc/self/environ", O_RDONLY);
    char expected[4096];
    size_t environment_length = 0;
    for (char **variable = environment; *variable; variable++) {
        size_t size = strlen(*variable) + 1;
        if (environment_length + size > sizeof(expected)) return 0;
        memcpy(expected + environment_length, *variable, size);
        environment_length += size;
    }
    if (!check(variables >= 3 && read(variables, bytes, sizeof(bytes)) == (ssize_t)environment_length &&
        !memcmp(bytes, expected, environment_length), 211)) return 0;
    if (environment[0]) {
        char original_environment = environment[0][0];
        environment[0][0] = 'Y';
        if (!check(pread(variables, bytes, 1, 0) == 1 && bytes[0] == 'Y', 212)) return 0;
        environment[0][0] = original_environment;
    }
    close(variables);
    munmap(readonly, 4096);
    puts(PROBE_NAME ": PASS executable identity live proc arguments/environment and shared cursor");
    return 1;
}

#ifndef __i386__
static int proc_exec_verify(int argc, char **argv, char **environment) {
    if (!check(argc == 5 && !strcmp(argv[0], "different-title") &&
        !strcmp(environment[0], "PROC_TEST=actual") && !strcmp(environment[1], "HOME=/root") &&
        environment[2] == NULL, 213)) return 213;
    int fd = atoi(argv[2]);
    char bytes[4096], expected[4096], path[4096];
    if (!check(getcwd(path, sizeof(path)) && !strcmp(path, argv[4]), 218)) return 218;
    size_t length = 0;
    for (int i = 0; i < argc; i++) {
        size_t size = strlen(argv[i]) + 1;
        memcpy(expected + length, argv[i], size);
        length += size;
    }
    // This descriptor was opened before execve. Its process view must refer
    // to the new loader ranges, not to strings copied from the old image.
    if (!check(pread(fd, bytes, sizeof(bytes), 0) == (ssize_t)length &&
        !memcmp(bytes, expected, length) && readlink("/proc/self/exe", path, sizeof(path)) == (ssize_t)strlen(argv[3]) &&
        !memcmp(path, argv[3], strlen(argv[3])), 214)) return 214;
    close(fd);
    fd = open("/proc/self/environ", O_RDONLY);
    const char expected_environment[] = "PROC_TEST=actual\0HOME=/root\0";
    if (!check(fd >= 3 && read(fd, bytes, sizeof(bytes)) == sizeof(expected_environment) - 1 &&
        !memcmp(bytes, expected_environment, sizeof(expected_environment) - 1), 215)) return 215;
    close(fd);
    puts(PROBE_NAME ": PASS execve refreshes image identity and inherited proc descriptor");
    puts(PROBE_NAME ": PASS relative execve retains cwd and actual executable identity");
    return 0;
}

static int proc_exec_start(const char *path) {
    (void)path;
    char directory[128], executable[256], bytes[4096];
    snprintf(directory, sizeof(directory), "/tmp/exec-cwd-%ld", (long)getpid());
    snprintf(executable, sizeof(executable), "%s/image", directory);
    int source = open("/proc/self/exe", O_RDONLY);
    if (!check(source >= 3 && mkdir(directory, 0700) == 0, 219)) return 219;
    int destination = open(executable, O_WRONLY | O_CREAT | O_EXCL, 0700);
    if (!check(destination >= 3, 220)) return 220;
    ssize_t count;
    while ((count = read(source, bytes, sizeof(bytes))) > 0) {
        if (!check(write(destination, bytes, (size_t)count) == count, 221)) return 221;
    }
    if (!check(count == 0 && close(source) == 0 && close(destination) == 0 && chdir(directory) == 0, 222)) return 222;
    int fd = open("/proc/self/cmdline", O_RDONLY);
    char descriptor[32];
    if (!check(fd >= 3, 216)) return 216;
    snprintf(descriptor, sizeof(descriptor), "%d", fd);
    char *arguments[] = {"different-title", "--proc-exec", descriptor, executable, directory, NULL};
    char *environment[] = {"PROC_TEST=actual", "HOME=/root", NULL};
    execve("./image", arguments, environment);
    check(0, 217);
    return 217;
}
#endif
