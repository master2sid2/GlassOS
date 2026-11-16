/* vsnprintf: flags, width, precision, d i u x X o c s p % f e E g G a A.
 * Length modifiers: hh h l ll z j t. This is enough for Lua (tostring and string.format). */

#include <math.h>
#include <stdarg.h>
#include <stddef.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>

typedef struct {
    char *buf;
    size_t cap;
    size_t len;
} out;

static void put(out *o, char c) {
    if (o->len + 1 < o->cap) o->buf[o->len] = c;
    o->len++;
}

static void put_n(out *o, const char *s, int n) {
    while (n-- > 0) put(o, *s++);
}

static void pad(out *o, char c, int n) {
    while (n-- > 0) put(o, c);
}

typedef struct {
    int left, plus, space, alt, zero, width, prec;
} spec;

/* Prints the sign/prefix and the body padded to the width. */
static void emit(out *o, const spec *sp, const char *prefix, const char *body, int blen) {
    int plen = (int)strlen(prefix);
    int fill = sp->width - plen - blen;
    if (!sp->left && !sp->zero) pad(o, ' ', fill);
    put_n(o, prefix, plen);
    if (!sp->left && sp->zero) pad(o, '0', fill);
    put_n(o, body, blen);
    if (sp->left) pad(o, ' ', fill);
}

static const char *sign_of(const spec *sp, int neg) {
    return neg ? "-" : sp->plus ? "+" : sp->space ? " " : "";
}

/* ---------------------------------------------------------------- integers */

static void fmt_int(out *o, spec sp, unsigned long long v, int neg, int base, int upper) {
    char tmp[80];
    char *end = tmp + sizeof tmp, *p = end;
    const char *digits = upper ? "0123456789ABCDEF" : "0123456789abcdef";
    if (!(v == 0 && sp.prec == 0)) {
        do {
            *--p = digits[v % base];
            v /= base;
        } while (v);
    }
    while (end - p < sp.prec) *--p = '0';
    if (base == 8 && sp.alt && *p != '0') *--p = '0';
    const char *prefix = sign_of(&sp, neg);
    if (base == 16 && sp.alt && end - p > 0) prefix = upper ? "0X" : "0x";
    if (sp.prec >= 0) sp.zero = 0;
    emit(o, &sp, prefix, p, (int)(end - p));
}

/* ---------------------------------------------------------------- floating point */

static double pow10d(int e) {
    double r = 1, b = 10;
    int n = e < 0 ? -e : e;
    while (n) {
        if (n & 1) r *= b;
        b *= b;
        n >>= 1;
    }
    return e < 0 ? 1 / r : r;
}

/* ndig significant digits of v > 0 into d[]; returns the decimal exponent e:
   v ≈ d0.d1d2... * 10^e. */
static int sig_digits(double v, int ndig, char *d) {
    if (ndig > 17) ndig = 17;
    if (ndig < 1) ndig = 1;
    unsigned long long lo = 1, n = 0;
    for (int i = 1; i < ndig; i++) lo *= 10;
    unsigned long long hi = lo * 10;
    int e = (int)floor(log10(v));
    for (int iter = 0; iter < 4; iter++) {
        int s = ndig - 1 - e;
        double scaled = v;
        if (s > 300) {
            scaled *= 1e300;
            s -= 300;
        }
        scaled = s >= 0 ? scaled * pow10d(s) : scaled / pow10d(-s);
        n = (unsigned long long)(scaled + 0.5);
        if (n >= hi) e++;
        else if (n < lo) e--;
        else break;
    }
    if (n >= hi) {
        n /= 10;
        e++;
    }
    for (int i = ndig - 1; i >= 0; i--) {
        d[i] = (char)('0' + n % 10);
        n /= 10;
    }
    return e;
}

/* Digit number i (0 is the most significant) or '0' beyond the generated ones. */
static char digit_at(const char *d, int ndig, int i) {
    return i >= 0 && i < ndig ? d[i] : '0';
}

/* Builds a %e-style representation from the digits d[0..ndig). */
static int build_e(char *b, const char *d, int ndig, int e, int prec, int alt, int upper) {
    int n = 0;
    b[n++] = d[0];
    if (prec > 0 || alt) b[n++] = '.';
    for (int i = 1; i <= prec; i++) b[n++] = digit_at(d, ndig, i);
    b[n++] = upper ? 'E' : 'e';
    b[n++] = e < 0 ? '-' : '+';
    int ae = e < 0 ? -e : e;
    if (ae >= 100) b[n++] = (char)('0' + ae / 100);
    b[n++] = (char)('0' + ae / 10 % 10);
    b[n++] = (char)('0' + ae % 10);
    return n;
}

