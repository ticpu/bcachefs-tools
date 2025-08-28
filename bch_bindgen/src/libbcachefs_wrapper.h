#include "libbcachefs/super-io.h"
#include "libbcachefs/checksum.h"
#include "libbcachefs/bcachefs_format.h"
#include "libbcachefs/btree_cache.h"
#include "libbcachefs/btree_iter.h"
#include "libbcachefs/debug.h"
#include "libbcachefs/errcode.h"
#include "libbcachefs/error.h"
#include "libbcachefs/opts.h"
#include "libbcachefs.h"
#include "crypto.h"
#include "include/linux/bio.h"
#include "include/linux/blkdev.h"
#include "cmds.h"
#include "raid/raid.h"
#include "src/rust_to_c.h"

/* Fix753 is a workaround for https://github.com/rust-lang/rust-bindgen/issues/753
 * Functional macro are not expanded with bindgen, e.g. ioctl are automatically ignored
 * from the generation
 *
 * To avoid this, use `MARK_FIX_753` to force the synthesis of your macro constant.
 * It will appear in Rust with its proper name and not Fix753_{name}.
 */

/* MARK_FIX_753: force generate a macro constant in Rust
 *
 * @type_name   - a type for this constant
 * @req_name    - a name for this constant which will be used inside of Rust
 */
#define MARK_FIX_753(type_name, req_name) const type_name Fix753_##req_name = req_name;

MARK_FIX_753(blk_mode_t, BLK_OPEN_READ);
MARK_FIX_753(blk_mode_t, BLK_OPEN_WRITE);
MARK_FIX_753(blk_mode_t, BLK_OPEN_EXCL);

MARK_FIX_753(__u32, BCH_IOCTL_SUBVOLUME_CREATE);
MARK_FIX_753(__u32, BCH_IOCTL_SUBVOLUME_DESTROY);

MARK_FIX_753(__u32, BCH_IOCTL_SUBVOLUME_CREATE_v2);
MARK_FIX_753(__u32, BCH_IOCTL_SUBVOLUME_DESTROY_v2);
