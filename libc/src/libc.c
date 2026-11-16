/* A minimal libc for Lua in GlassOS.
 *
 * Memory, output and time come from the kernel (the myos_* functions are implemented in Rust).
 * memcpy/memset/memmove/memcmp/strlen come from Rust's compiler_builtins. */

#include <ctype.h>
#include <errno.h>
#include <locale.h>
#include <stddef.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>

extern void *myos_alloc(size_t size);
extern void myos_free(void *p, size_t size);
extern void myos_write(const char *s, size_t n);
extern void myos_abort(const char *msg) __attribute__((noreturn));
extern unsigned long long myos_ticks_ms(void);

int errno;

/* ---------------------------------------------------------------- memory */

/* The block size is stored before the block: the kernel allocator needs the size when freeing too.
   The 16-byte header keeps 16-byte alignment. */
typedef struct {
    size_t size;
    size_t pad;
} header;

void *malloc(size_t n) {
    header *h = myos_alloc(n + sizeof(header));
    if (!h) return NULL;
    h->size = n;
    return h + 1;
}

void free(void *p) {
    if (!p) return;
    header *h = (header *)p - 1;
    myos_free(h, h->size + sizeof(header));
}

void *realloc(void *p, size_t n) {
    if (!p) return malloc(n);
    if (n == 0) {
        free(p);
        return NULL;
    }
    header *h = (header *)p - 1;
    if (n <= h->size && n >= h->size / 2) return p;
    void *q = malloc(n);
    if (!q) return NULL;
    memcpy(q, p, h->size < n ? h->size : n);
    free(p);
    return q;
}

void *calloc(size_t n, size_t size) {
    void *p = malloc(n * size);
    if (p) memset(p, 0, n * size);
    return p;
}

void abort(void) { myos_abort("abort() called"); }
void exit(int code) { (void)code; myos_abort("exit() called"); }
char *getenv(const char *name) { (void)name; return NULL; }
int abs(int x) { return x < 0 ? -x : x; }

/* ---------------------------------------------------------------- strings */

int strcmp(const char *a, const char *b) {
    while (*a && *a == *b) a++, b++;
    return (unsigned char)*a - (unsigned char)*b;
}

int strncmp(const char *a, const char *b, size_t n) {
    for (; n; n--, a++, b++) {
        if (*a != *b || !*a) return (unsigned char)*a - (unsigned char)*b;
    }
    return 0;
}

int strcoll(const char *a, const char *b) { return strcmp(a, b); }

char *strchr(const char *s, int c) {
    for (;; s++) {
        if (*s == (char)c) return (char *)s;
        if (!*s) return NULL;
    }
}

char *strrchr(const char *s, int c) {
    const char *last = NULL;
    for (;; s++) {
        if (*s == (char)c) last = s;
        if (!*s) return (char *)last;
    }
}

void *memchr(const void *s, int c, size_t n) {
    const unsigned char *p = s;
    for (; n; n--, p++) {
        if (*p == (unsigned char)c) return (void *)p;
    }
    return NULL;
}

char *strstr(const char *h, const char *n) {
    size_t len = strlen(n);
    if (!len) return (char *)h;
    for (; *h; h++) {
        if (*h == *n && strncmp(h, n, len) == 0) return (char *)h;
    }
    return NULL;
}

size_t strspn(const char *s, const char *accept) {
    size_t n = 0;
    while (s[n] && strchr(accept, s[n])) n++;
    return n;
}

size_t strcspn(const char *s, const char *reject) {
    size_t n = 0;
    while (s[n] && !strchr(reject, s[n])) n++;
    return n;
}

char *strpbrk(const char *s, const char *accept) {
    s += strcspn(s, accept);
    return *s ? (char *)s : NULL;
}

char *strcpy(char *dst, const char *src) {
    char *d = dst;
    while ((*d++ = *src++)) {}
    return dst;
}

char *strncpy(char *dst, const char *src, size_t n) {
    size_t i = 0;
    for (; i < n && src[i]; i++) dst[i] = src[i];
    for (; i < n; i++) dst[i] = 0;
    return dst;
}

char *strerror(int err) {
    switch (err) {
    case ENOENT: return "No such file or directory";
    case EINVAL: return "Invalid argument";
    case ERANGE: return "Result out of range";
    default: return "Unknown error";
    }
}

long strtol(const char *s, char **end, int base) {
    const char *p = s;
    while (isspace(*p)) p++;
    int neg = 0;
    if (*p == '+' || *p == '-') neg = *p++ == '-';
    if ((base == 0 || base == 16) && p[0] == '0' && (p[1] == 'x' || p[1] == 'X')) {
        p += 2;
        base = 16;
    } else if (base == 0) {
        base = *p == '0' ? 8 : 10;
    }
    long v = 0;
    const char *start = p;
    for (;; p++) {
        int d = isdigit(*p) ? *p - '0' : isalpha(*p) ? tolower(*p) - 'a' + 10 : 99;
        if (d >= base) break;
        v = v * base + d;
    }
    if (end) *end = (char *)(p == start ? s : p);
    return neg ? -v : v;
}

/* ---------------------------------------------------------------- ctype (ASCII) */

