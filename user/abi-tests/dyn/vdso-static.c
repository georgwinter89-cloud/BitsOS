/* AB-14: the vDSO through glibc linked statically (static-PIE: glibc finds the vDSO itself, without ld.so);
 * the program is c/vdsotest.h. */
#include "vdsotest.h"
