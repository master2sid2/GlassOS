#pragma once
#define HUGE_VAL (__builtin_huge_val())
#define INFINITY (__builtin_inff())
#define NAN (__builtin_nanf(""))
#define isnan(x) __builtin_isnan(x)
#define isinf(x) __builtin_isinf(x)
#define signbit(x) __builtin_signbit(x)
double fabs(double x); double floor(double x); double ceil(double x); double trunc(double x);
double sqrt(double x); double fmod(double x, double y); double pow(double x, double y);
double exp(double x); double log(double x); double log2(double x); double log10(double x);
double sin(double x); double cos(double x); double tan(double x);
double asin(double x); double acos(double x); double atan(double x); double atan2(double y, double x);
double frexp(double x, int *e); double ldexp(double x, int e);
