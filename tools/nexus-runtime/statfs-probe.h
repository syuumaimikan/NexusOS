/* Verify the Linux filesystem-stat ABI for the native word size. */
static int statfs_probe(void) {
    struct statfs path_info, descriptor_info, console_info;
    int directory = open(".", O_RDONLY | O_DIRECTORY);
    if (!check(directory >= 3 && statfs(".", &path_info) == 0 &&
        fstatfs(directory, &descriptor_info) == 0 &&
        fstatfs(STDOUT_FILENO, &console_info) == 0, 330)) return 0;
    if (!check(path_info.f_type == descriptor_info.f_type &&
        path_info.f_bsize == descriptor_info.f_bsize && path_info.f_bsize == 4096 &&
        path_info.f_blocks > 0 && path_info.f_bavail <= path_info.f_blocks, 331)) return 0;
    close(directory);
    puts(PROBE_NAME ": PASS statfs and fstatfs filesystem identity and capacity");
    return 1;
}
