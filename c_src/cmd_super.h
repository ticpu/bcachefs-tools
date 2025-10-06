#ifndef _TOOLS_CMD_SHOW_SUPER_H
#define _TOOLS_CMD_SHOW_SUPER_H

#include "sb/io.h"

void bch2_sb_to_text_with_names(struct printbuf *, struct bch_sb *, bool, unsigned, int);

#endif /* _TOOLS_CMD_SHOW_SUPER_H */