int isdigit(int c) { return c >= '0' && c <= '9'; }
int islower(int c) { return c >= 'a' && c <= 'z'; }
int isupper(int c) { return c >= 'A' && c <= 'Z'; }
int isalpha(int c) { return islower(c) || isupper(c); }
int isalnum(int c) { return isalpha(c) || isdigit(c); }
int isxdigit(int c) { return isdigit(c) || (c >= 'a' && c <= 'f') || (c >= 'A' && c <= 'F'); }
int isspace(int c) { return c == ' ' || (c >= '\t' && c <= '\r'); }
int iscntrl(int c) { return (c >= 0 && c < 32) || c == 127; }
int isprint(int c) { return c >= 32 && c < 127; }
int isgraph(int c) { return c > 32 && c < 127; }
int ispunct(int c) { return isgraph(c) && !isalnum(c); }
int tolower(int c) { return isupper(c) ? c + 32 : c; }
int toupper(int c) { return islower(c) ? c - 32 : c; }

/* ---------------------------------------------------------------- locale, time */

static char decimal_point[] = ".";
static struct lconv lconv_c = {decimal_point};
struct lconv *localeconv(void) { return &lconv_c; }
char *setlocale(int category, const char *locale) {
    (void)category;
    (void)locale;
    return "C";
}

time_t time(time_t *t) {
    time_t v = (time_t)(myos_ticks_ms() / 1000);
    if (t) *t = v;
    return v;
}

clock_t clock(void) { return (clock_t)myos_ticks_ms(); }

/* ---------------------------------------------------------------- stdio
 * libc has no files on disk: stdout/stderr go to the kernel (to the terminal or
 * the debug port), everything else honestly returns an error. */

struct myos_file {
    int fd;
};
static FILE files[3] = {{0}, {1}, {2}};
FILE *stdin = &files[0], *stdout = &files[1], *stderr = &files[2];

FILE *fopen(const char *path, const char *mode) {
    (void)path;
    (void)mode;
    errno = ENOENT;
    return NULL;
}
FILE *freopen(const char *path, const char *mode, FILE *f) {
    (void)path;
    (void)mode;
    (void)f;
    errno = ENOENT;
    return NULL;
}
int fclose(FILE *f) { (void)f; return 0; }
size_t fread(void *p, size_t size, size_t n, FILE *f) {
    (void)p;
    (void)size;
    (void)n;
    (void)f;
    return 0;
}
size_t fwrite(const void *p, size_t size, size_t n, FILE *f) {
    if (f != stdout && f != stderr) return 0;
    myos_write(p, size * n);
    return n;
}
int fputs(const char *s, FILE *f) { return fwrite(s, 1, strlen(s), f) ? 0 : EOF; }
int fputc(int c, FILE *f) {
    char ch = (char)c;
    return fwrite(&ch, 1, 1, f) ? c : EOF;
}
int vfprintf(FILE *f, const char *fmt, va_list ap) {
    char buf[512];
    int n = vsnprintf(buf, sizeof buf, fmt, ap);
    fwrite(buf, 1, n < (int)sizeof buf ? (size_t)n : sizeof buf - 1, f);
    return n;
}
int fprintf(FILE *f, const char *fmt, ...) {
    va_list ap;
    va_start(ap, fmt);
    int n = vfprintf(f, fmt, ap);
    va_end(ap);
    return n;
}
int printf(const char *fmt, ...) {
    va_list ap;
    va_start(ap, fmt);
    int n = vfprintf(stdout, fmt, ap);
    va_end(ap);
    return n;
}
int getc(FILE *f) { (void)f; return EOF; }
int ungetc(int c, FILE *f) { (void)c; (void)f; return EOF; }
int feof(FILE *f) { (void)f; return 1; }
int ferror(FILE *f) { (void)f; return 0; }
int fflush(FILE *f) { (void)f; return 0; }
void clearerr(FILE *f) { (void)f; }

/* ---------------------------------------------------------------- setjmp/longjmp
 * System V ABI: xmm registers need not be saved, nobody is obliged to preserve them. */

__asm__(".intel_syntax noprefix\n"
        ".globl myos_setjmp\n"
        "myos_setjmp:\n"
        "  mov [rdi], rbx\n"
        "  mov [rdi+8], rbp\n"
        "  mov [rdi+16], r12\n"
        "  mov [rdi+24], r13\n"
        "  mov [rdi+32], r14\n"
        "  mov [rdi+40], r15\n"
        "  lea rdx, [rsp+8]\n"
        "  mov [rdi+48], rdx\n"
        "  mov rdx, [rsp]\n"
        "  mov [rdi+56], rdx\n"
        "  xor eax, eax\n"
        "  ret\n"
        ".globl myos_longjmp\n"
        "myos_longjmp:\n"
        "  mov rbx, [rdi]\n"
        "  mov rbp, [rdi+8]\n"
        "  mov r12, [rdi+16]\n"
        "  mov r13, [rdi+24]\n"
        "  mov r14, [rdi+32]\n"
        "  mov r15, [rdi+40]\n"
        "  mov eax, esi\n"
        "  test eax, eax\n"
        "  jnz 1f\n"
        "  mov eax, 1\n"
        "1:\n"
        "  mov rsp, [rdi+48]\n"
        "  jmp qword ptr [rdi+56]\n"
        ".att_syntax prefix\n");
