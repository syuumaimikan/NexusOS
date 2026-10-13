/* dlopen probe: relocation and library TLS must both work. */
static _Thread_local volatile int value = 47;
int nexus_library_value(void) { return value; }