/* Builds a %f-style representation with prec digits after the point. */
static int build_f(char *b, const char *d, int ndig, int e, int prec, int alt) {
    int n = 0;
    if (e < 0) b[n++] = '0';
    else
        for (int i = 0; i <= e; i++) b[n++] = digit_at(d, ndig, i);
    if (prec > 0 || alt) b[n++] = '.';
    for (int i = 0; i < prec; i++) b[n++] = digit_at(d, ndig, e + 1 + i);
    return n;
}

static void fmt_float(out *o, spec sp, double v, char conv) {
    int upper = conv >= 'A' && conv <= 'Z';
    int neg = signbit(v);
    if (neg) v = -v;
    if (isnan(v) || isinf(v)) {
        sp.zero = 0;
        const char *s = isnan(v) ? (upper ? "NAN" : "nan") : (upper ? "INF" : "inf");
        emit(o, &sp, sign_of(&sp, neg), s, 3);
        return;
    }
    char lc = (char)(conv | 0x20);
    if (lc == 'a') {
        /* hexadecimal notation: 0x1.xxxxp+e */
        char b[64];
        int n = 0;
        int e;
        double m = v == 0 ? 0 : frexp(v, &e);
        b[n++] = '0';
        b[n++] = upper ? 'X' : 'x';
        if (v == 0) {
            b[n++] = '0';
            e = 0;
        } else {
            m *= 2;
            e -= 1;
            b[n++] = '1';
            m -= 1;
            int start = n;
            b[n++] = '.';
            for (int i = 0; i < 13 && m > 0; i++) {
                m *= 16;
                int dgt = (int)m;
                m -= dgt;
                b[n++] = (upper ? "0123456789ABCDEF" : "0123456789abcdef")[dgt];
            }
            if (n == start + 1) n = start;
        }
        n += snprintf(b + n, sizeof b - n, "%c%+d", upper ? 'P' : 'p', e);
        emit(o, &sp, sign_of(&sp, neg), b, n);
        return;
    }

    int prec = sp.prec < 0 ? 6 : sp.prec;
    if (prec > 60) prec = 60;
    char d[20], b[400];
    int n, e;
    if (lc == 'e') {
        e = v == 0 ? (memset(d, '0', 17), 0) : sig_digits(v, prec + 1, d);
        n = build_e(b, d, 17, e, prec, sp.alt, upper);
    } else if (lc == 'f') {
        if (v == 0) {
            memset(d, '0', 17);
            e = 0;
            n = build_f(b, d, 17, 0, prec, sp.alt);
        } else {
            e = (int)floor(log10(v));
            int ndig = e + 1 + prec;
            if (ndig <= 0) {
                /* the number is below half of the last digit or rounds to it */
                int up = v * pow10d(prec) >= 0.5;
                memset(d, '0', 17);
                n = build_f(b, d, 17, 0, prec, sp.alt);
                b[0] = '0';
                if (up) b[n - 1] = '1';
                if (e > 300) n = 0;
            } else {
                int got = ndig > 17 ? 17 : ndig;
                e = sig_digits(v, got, d);
                if (e > 300) e = 300;
                n = build_f(b, d, got, e, prec, sp.alt);
            }
        }
    } else { /* g */
        int p = prec == 0 ? 1 : prec;
        if (v == 0) {
            memset(d, '0', 17);
            e = 0;
        } else {
            e = sig_digits(v, p, d);
        }
        int ndig = p > 17 ? 17 : p;
        if (e < -4 || e >= p) n = build_e(b, d, ndig, e, p - 1, sp.alt, upper);
        else n = build_f(b, d, ndig, e, p - 1 - e, sp.alt);
        if (!sp.alt) {
            /* strip trailing zeros of the fraction (and the point) */
            int epos = n;
            for (int i = 0; i < n; i++)
                if (b[i] == 'e' || b[i] == 'E') epos = i;
            int has_point = 0;
            for (int i = 0; i < epos; i++)
                if (b[i] == '.') has_point = 1;
            if (has_point) {
                int k = epos;
                while (k > 0 && b[k - 1] == '0') k--;
                if (k > 0 && b[k - 1] == '.') k--;
                memmove(b + k, b + epos, n - epos);
                n -= epos - k;
            }
        }
    }
    emit(o, &sp, sign_of(&sp, neg), b, n);
}

