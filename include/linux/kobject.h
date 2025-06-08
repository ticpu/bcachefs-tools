/*
 * kobject.h - generic kernel object infrastructure.
 *
 * Copyright (c) 2002-2003 Patrick Mochel
 * Copyright (c) 2002-2003 Open Source Development Labs
 * Copyright (c) 2006-2008 Greg Kroah-Hartman <greg@kroah.com>
 * Copyright (c) 2006-2008 Novell Inc.
 *
 * This file is released under the GPLv2.
 *
 * Please read Documentation/kobject.txt before using the kobject
 * interface, ESPECIALLY the parts about reference counts and object
 * destructors.
 */

#ifndef _KOBJECT_H_
#define _KOBJECT_H_

#include <linux/atomic.h>
#include <linux/bug.h>
#include <linux/compiler.h>
#include <linux/kernel.h>
#include <linux/slab.h>
#include <linux/sysfs.h>
#include <linux/types.h>
#include <linux/workqueue.h>

struct kset;

struct kobj_type {
	void (*release)(struct kobject *kobj);
	const struct sysfs_ops *sysfs_ops;
	const struct attribute_group **default_groups;
	const struct kobj_ns_type_operations *(*child_ns_type)(struct kobject *kobj);
	const void *(*namespace)(struct kobject *kobj);
};

struct kobj_uevent_env {
};

struct kobj_attribute {
	struct attribute attr;
	ssize_t (*show)(struct kobject *kobj, struct kobj_attribute *attr,
			char *buf);
	ssize_t (*store)(struct kobject *kobj, struct kobj_attribute *attr,
			 const char *buf, size_t count);
};

struct kobject {
	struct kobject		*parent;
	struct kset		*kset;
	const struct kobj_type	*ktype;
	struct kernfs_node	*sd; /* sysfs directory entry */
	atomic_t		ref;
	unsigned int state_initialized:1;
	unsigned int state_in_sysfs:1;
	unsigned int state_add_uevent_sent:1;
	unsigned int state_remove_uevent_sent:1;
	unsigned int uevent_suppress:1;
};

struct kset {
	struct kobject		kobj;
};

#define kobject_add(...)	0

static inline void kobject_init(struct kobject *kobj, const struct kobj_type *ktype)
{
	memset(kobj, 0, sizeof(*kobj));

	atomic_set(&kobj->ref, 1);
	kobj->ktype = ktype;
	kobj->state_initialized = 1;
}

static inline void kobject_del(struct kobject *kobj);

static inline void kobject_cleanup(struct kobject *kobj)
{
	const struct kobj_type *t = kobj->ktype;

	/* remove from sysfs if the caller did not do it */
	if (kobj->state_in_sysfs)
		kobject_del(kobj);

	if (t && t->release)
		t->release(kobj);
}

static inline void kobject_put(struct kobject *kobj)
{
	BUG_ON(!kobj);
	BUG_ON(!kobj->state_initialized);

	if (atomic_dec_and_test(&kobj->ref))
		kobject_cleanup(kobj);
}

static inline void kobject_del(struct kobject *kobj)
{
	if (!kobj)
		return;

	kobj->state_in_sysfs = 0;
#if 0
	kobj_kset_leave(kobj);
#endif
	kobject_put(kobj->parent);
	kobj->parent = NULL;
}

static inline struct kobject *kobject_get(struct kobject *kobj)
{
	BUG_ON(!kobj);
	BUG_ON(!kobj->state_initialized);

	atomic_inc(&kobj->ref);
	return kobj;
}

static inline void kset_unregister(struct kset *kset)
{
	kfree(kset);
}

#define kset_create_and_add(_name, _u, _parent)				\
	((struct kset *) kzalloc(sizeof(struct kset), GFP_KERNEL))

enum kobject_action {
	KOBJ_ADD,
	KOBJ_REMOVE,
	KOBJ_CHANGE,
	KOBJ_MOVE,
	KOBJ_ONLINE,
	KOBJ_OFFLINE,
	KOBJ_BIND,
	KOBJ_UNBIND,
};

static inline void kobject_uevent_env(struct kobject *kobj, int flags, char **envp) {}

#endif /* _KOBJECT_H_ */
