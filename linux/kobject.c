#include <linux/kobject.h>
#include <linux/mutex.h>
#include "../c_src/tools-util.h"

#include "util/printbuf.h"
#include "util/util.h"

static DEFINE_MUTEX(kobj_lock);

static void kobject_cleanup(struct kobject *);

static struct kobject *root_kobj;

void kobject_init(struct kobject *kobj, const struct kobj_type *ktype)
{
	memset(kobj, 0, sizeof(*kobj));

	atomic_set(&kobj->ref, 1);
	kobj->ktype = ktype;
	kobj->state_initialized = 1;
}

int kobject_add(struct kobject *kobj, struct kobject *parent,
		const char *fmt, ...)
{
	va_list args;
	va_start(args, fmt);
	kobj->name = vmprintf(fmt, args);
	va_end(args);

	if (kobj->ktype->default_groups &&
	    kobj->ktype->default_groups[0] &&
	    kobj->ktype->default_groups[0]->attrs) {
		struct attribute **attr = kobj->ktype->default_groups[0]->attrs;

		while (*attr) {
			darray_push(&kobj->files, *attr);
			attr++;
		}
	}

	scoped_guard(mutex, &kobj_lock) {
		if (parent) {
			darray_push(&parent->subdirs, kobj);
			kobj->parent = kobject_get(parent);
		} else {
			BUG_ON(root_kobj);
			root_kobj = kobj;
		}
	}

	kobj->state_in_sysfs = true;

	return 0;
}

void kobject_del(struct kobject *kobj)
{
	if (!kobj)
		return;

	if (kobj->state_in_sysfs) {
		guard(mutex)(&kobj_lock);

		if (kobj->parent) {
			struct kobject **i = darray_find(kobj->parent->subdirs, kobj);
			if (i) {
				darray_remove_item(&kobj->parent->subdirs, i);
			} else {
				WARN(1, "child %s not found in parent %s with %zu subdirs when deleting kobj",
				     kobj->name, kobj->parent->name, kobj->parent->subdirs.nr);
			}
		} else {
			BUG_ON(kobj != root_kobj);
			root_kobj = NULL;
		}
	}

	kobj->state_in_sysfs = false;

	if (kobj->parent)
		kobject_put(kobj->parent);
	kobj->parent = NULL;
}

static void kobject_cleanup(struct kobject *kobj)
{
	const struct kobj_type *t = kobj->ktype;

	/* remove from sysfs if the caller did not do it */
	if (kobj->state_in_sysfs)
		kobject_del(kobj);

	darray_exit(&kobj->files);
	darray_exit(&kobj->subdirs);

	if (t && t->release)
		t->release(kobj);
}

void kobject_put(struct kobject *kobj)
{
	BUG_ON(!kobj);
	BUG_ON(!kobj->state_initialized);

	if (atomic_dec_and_test(&kobj->ref))
		kobject_cleanup(kobj);
}

struct kobject *kobject_get(struct kobject *kobj)
{
	BUG_ON(!kobj);
	BUG_ON(!kobj->state_initialized);

	atomic_inc(&kobj->ref);
	return kobj;
}

int sysfs_create_file(struct kobject *kobj, const struct attribute *attr)
{
	guard(mutex)(&kobj_lock);
	return darray_push(&kobj->files, attr);
}

static bool str_end_eq(const char *s1, const char *s2, const char *s2_end)
{
	return strlen(s1) == s2_end - s2 &&
		!memcmp(s1, s2, s2_end - s2);
}

struct attribute *path_lookup(const char *path, struct kobject **_dir)
{
	struct kobject *dir = root_kobj;
	if (!dir)
		return ERR_PTR(-ENOENT);

	while (true) {
		*_dir = dir;

		while (*path == '/')
			path++;
		if (!*path)
			break;

		const char *end = strchrnul(path, '/');

		if (!*end) {
			struct attribute **attr = (struct attribute **)
				darray_find_p(dir->files, i, str_end_eq((*i)->name, path, end));
			if (attr)
				return *attr;
		}

		struct kobject **child =
			darray_find_p(dir->subdirs, i, str_end_eq((*i)->name, path, end));
		if (!child)
			return ERR_PTR(-ENOENT);

		dir = *child;
		path = end;
	}

	return NULL;
}

int sysfs_read_or_html_dirlist(const char *path, struct printbuf *out)
{
	guard(mutex)(&kobj_lock);

	struct kobject *dir;
	struct attribute *attr = errptr_try(path_lookup(path, &dir));
	if (!attr) {
		prt_str(out, "<html>\n");

		prt_printf(out, "<p> %s </p>", path);

		darray_for_each(dir->subdirs, i)
			prt_printf(out, "<p> <a href=%s/%s>%s/</a></p>\n", path, (*i)->name, (*i)->name);

		prt_printf(out, "<p> %zu subdirs </p>\n", dir->subdirs.nr);

		darray_for_each(dir->files, i)
			prt_printf(out, "<p> <a href=%s/%s>%s</a></p>\n", path, (*i)->name, (*i)->name);

		prt_printf(out, "<p> %zu files </p>\n", dir->files.nr);

		prt_str(out, "</html>\n");
		return 0;
	} else {
		try(bch2_printbuf_make_room(out, PAGE_SIZE));

		int ret = dir->ktype->sysfs_ops->show(dir, attr, out->buf);
		if (ret < 0)
			return ret;
		out->pos = ret;
		return 0;
	}
}

int sysfs_write(const char *path, const char *buf, size_t len)
{
	BUG_ON(len > PAGE_SIZE);

	guard(mutex)(&kobj_lock);

	struct kobject *dir;
	struct attribute *attr = errptr_try(path_lookup(path, &dir));
	if (!attr)
		return -ENOENT;

	return dir->ktype->sysfs_ops->store(dir, attr, buf, len);
}
