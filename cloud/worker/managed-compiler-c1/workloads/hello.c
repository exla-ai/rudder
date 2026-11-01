#include <stdio.h>
#include <stdlib.h>
int c1_add(int a, int b) { return a + b; }
#ifndef C1_LIBRARY_ONLY
int main(void) { puts("c1-c-ok"); return c1_add(19, 23) == 42 ? EXIT_SUCCESS : EXIT_FAILURE; }
#endif
