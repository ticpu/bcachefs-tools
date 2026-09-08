/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_COMPAT_H
#define _BCACHEFS_COMPAT_H

#include <crypto/chacha.h>
#include <linux/bio.h>
#include <linux/cleanup.h>
#include <linux/highmem.h>
#include <linux/idr.h>
#include <linux/mm.h>
#include <linux/percpu-rwsem.h>
#include <linux/sort.h>
#include <linux/string.h>
#include <linux/version.h>

/*
 * Kernel APIs bcachefs uses unconditionally that predate its 6.16 floor.
 * Building against an older kernel needs them supplied here.
 */
#if defined(__KERNEL__) && LINUX_VERSION_CODE < KERNEL_VERSION(6,18,0)
#define BCH_KERNEL_PRE_6_18	1
#endif

#ifdef BCH_KERNEL_PRE_6_18

#ifndef cmp_int
#define cmp_int(l, r)		(((l) > (r)) - ((l) < (r)))
#endif

#ifndef TYPEOF_UNQUAL
#define TYPEOF_UNQUAL(exp)	__typeof__(exp)
#endif

/*
 * Same signatures as sort()/sort_r(), which on these kernels are what these
 * call sites used: the _nonatomic variants only add periodic cond_resched().
 */
#define sort_nonatomic		sort
#define sort_r_nonatomic	sort_r

/*
 * 6.18 wrapped chacha's bare u32[CHACHA_STATE_WORDS] in a struct. Same
 * storage, so wrap the older calls rather than touching the call sites.
 */
struct chacha_state {
	u32 x[CHACHA_STATE_WORDS];
};

#define chacha_init(state, key, iv)	chacha_init((state)->x, key, iv)
#define chacha20_crypt(state, dst, src, bytes)				\
	chacha20_crypt((state)->x, dst, src, bytes)

static inline void chacha_zeroize_state(struct chacha_state *state)
{
	memzero_explicit(state, sizeof(*state));
}

/* The lock ops themselves are all here; only these wrappers came later. */
DEFINE_GUARD(percpu_read, struct percpu_rw_semaphore *,
	     percpu_down_read(_T), percpu_up_read(_T))
DEFINE_GUARD_COND(percpu_read, _try, percpu_down_read_trylock(_T))

/*
 * Only used to assert an ida is drained, which ida_is_empty() answers; the
 * id it would return is not looked at. Returning -1 keeps ">= 0" meaning
 * "not empty" at the call site.
 */
static inline int ida_find_first(struct ida *ida)
{
	return ida_is_empty(ida) ? -1 : 0;
}

/*
 * Ported from 6.18 block/bio.c: vmalloc memory is not physically
 * contiguous, so it goes in a page at a time. Reads still need the caller's
 * invalidate_kernel_vmap_range() in the completion handler.
 */
static inline unsigned int bio_add_vmalloc_chunk(struct bio *bio, void *vaddr,
						 unsigned int len)
{
	unsigned int offset = offset_in_page(vaddr);

	len = min(len, (unsigned int)(PAGE_SIZE - offset));
	if (bio_add_page(bio, vmalloc_to_page(vaddr), len, offset) < len)
		return 0;
	if (op_is_write(bio_op(bio)))
		flush_kernel_vmap_range(vaddr, len);
	return len;
}

static inline bool bio_add_vmalloc(struct bio *bio, void *vaddr,
				   unsigned int len)
{
	do {
		unsigned int added = bio_add_vmalloc_chunk(bio, vaddr, len);

		if (!added)
			return false;
		vaddr += added;
		len -= added;
	} while (len);

	return true;
}

#if LINUX_VERSION_CODE < KERNEL_VERSION(6,13,0) && IS_ENABLED(CONFIG_UNICODE)
#include <linux/fs.h>
#include <linux/unicode.h>

/*
 * Ported from 6.13 fs/libfs.c, which exported them; before that they were
 * static, with identical bodies, behind generic_set_sb_d_ops().
 */
static inline int generic_ci_d_compare(const struct dentry *dentry,
				       unsigned int len, const char *str,
				       const struct qstr *name)
{
	const struct dentry *parent;
	const struct inode *dir;
	char strbuf[DNAME_INLINE_LEN];
	struct qstr qstr;

	if (len == name->len && !memcmp(str, name->name, len))
		return 0;

	parent = READ_ONCE(dentry->d_parent);
	dir = READ_ONCE(parent->d_inode);
	if (!dir || !IS_CASEFOLDED(dir))
		return 1;

	/*
	 * An inline name can change under a concurrent rename, and
	 * utf8_strncasecmp() must not see an unstable string.
	 */
	if (len <= DNAME_INLINE_LEN - 1) {
		memcpy(strbuf, str, len);
		strbuf[len] = 0;
		str = strbuf;
		/* prevent compiler from optimizing out the temporary buffer */
		barrier();
	}
	qstr.len = len;
	qstr.name = str;

	return utf8_strncasecmp(dentry->d_sb->s_encoding, name, &qstr);
}

static inline int generic_ci_d_hash(const struct dentry *dentry,
				    struct qstr *str)
{
	const struct inode *dir = READ_ONCE(dentry->d_inode);
	struct super_block *sb = dentry->d_sb;
	const struct unicode_map *um = sb->s_encoding;
	int ret;

	if (!dir || !IS_CASEFOLDED(dir))
		return 0;

	ret = utf8_casefold_hash(um, dentry, str);
	if (ret < 0 && sb_has_strict_encoding(sb))
		return -EINVAL;
	return 0;
}
#endif

/*
 * The filldir64 fastpath reads dir_context.count, which does not exist
 * here. Not a layout that could be verified - the field is absent - so the
 * fastpath is compiled out and readdir takes the unlock path.
 */
#define BCH_NO_FILLDIR64_FASTPATH	1

/* This VFS has no large-block-size support to opt into. */
#define FS_LBS				0

/* sysfs constified the bin_attribute passed to ->read/->write in 6.18. */
#define BCH_BIN_ATTR_CONST

#else

#define BCH_BIN_ATTR_CONST		const

#endif

#endif /* _BCACHEFS_COMPAT_H */
