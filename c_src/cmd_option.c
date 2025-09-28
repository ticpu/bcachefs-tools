/*
 * Authors: Kent Overstreet <kent.overstreet@gmail.com>
 *
 * GPLv2
 */
#include <ctype.h>
#include <errno.h>
#include <fcntl.h>
#include <getopt.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/types.h>
#include <unistd.h>

#include <uuid/uuid.h>

#include "cmds.h"
#include "libbcachefs.h"
#include "libbcachefs/errcode.h"
#include "libbcachefs/opts.h"
#include "libbcachefs/super-io.h"

static void set_option_usage(void)
{
	puts("bcachefs set-fs-option \n"
	     "Usage: bcachefs set-fs-option [OPTION].. device\n"
	     "\n"
	     "Options:\n");
	bch2_opts_usage(OPT_MOUNT|OPT_RUNTIME);
	puts("  -d, --dev-idx               index for device specific options\n"
	     "  -h, --help                  display this help and exit\n"
	     "Report bugs to <linux-bcachefs@vger.kernel.org>");
	exit(EXIT_SUCCESS);
}

static int name_to_dev_idx(struct bch_fs *c, const char *dev)
{
	int ret = -1;

	rcu_read_lock();
	for_each_member_device_rcu(c, ca, NULL)
		if (!strcmp(ca->name, dev)) {
			ret = ca->dev_idx;
			break;
		}
	rcu_read_unlock();

	return ret;
}

int cmd_set_option(int argc, char *argv[])
{
	struct bch_opt_strs new_opt_strs = bch2_cmdline_opts_get(&argc, argv, OPT_FS|OPT_DEVICE);
	struct bch_opts new_opts = bch2_parse_opts(new_opt_strs);
	DARRAY(unsigned) dev_idxs = {};
	int opt, ret = 0;

	static const struct option longopts[] = {
		{ "dev-idx",			required_argument,	NULL, 'd' },
		{ "help",			no_argument,		NULL, 'h' },
		{ NULL }
	};

	while ((opt = getopt_long(argc, argv, "d:h", longopts, NULL)) != -1)
		switch (opt) {
		case 'd': {
			unsigned dev_idx;
			if (kstrtoint(optarg, 10, &dev_idx))
				die("error parsing %s", optarg);
			darray_push(&dev_idxs, dev_idx);
			break;
		}
		case 'h':
			set_option_usage();
			break;
		}
	args_shift(optind);

	if (!argc) {
		fprintf(stderr, "Please supply device(s)\n");
		exit(EXIT_FAILURE);
	}

	bool online = false;
	unsigned i;
	for (i = 0; i < argc; i++)
		if (dev_mounted(argv[i])) {
			online = true;
			break;
		}

	if (!online) {
		darray_const_str devs = get_or_split_cmdline_devs(argc, argv);

		struct bch_opts open_opts = bch2_opts_empty();
		opt_set(open_opts, nostart, true);

		struct bch_fs *c = bch2_fs_open(&devs, &open_opts);
		if (IS_ERR(c)) {
			fprintf(stderr, "error opening %s: %s\n", argv[0], bch2_err_str(PTR_ERR(c)));
			exit(EXIT_FAILURE);
		}

		for (i = 0; i < bch2_opts_nr; i++) {
			const struct bch_option *opt = bch2_opt_table + i;

			u64 v = bch2_opt_get_by_id(&new_opts, i);

			if (!bch2_opt_defined_by_id(&new_opts, i))
				continue;

			if (!(opt->flags & (OPT_FS|OPT_DEVICE)))
				fprintf(stderr, "Can't set option %s\n", opt->attr.name);

			if (opt->flags & OPT_FS) {
				ret = bch2_opt_hook_pre_set(c, NULL, 0, i, v, true);
				if (ret < 0) {
					fprintf(stderr, "error setting %s: %i\n", opt->attr.name, ret);
					continue;
				}

				bch2_opt_set_sb(c, NULL, opt, v);
			}

			if (opt->flags & OPT_DEVICE) {
				if (dev_idxs.nr) {
					darray_for_each(dev_idxs, dev) {
						struct bch_dev *ca = bch2_dev_tryget_noerror(c, *dev);
						if (!ca) {
							fprintf(stderr, "Couldn't look up device %u\n", *dev);
							continue;
						}

						ret = bch2_opt_hook_pre_set(c, ca, 0, i, v, true);
						if (ret < 0) {
							fprintf(stderr, "error setting %s: %i\n", opt->attr.name, ret);
							continue;
						}

						bch2_opt_set_sb(c, ca, opt, v);
						bch2_dev_put(ca);
					}
				} else {
					for (unsigned dev = 0; dev < argc; dev++) {
						int dev_idx = name_to_dev_idx(c, argv[dev]);
						if (dev_idx < 0) {
							fprintf(stderr, "Couldn't look up device %s\n", argv[i]);
							continue;
						}

						bch2_opt_set_sb(c, c->devs[dev_idx], opt, v);
					}
				}
			}
		}

		bch2_fs_stop(c);
		return ret;
	} else {
		unsigned dev_idx;
		struct bchfs_handle fs = bchu_fs_open_by_dev(argv[i], &dev_idx);

		for (i = 0; i < argc; i++) {
			struct bchfs_handle fs2 = bchu_fs_open_by_dev(argv[i], &dev_idx);
			if (memcmp(&fs.uuid, &fs2.uuid, sizeof(fs2.uuid)))
				die("Filesystem mounted, but not all devices are members");
			bcache_fs_close(fs2);
		}

		for (i = 0; i < bch2_opts_nr; i++) {
			if (!new_opt_strs.by_id[i])
				continue;

			const struct bch_option *opt = bch2_opt_table + i;

			if (!(opt->flags & (OPT_FS|OPT_DEVICE)))
				fprintf(stderr, "Can't set option %s\n", opt->attr.name);

			if (opt->flags & OPT_FS) {
				char *path = mprintf("options/%s", opt->attr.name);

				write_file_str(fs.sysfs_fd, path, new_opt_strs.by_id[i]);
				free(path);
			}

			if (opt->flags & OPT_DEVICE) {
				for (unsigned dev = 0; dev < argc; dev++) {
					struct bchfs_handle fs2 = bchu_fs_open_by_dev(argv[i], &dev_idx);
					bcache_fs_close(fs2);


					char *path = mprintf("dev-%u/%s", dev_idx, opt->attr.name);
					write_file_str(fs.sysfs_fd, path, new_opt_strs.by_id[i]);
					free(path);
				}
			}
		}
	}
	return 0;
}
