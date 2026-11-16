/* Math for Lua: simple implementations through argument reduction and series.
 * The accuracy is about 1e-15 relative error; enough for scripts. */

#include <ctype.h>
#include <math.h>
#include <stdint.h>
#include <stdlib.h>

#define PI 3.14159265358979323846
#define LN2 0.69314718055994530942
#define LN2_HI 6.93147180369123816490e-01
#define LN2_LO 1.90821492927058770002e-10
#define LOG2E 1.44269504088896340736
#define LOG10E 0.43429448190325182765
#define PIO2_HI 1.57079632673412561417e+00
#define PIO2_LO 6.07710050650619224932e-11

typedef union {
    double d;
    uint64_t u;
} bits;

double fabs(double x) {
    bits b = {x};
    b.u &= ~(1ULL << 63);
    return b.d;
}

double floor(double x) {
    bits b = {x};
    int e = (int)((b.u >> 52) & 0x7FF) - 1023;
    if (e >= 52) return x; /* already an integer, inf or nan */
    if (e < 0) {
        if (x == 0) return x;
        return x > 0 ? 0.0 : -1.0;
    }
    double d = (double)(long long)x;
    return d > x ? d - 1.0 : d;
}

double ceil(double x) { return -floor(-x); }
double trunc(double x) { return x < 0 ? ceil(x) : floor(x); }
double sqrt(double x) { return __builtin_sqrt(x); }

double frexp(double x, int *e) {
    bits b = {x};
    int ex = (int)((b.u >> 52) & 0x7FF);
    if (ex == 0) {
        if (x == 0) {
            *e = 0;
            return x;
        }
        /* subnormal: normalize by multiplying by 2^64 */
        x = frexp(x * 18446744073709551616.0, e);
        *e -= 64;
        return x;
    }
    if (ex == 0x7FF) {
        *e = 0;
        return x;
    }
    *e = ex - 1022;
    b.u = (b.u & ~(0x7FFULL << 52)) | (1022ULL << 52);
    return b.d;
}

double ldexp(double x, int n) {
    while (n > 1023) {
        x *= 8.98846567431157953865e+307; /* 2^1023 */
        n -= 1023;
        if (n > 1023 && isinf(x)) return x;
    }
    while (n < -1022) {
        x *= 2.22507385850720138309e-308; /* 2^-1022 */
        n += 1022;
        if (x == 0) return x;
    }
    bits b = {.u = (uint64_t)(n + 1023) << 52};
    return x * b.d;
}

double exp(double x) {
    if (isnan(x)) return x;
    if (x > 709.78) return HUGE_VAL;
    if (x < -745.2) return 0.0;
    int k = (int)floor(x * LOG2E + 0.5);
    double r = (x - k * LN2_HI) - k * LN2_LO;
    double term = 1.0, sum = 1.0;
    for (int i = 1; i <= 14; i++) {
        term *= r / i;
        sum += term;
    }
    return ldexp(sum, k);
}

double log(double x) {
    if (isnan(x) || x < 0) return NAN;
    if (x == 0) return -HUGE_VAL;
    if (isinf(x)) return x;
    int e;
    double m = frexp(x, &e);
    if (m < 0.70710678118654752440) {
        m *= 2;
        e -= 1;
    }
    double s = (m - 1) / (m + 1), s2 = s * s, term = s, sum = 0;
    for (int i = 1; i <= 23; i += 2) {
        sum += term / i;
        term *= s2;
    }
    return e * LN2 + 2 * sum;
}

double log2(double x) { return log(x) * LOG2E; }
double log10(double x) { return log(x) * LOG10E; }

double pow(double x, double y) {
    if (y == 0 || x == 1) return 1.0;
    if (isnan(x) || isnan(y)) return NAN;
    if (y == floor(y) && fabs(y) < 2147483648.0) {
        long long n = (long long)fabs(y);
        double r = 1.0, b = x;
        while (n) {
            if (n & 1) r *= b;
            b *= b;
            n >>= 1;
        }
        return y < 0 ? 1.0 / r : r;
    }
    if (x == 0) return y > 0 ? 0.0 : HUGE_VAL;
    if (x < 0) return NAN;
    return exp(y * log(x));
}

double fmod(double x, double y) {
    if (y == 0 || isnan(x) || isnan(y) || isinf(x)) return NAN;
    if (isinf(y)) return x;
    double r = fabs(x), a = fabs(y);
    int er, ea;
    while (r >= a) {
        frexp(r, &er);
        frexp(a, &ea);
        double t = ldexp(a, er - ea);
        if (t > r) t /= 2;
        r -= t;
    }
    return x < 0 ? -r : r;
}

/* Taylor polynomials for |r| <= pi/4. */
static double sin_poly(double r) {
    double r2 = r * r, term = r, sum = r;
    for (int i = 3; i <= 19; i += 2) {
        term *= -r2 / ((i - 1) * i);
        sum += term;
    }
    return sum;
}

static double cos_poly(double r) {
    double r2 = r * r, term = 1, sum = 1;
    for (int i = 2; i <= 20; i += 2) {
        term *= -r2 / ((i - 1) * i);
        sum += term;
    }
    return sum;
}

