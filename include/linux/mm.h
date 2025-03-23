/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _TOOLS_LINUX_MM_H
#define _TOOLS_LINUX_MM_H

#include <sys/syscall.h>
#include <unistd.h>
#include <linux/bug.h>
#include <linux/types.h>

struct sysinfo {
	long uptime;		/* Seconds since boot */
	unsigned long loads[3];	/* 1, 5, and 15 minute load averages */
	unsigned long totalram;	/* Total usable main memory size */
	unsigned long freeram;	/* Available memory size */
	unsigned long sharedram;	/* Amount of shared memory */
	unsigned long bufferram;	/* Memory used by buffers */
	unsigned long totalswap;	/* Total swap space size */
	unsigned long freeswap;	/* swap space still available */
	__u16 procs;		   	/* Number of current processes */
	__u16 pad;		   	/* Explicit padding for m68k */
	unsigned long totalhigh;	/* Total high memory size */
	unsigned long freehigh;	/* Available high memory size */
	__u32 mem_unit;			/* Memory unit size in bytes */
};



static inline void si_meminfo(struct sysinfo *val)
{
	BUG_ON(syscall(SYS_sysinfo, val));
}

#endif /* _TOOLS_LINUX_MM_H */
