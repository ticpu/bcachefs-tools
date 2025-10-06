#ifndef _BCACHEFS_TOOLS_RUST_TO_C_H
#define _BCACHEFS_TOOLS_RUST_TO_C_H

#include "init/dev_types.h"
#include "util/darray.h"

struct sb_name {
	const char		*name;
	struct bch_sb_handle	sb;
};
typedef DARRAY(struct sb_name) sb_names;

int bch2_scan_device_sbs(char *, sb_names *ret);

char *bch2_scan_devices(char *);

#endif /* _BCACHEFS_TOOLS_RUST_TO_C_H */
