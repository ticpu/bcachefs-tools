#ifndef _LINUX_SORT_H
#define _LINUX_SORT_H

#include <stdlib.h>
#include <linux/types.h>

void sort_r(void *base, size_t num, size_t size,
	    cmp_r_func_t cmp_func,
	    swap_r_func_t swap_func,
	    const void *priv);

static inline void sort(void *base, size_t num, size_t size,
			int (*cmp_func)(const void *, const void *),
			void (*swap_func)(void *, void *, int size))
{
	return qsort(base, num, size, cmp_func);
}

#define sort_nonatomic(...)	sort(__VA_ARGS__)

#endif
