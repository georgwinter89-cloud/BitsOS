/* The part of libcap's <sys/capability.h> that bubblewrap uses (cap_from_name, cap_value_t, capget/capset),
 * written for BitsOS V2's static musl build so the port needs no libcap download. musl provides capget/capset;
 * the constants come from the kernel header. */
#ifndef V2_SYS_CAPABILITY_H
#define V2_SYS_CAPABILITY_H
#include <linux/capability.h>

typedef int cap_value_t;

/* 0 and *value set for a known name ("cap_chown", any case) or a decimal number up to CAP_LAST_CAP; -1 otherwise. */
int cap_from_name(const char *name, cap_value_t *value);

int capget(cap_user_header_t header, cap_user_data_t data);
int capset(cap_user_header_t header, const cap_user_data_t data);
#endif
