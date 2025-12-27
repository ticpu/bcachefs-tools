#ifndef _SYSFS_H_
#define _SYSFS_H_

#include <linux/compiler.h>

struct kobject;

struct attribute {
	const char		*name;
	umode_t			mode;
};

struct attribute_group {
	struct attribute	**attrs;
};

struct sysfs_ops {
	ssize_t	(*show)(struct kobject *, struct attribute *, char *);
	ssize_t	(*store)(struct kobject *, struct attribute *, const char *, size_t);
};

int sysfs_create_file(struct kobject *, const struct attribute *);

static inline int sysfs_create_link(struct kobject *kobj,
				    struct kobject *target, const char *name)
{
	return 0;
}

static inline void sysfs_remove_link(struct kobject *kobj, const char *name)
{
}

#endif /* _SYSFS_H_ */
