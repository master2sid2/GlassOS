#pragma once
#include <stddef.h>
#define EXIT_SUCCESS 0
#define EXIT_FAILURE 1
void *malloc(size_t n);
void *calloc(size_t n, size_t size);
void *realloc(void *p, size_t n);
void free(void *p);
void abort(void) __attribute__((noreturn));
void exit(int code) __attribute__((noreturn));
char *getenv(const char *name);
double strtod(const char *s, char **end);
long strtol(const char *s, char **end, int base);
int abs(int x);
