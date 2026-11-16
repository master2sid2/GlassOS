#pragma once
/* Our own setjmp/longjmp (System V ABI). The names are prefixed so clang does not replace
   the call with the built-in _setjmp. */
typedef unsigned long long jmp_buf[32];
int myos_setjmp(jmp_buf env) __attribute__((returns_twice));
void myos_longjmp(jmp_buf env, int val) __attribute__((noreturn));
#define setjmp(env) myos_setjmp(env)
#define longjmp(env, val) myos_longjmp(env, val)
