static int symlink_probe(void) {
    char base[128], expected[256], output[4096];
    snprintf(base, sizeof(base), "/tmp/links-%ld", (long)getpid());
    if (!check(mkdir(base, 0700) == 0 && chdir(base) == 0 && mkdir("dir", 0700) == 0 && mkdir("dir/sub", 0700) == 0, 300)) return 0;
    int file = open("dir/value", O_RDWR | O_CREAT | O_EXCL, 0600);
    struct stat original, followed, link;
    if (!check(file >= 3 && write(file, "native-link", 11) == 11 && fstat(file, &original) == 0 && close(file) == 0 &&
        symlink("dir/value", "alias") == 0 && symlink("dir", "directory") == 0 &&
        stat("alias", &followed) == 0 && original.st_ino == followed.st_ino &&
        lstat("alias", &link) == 0 && S_ISLNK(link.st_mode) && link.st_size == 9 && link.st_ino != original.st_ino, 301)) return 0;
    memset(output, 0x5a, sizeof(output));
    if (!check(readlink("alias", output, 3) == 3 && !memcmp(output, "dir", 3) && output[3] == 0x5a, 302)) return 0;
    errno = 0;
    if (!check(open("alias", O_RDONLY | O_NOFOLLOW) == -1 && errno == ELOOP, 303)) return 0;
    int directory = open(".", O_RDONLY | O_DIRECTORY);
    if (!check(directory >= 3 && symlinkat("../value", directory, "dir/sub/back") == 0 &&
        chdir("directory/sub") == 0 && stat("back", &followed) == 0 && original.st_ino == followed.st_ino, 304)) return 0;
    snprintf(expected, sizeof(expected), "%s/dir/sub", base);
    if (!check(getcwd(output, sizeof(output)) && !strcmp(output, expected) &&
        readlinkat(directory, "dir/sub/back", output, sizeof(output)) == 8 && !memcmp(output, "../value", 8), 305)) return 0;
    if (!check(fchdir(directory) == 0 && symlink("created", "dangling") == 0, 306)) return 0;
    errno = 0;
    if (!check(stat("dangling", &followed) == -1 && errno == ENOENT && lstat("dangling", &link) == 0 && S_ISLNK(link.st_mode), 307)) return 0;
    errno = 0;
    if (!check(open("dangling", O_CREAT | O_EXCL | O_WRONLY, 0600) == -1 && errno == EEXIST, 308)) return 0;
    file = open("dangling", O_CREAT | O_WRONLY, 0600);
    if (!check(file >= 3 && write(file, "target", 6) == 6 && close(file) == 0 &&
        unlink("dangling") == 0 && stat("created", &followed) == 0 && followed.st_size == 6, 309)) return 0;
    if (!check(symlink("loop-b", "loop-a") == 0 && symlink("loop-a", "loop-b") == 0, 310)) return 0;
    errno = 0;
    if (!check(stat("loop-a", &followed) == -1 && errno == ELOOP && lstat("loop-a", &link) == 0, 311)) return 0;
    if (!check(symlink("/", "root") == 0 && chdir("root/../../../") == 0 && getcwd(output, sizeof(output)) && !strcmp(output, "/") &&
        fchdir(directory) == 0, 312)) return 0;
    if (!check(rename("alias", "moved") == 0 && readlink("moved", output, sizeof(output)) == 9 &&
        !memcmp(output, "dir/value", 9) && stat("moved", &followed) == 0 && original.st_ino == followed.st_ino, 313)) return 0;
    errno = 0;
    if (!check(rmdir("directory/") == -1 && errno == ENOTDIR && stat("dir/value", &followed) == 0, 314)) return 0;
    DIR *list = opendir(".");
    struct dirent *entry;
    int seen = 0;
    if (!check(list != NULL, 315)) return 0;
    while ((entry = readdir(list))) if (!strcmp(entry->d_name, "moved") && entry->d_type == DT_LNK) seen++;
    if (!check(closedir(list) == 0 && seen == 1, 316)) return 0;
    void *readonly = mmap(NULL, 4096, PROT_READ, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    errno = 0;
    if (!check(readonly != MAP_FAILED && readlink("moved", readonly, 4096) == -1 && errno == EFAULT, 317)) return 0;
    munmap(readonly, 4096);
    errno = 0;
    if (!check(symlink("", "empty") == -1 && errno == ENOENT, 318)) return 0;
    if (!check(unlink("moved") == 0 && unlink("directory") == 0 && unlink("dir/sub/back") == 0 &&
        unlink("loop-a") == 0 && unlink("loop-b") == 0 && unlink("root") == 0 && unlink("created") == 0 &&
        unlink("dir/value") == 0 && rmdir("dir/sub") == 0 && rmdir("dir") == 0 && chdir("/") == 0 &&
        close(directory) == 0 && rmdir(base) == 0, 319)) return 0;
    puts(PROBE_NAME ": PASS persistent symlinks follow lstat readlink loops dirfd and safe unlink");
    return 1;
}
