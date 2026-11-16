#pragma once
#include <stddef.h>
#include <stdarg.h>
typedef struct myos_file FILE;
extern FILE *stdin, *stdout, *stderr;
#define EOF (-1)
#define BUFSIZ 512
#define SEEK_SET 0
#define SEEK_CUR 1
#define SEEK_END 2
FILE *fopen(const char *path, const char *mode);
FILE *freopen(const char *path, const char *mode, FILE *f);
int fclose(FILE *f);
size_t fread(void *p, size_t size, size_t n, FILE *f);
size_t fwrite(const void *p, size_t size, size_t n, FILE *f);
int fputs(const char *s, FILE *f);
int fputc(int c, FILE *f);
int fprintf(FILE *f, const char *fmt, ...);
int vfprintf(FILE *f, const char *fmt, va_list ap);
int printf(const char *fmt, ...);
int sprintf(char *buf, const char *fmt, ...);
int snprintf(char *buf, size_t n, const char *fmt, ...);
int vsnprintf(char *buf, size_t n, const char *fmt, va_list ap);
int getc(FILE *f);
int ungetc(int c, FILE *f);
int feof(FILE *f);
int ferror(FILE *f);
int fflush(FILE *f);
void clearerr(FILE *f);
