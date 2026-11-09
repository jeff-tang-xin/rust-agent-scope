#include <stdint.h>
#include <stdio.h>
#include <string.h>

/* FNV-1a 64-bit hash. */
static uint64_t fnv1a(const char *s, size_t len) {
    uint64_t h = 1469598103934665603ULL;
    for (size_t i = 0; i < len; i++) {
        h ^= (unsigned char)s[i];
        h *= 1099511628211ULL;
    }
    return h;
}

int main(int argc, char **argv) {
    int unused = 42; /* intentional warning: unused variable */
    for (int i = 1; i < argc; i++) {
        printf("%016llx  %s\n", (unsigned long long)fnv1a(argv[i], strlen(argv[i])), argv[i]);
    }
    return 0;
}