/* ---------------------------------------------------------------- vsnprintf */

int vsnprintf(char *buf, size_t cap, const char *fmt, va_list ap) {
    out o = {buf, cap, 0};
    for (const char *f = fmt; *f; f++) {
        if (*f != '%') {
            put(&o, *f);
            continue;
        }
        spec sp = {0, 0, 0, 0, 0, 0, -1};
        for (;; f++) {
            char c = f[1];
            if (c == '-') sp.left = 1;
            else if (c == '+') sp.plus = 1;
            else if (c == ' ') sp.space = 1;
            else if (c == '#') sp.alt = 1;
            else if (c == '0') sp.zero = 1;
            else break;
        }
        f++;
        if (*f == '*') {
            sp.width = va_arg(ap, int);
            if (sp.width < 0) {
                sp.left = 1;
                sp.width = -sp.width;
            }
            f++;
        } else {
            while (*f >= '0' && *f <= '9') sp.width = sp.width * 10 + (*f++ - '0');
        }
        if (*f == '.') {
            f++;
            sp.prec = 0;
            if (*f == '*') {
                sp.prec = va_arg(ap, int);
                f++;
            } else {
                while (*f >= '0' && *f <= '9') sp.prec = sp.prec * 10 + (*f++ - '0');
            }
        }
        if (sp.left) sp.zero = 0;
        int lng = 0; /* 0 int, 1 long (32 bits on Win64), 2 long long/size_t */
        for (;; f++) {
            if (*f == 'l') lng = lng == 0 ? 1 : 2;
            else if (*f == 'z' || *f == 'j' || *f == 't') lng = 2;
            else if (*f == 'h' || *f == 'L') {
            } else break;
        }
        char conv = *f;
        if (!conv) break;
        switch (conv) {
        case 'd':
        case 'i': {
            long long v = lng == 2 ? va_arg(ap, long long) : lng == 1 ? va_arg(ap, long) : va_arg(ap, int);
            fmt_int(&o, sp, v < 0 ? 0ULL - (unsigned long long)v : (unsigned long long)v, v < 0, 10, 0);
            break;
        }
        case 'u':
        case 'x':
        case 'X':
        case 'o': {
            unsigned long long v = lng == 2   ? va_arg(ap, unsigned long long)
                                   : lng == 1 ? va_arg(ap, unsigned long)
                                              : va_arg(ap, unsigned int);
            int base = conv == 'o' ? 8 : conv == 'u' ? 10 : 16;
            sp.plus = sp.space = 0;
            fmt_int(&o, sp, v, 0, base, conv == 'X');
            break;
        }
        case 'p': {
            sp.alt = 1;
            fmt_int(&o, sp, (unsigned long long)(uintptr_t)va_arg(ap, void *), 0, 16, 0);
            break;
        }
        case 'c': {
            char ch = (char)va_arg(ap, int);
            sp.zero = 0;
            emit(&o, &sp, "", &ch, 1);
            break;
        }
        case 's': {
            const char *s = va_arg(ap, const char *);
            if (!s) s = "(null)";
            int len = 0;
            while (s[len] && (sp.prec < 0 || len < sp.prec)) len++;
            sp.zero = 0;
            emit(&o, &sp, "", s, len);
            break;
        }
        case 'f':
        case 'F':
        case 'e':
        case 'E':
        case 'g':
        case 'G':
        case 'a':
        case 'A':
            fmt_float(&o, sp, va_arg(ap, double), conv);
            break;
        case '%': put(&o, '%'); break;
        default:
            put(&o, '%');
            put(&o, conv);
        }
    }
    if (cap) o.buf[o.len < cap ? o.len : cap - 1] = 0;
    return (int)o.len;
}

int snprintf(char *buf, size_t n, const char *fmt, ...) {
    va_list ap;
    va_start(ap, fmt);
    int r = vsnprintf(buf, n, fmt, ap);
    va_end(ap);
    return r;
}

int sprintf(char *buf, const char *fmt, ...) {
    va_list ap;
    va_start(ap, fmt);
    int r = vsnprintf(buf, (size_t)-1 >> 1, fmt, ap);
    va_end(ap);
    return r;
}