/* Reduces x to r in [-pi/4, pi/4]; returns the quadrant number. */
static int reduce(double x, double *r) {
    double k = floor(x / (PI / 2) + 0.5);
    *r = (x - k * PIO2_HI) - k * PIO2_LO;
    return (int)((long long)k & 3);
}

double sin(double x) {
    if (isnan(x) || isinf(x)) return NAN;
    double r;
    switch (reduce(x, &r)) {
    case 0: return sin_poly(r);
    case 1: return cos_poly(r);
    case 2: return -sin_poly(r);
    default: return -cos_poly(r);
    }
}

double cos(double x) {
    if (isnan(x) || isinf(x)) return NAN;
    double r;
    switch (reduce(x, &r)) {
    case 0: return cos_poly(r);
    case 1: return -sin_poly(r);
    case 2: return -cos_poly(r);
    default: return sin_poly(r);
    }
}

double tan(double x) { return sin(x) / cos(x); }

double atan(double x) {
    if (isnan(x)) return x;
    int neg = x < 0;
    double a = fabs(x);
    int invert = a > 1;
    if (invert) a = 1 / a;
    /* atan(t) = 2*atan(t / (1 + sqrt(1 + t^2))) — reduce the argument twice */
    for (int i = 0; i < 2; i++) a = a / (1 + sqrt(1 + a * a));
    double a2 = a * a, term = a, sum = 0;
    for (int i = 1; i <= 27; i += 2) {
        sum += term / i;
        term *= -a2;
    }
    double res = 4 * sum;
    if (invert) res = PI / 2 - res;
    return neg ? -res : res;
}

double atan2(double y, double x) {
    if (isnan(x) || isnan(y)) return NAN;
    if (x > 0) return atan(y / x);
    if (x < 0) return y >= 0 ? atan(y / x) + PI : atan(y / x) - PI;
    if (y > 0) return PI / 2;
    if (y < 0) return -PI / 2;
    return 0.0;
}

double asin(double x) {
    if (x < -1 || x > 1) return NAN;
    return atan2(x, sqrt(1 - x * x));
}

double acos(double x) {
    if (x < -1 || x > 1) return NAN;
    return atan2(sqrt(1 - x * x), x);
}

/* ---------------------------------------------------------------- strtod */

static int hexval(int c) {
    if (isdigit(c)) return c - '0';
    c = tolower(c);
    return c >= 'a' && c <= 'f' ? c - 'a' + 10 : -1;
}

static double pow10i(int e) {
    double r = 1, b = 10;
    int n = e < 0 ? -e : e;
    while (n) {
        if (n & 1) r *= b;
        b *= b;
        n >>= 1;
    }
    return e < 0 ? 1 / r : r;
}

/* Reads an optional sign and a decimal exponent; without digits p is left alone. */
static const char *read_exponent(const char *p, int *out) {
    const char *q = p + 1;
    int neg = 0, e = 0;
    if (*q == '+' || *q == '-') neg = *q++ == '-';
    if (!isdigit(*q)) return p;
    while (isdigit(*q)) {
        if (e < 100000) e = e * 10 + (*q - '0');
        q++;
    }
    *out = neg ? -e : e;
    return q;
}

double strtod(const char *s, char **end) {
    const char *p = s;
    while (isspace(*p)) p++;
    int neg = 0;
    if (*p == '+' || *p == '-') neg = *p++ == '-';
    uint64_t m = 0;
    int any = 0;
    double v;
    if (p[0] == '0' && (p[1] == 'x' || p[1] == 'X')) {
        p += 2;
        int e2 = 0, d;
        for (; (d = hexval(*p)) >= 0; p++, any = 1) {
            if (m < (1ULL << 59)) m = m * 16 + d;
            else e2 += 4;
        }
        if (*p == '.') {
            for (p++; (d = hexval(*p)) >= 0; p++, any = 1) {
                if (m < (1ULL << 59)) {
                    m = m * 16 + d;
                    e2 -= 4;
                }
            }
        }
        if (!any) goto fail;
        if (*p == 'p' || *p == 'P') {
            int e = 0;
            p = read_exponent(p, &e);
            e2 += e;
        }
        v = ldexp((double)m, e2);
    } else {
        int e10 = 0;
        for (; isdigit(*p); p++, any = 1) {
            if (m < 1000000000000000000ULL) m = m * 10 + (*p - '0');
            else e10++;
        }
        if (*p == '.') {
            for (p++; isdigit(*p); p++, any = 1) {
                if (m < 1000000000000000000ULL) {
                    m = m * 10 + (*p - '0');
                    e10--;
                }
            }
        }
        if (!any) goto fail;
        if (*p == 'e' || *p == 'E') {
            int e = 0;
            p = read_exponent(p, &e);
            e10 += e;
        }
        v = (double)m;
        if (e10 > 0) v *= pow10i(e10 > 400 ? 400 : e10);
        else if (e10 < 0) {
            /* divide in two steps so 10^e10 does not overflow too early */
            int e = -e10 > 700 ? 700 : -e10;
            if (e > 300) {
                v /= pow10i(300);
                e -= 300;
            }
            v /= pow10i(e);
        }
    }
    if (end) *end = (char *)p;
    return neg ? -v : v;
fail:
    if (end) *end = (char *)s;
    return 0;
}
