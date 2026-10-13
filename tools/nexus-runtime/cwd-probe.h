/* libc-linked checks of native directory identity, not a simulated filesystem. */
#ifdef PROBE_THREADS
static void *cwd_worker(void *argument) {
    return chdir(argument) == 0 ? argument : NULL;
}
#endif

static int cwd_probe(void) {
    char base[128], moved[128], expected[256], output[4096];
    snprintf(base, sizeof(base), "/tmp/cwd-%ld", (long)getpid());
    snprintf(moved, sizeof(moved), "/tmp/cwd-moved-%ld", (long)getpid());
    if (!check(mkdir(base, 0700) == 0 && chdir(base) == 0 &&
        mkdir("a", 0700) == 0 && mkdir("a/b/", 0700) == 0 && chdir("a//./b") == 0, 250)) return 0;
    snprintf(expected, sizeof(expected), "%s/a/b", base);
    if (!check(getcwd(output, sizeof(output)) && !strcmp(output, expected) &&
        syscall(SYS_getcwd, output, sizeof(output)) == (long)strlen(expected) + 1, 251)) return 0;
    errno = 0;
    if (!check(syscall(SYS_getcwd, output, 1) == -1 && errno == ERANGE, 252)) return 0;
    errno = 0;
    if (!check(syscall(SYS_getcwd, output, 0) == -1 && errno == EINVAL, 253)) return 0;
    void *readonly = mmap(NULL, 4096, PROT_READ, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    errno = 0;
    if (!check(readonly != MAP_FAILED && syscall(SYS_getcwd, readonly, 4096) == -1 && errno == EFAULT, 254)) return 0;
    munmap(readonly, 4096);
    errno = 0;
    if (!check(syscall(SYS_chdir, (void *)1) == -1 && errno == EFAULT, 255)) return 0;
    errno = 0;
    if (!check(chdir("") == -1 && errno == ENOENT, 256)) return 0;
    errno = 0;
    if (!check(chdir("missing/..") == -1 && errno == ENOENT, 257)) return 0;
    int file = open("value", O_RDWR | O_CREAT | O_EXCL, 0600);
    struct stat actual, byname;
    if (!check(file >= 3 && write(file, "native-cwd", 10) == 10 && fstat(file, &actual) == 0 &&
        stat("./value", &byname) == 0 && actual.st_ino == byname.st_ino, 258)) return 0;
    errno = 0;
    if (!check(chdir("value/..") == -1 && errno == ENOTDIR, 259)) return 0;
    errno = 0;
    if (!check(fchdir(file) == -1 && errno == ENOTDIR, 260)) return 0;
    int directory = open(".", O_RDONLY | O_DIRECTORY);
    if (!check(directory >= 3 && fstat(directory, &actual) == 0, 261)) return 0;
    if (!check(fstatat(AT_FDCWD, "", &byname, AT_EMPTY_PATH) == 0 && actual.st_ino == byname.st_ino, 285)) return 0;
    errno = 0;
    if (!check(fchdir(AT_FDCWD) == -1 && errno == EBADF, 286)) return 0;
    errno = 0;
    if (!check(open("invalid-dir", O_CREAT | O_DIRECTORY | O_RDONLY, 0700) == -1 && errno == EINVAL, 287)) return 0;
    unsigned char sx[256] = {0};
    uint64_t inode;
    if (!check(syscall(SYS_statx, AT_FDCWD, "", AT_EMPTY_PATH, 0x7ff, sx) == 0, 262)) return 0;
    memcpy(&inode, sx + 32, sizeof(inode));
    if (!check(inode == (uint64_t)actual.st_ino, 263)) return 0;
    errno = 0;
    if (!check(openat(999999, "value", O_RDONLY) == -1 && errno == EBADF, 264)) return 0;
    errno = 0;
    if (!check(openat(file, "value", O_RDONLY) == -1 && errno == ENOTDIR, 265)) return 0;
    errno = 0;
    if (!check(readlink("proc/self/exe", output, sizeof(output)) == -1 && errno == ENOENT, 266)) return 0;
    if (!check(readlink("/proc/self/exe", output, sizeof(output)) > 0 && chdir("../../../../../..") == 0 &&
        getcwd(output, sizeof(output)) && !strcmp(output, "/"), 267)) return 0;
    int absolute = openat(999999, base, O_RDONLY | O_DIRECTORY);
    if (!check(absolute >= 3, 268)) return 0;
    close(absolute);
    int relative = openat(directory, "value", O_RDONLY);
    memset(output, 0, sizeof(output));
    if (!check(relative >= 3 && read(relative, output, sizeof(output)) == 10 &&
        !memcmp(output, "native-cwd", 10), 269)) return 0;
    close(relative);
    int created = openat(directory, "created", O_CREAT | O_RDWR | O_EXCL, 0600);
    if (!check(created >= 3 && fstat(created, &byname) == 0 && fstatat(directory, "created", &actual, 0) == 0 &&
        actual.st_ino == byname.st_ino && close(created) == 0 && unlinkat(directory, "created", 0) == 0 &&
        fstat(directory, &actual) == 0, 288)) return 0;
    if (!check(mkdirat(directory, "child", 0700) == 0 &&
        syscall(SYS_renameat, directory, "value", directory, "renamed") == 0 &&
        fchdir(directory) == 0 && access("renamed", F_OK) == 0, 270)) return 0;
    errno = 0;
    if (!check(unlink("child") == -1 && errno == EISDIR, 271)) return 0;
    errno = 0;
    if (!check(rmdir("renamed") == -1 && errno == ENOTDIR, 272)) return 0;
    errno = 0;
    if (!check(rmdir(".") == -1 && errno == EINVAL && stat(".", &byname) == 0, 273)) return 0;
    if (!check(rename(base, moved) == 0, 274)) return 0;
    snprintf(expected, sizeof(expected), "%s/a/b", moved);
    if (!check(getcwd(output, sizeof(output)) && !strcmp(output, expected) &&
        chdir("/") == 0 && fchdir(directory) == 0 && getcwd(output, sizeof(output)) &&
        !strcmp(output, expected) && fstat(directory, &byname) == 0 && actual.st_ino == byname.st_ino, 275)) return 0;
#ifdef PROBE_THREADS
    pthread_t thread;
    void *result = NULL;
    char *parent = "..";
    if (!check(pthread_create(&thread, NULL, cwd_worker, parent) == 0 &&
        pthread_join(thread, &result) == 0 && result == parent, 276)) return 0;
    snprintf(expected, sizeof(expected), "%s/a", moved);
    if (!check(getcwd(output, sizeof(output)) && !strcmp(output, expected) && fchdir(directory) == 0, 277)) return 0;
#endif
    // Open descriptors and cwd pin native nodes through rename and unlink.
    close(file);
    if (!check(unlinkat(directory, "renamed", 0) == 0 && unlinkat(directory, "child", AT_REMOVEDIR) == 0 &&
        chdir("..") == 0 && rmdir("b") == 0, 278)) return 0;
    errno = 0;
    if (!check(fchdir(directory) == -1 && errno == ENOENT, 279)) return 0;
    if (!check(mkdir("b", 0700) == 0 && chdir("b") == 0 && stat(".", &byname) == 0, 280)) return 0;
    if (!check(byname.st_ino != actual.st_ino, 284)) return 0;
    close(directory);
    snprintf(expected, sizeof(expected), "%s/a/b", moved);
    if (!check(rmdir(expected) == 0, 281)) return 0;
    errno = 0;
    if (!check(getcwd(output, sizeof(output)) == NULL && errno == ENOENT && chdir("/") == 0, 282)) return 0;
    snprintf(expected, sizeof(expected), "%s/a", moved);
    if (!check(rmdir(expected) == 0 && rmdir(moved) == 0, 283)) return 0;
    puts(PROBE_NAME ": PASS cwd parent traversal dirfd rename identity and removed directories");
    return 1;
}
