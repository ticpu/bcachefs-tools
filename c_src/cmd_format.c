/*
 * Authors: Kent Overstreet <kent.overstreet@gmail.com>
 *	    Gabriel de Perthuis <g2p.code@gmail.com>
 *	    Jacob Malevich <jam@datera.io>
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
#include "posix_to_bcachefs.h"
#include "libbcachefs.h"
#include "crypto.h"
#include "libbcachefs/errcode.h"
#include "libbcachefs/opts.h"
#include "libbcachefs/super-io.h"
#include "libbcachefs/util.h"

#include "libbcachefs/darray.h"

#define OPTS						\
x(0,	replicas,		required_argument)	\
x(0,	encrypted,		no_argument)		\
x(0,	no_passphrase,		no_argument)		\
x('L',	fs_label,		required_argument)	\
x('U',	uuid,			required_argument)	\
x(0,	fs_size,		required_argument)	\
x(0,	superblock_size,	required_argument)	\
x('l',	label,			required_argument)	\
x(0,	version,		required_argument)	\
x(0,	no_initialize,		no_argument)		\
x(0,	source,			required_argument)	\
x('f',	force,			no_argument)		\
x('q',	quiet,			no_argument)		\
x('v',	verbose,		no_argument)		\
x('h',	help,			no_argument)

static void format_usage(void)
{
	puts("bcachefs format - create a new bcachefs filesystem on one or more devices\n"
	     "Usage: bcachefs format [OPTION]... <devices>\n"
	     "\n"
	     "Options:");

	bch2_opts_usage(OPT_FORMAT|OPT_FS);

	puts("      --replicas=#            Sets both data and metadata replicas\n"
	     "      --encrypted             Enable whole filesystem encryption (chacha20/poly1305)\n"
	     "      --no_passphrase         Don't encrypt master encryption key\n"
	     "  -L, --fs_label=label\n"
	     "  -U, --uuid=uuid\n"
	     "      --superblock_size=size\n"
	     "      --source=path           Initialize the bcachefs filesystem from this root directory\n"
	     "\n"
	     "Device specific options:");

	bch2_opts_usage(OPT_FORMAT|OPT_DEVICE);

	puts("      --fs_size=size          Size of filesystem on device\n"
	     "  -l, --label=label           Disk label\n"
	     "\n"
	     "  -f, --force\n"
	     "  -q, --quiet                 Only print errors\n"
	     "  -v, --verbose               Verbose filesystem initialization\n"
	     "  -h, --help                  Display this help and exit\n"
	     "\n"
	     "Device specific options must come before corresponding devices, e.g.\n"
	     "  bcachefs format --label cache /dev/sdb /dev/sdc\n"
	     "\n"
	     "Report bugs to <linux-bcachefs@vger.kernel.org>");
}

enum {
	O_no_opt = 1,
#define x(shortopt, longopt, arg)	O_##longopt,
	OPTS
#undef x
};

#define x(shortopt, longopt, arg) {			\
	.name		= #longopt,			\
	.has_arg	= arg,				\
	.flag		= NULL,				\
	.val		= O_##longopt,			\
},
static const struct option format_opts[] = {
	OPTS
	{ NULL }
};
#undef x

u64 read_flag_list_or_die(char *opt, const char * const list[],
			  const char *msg)
{
	u64 v = bch2_read_flag_list(opt, list);
	if (v == (u64) -1)
		die("Bad %s %s", msg, opt);

	return v;
}

static void build_fs(struct bch_fs *c, const char *src_path)
{
	struct copy_fs_state s = {};
	int src_fd = xopen(src_path, O_RDONLY|O_NOATIME);
	struct stat stat = xfstat(src_fd);

	if (!S_ISDIR(stat.st_mode))
		die("%s is not a directory", src_path);

	copy_fs(c, src_fd, src_path, &s, 0);
}

int cmd_format(int argc, char *argv[])
{
	dev_opts_list devices = {};
	darray_str device_paths = {};
	struct format_opts opts	= format_opts_default();
	struct dev_opts dev_opts = dev_opts_default();
	bool force = false, no_passphrase = false, quiet = false, initialize = true, verbose = false;
	bool unconsumed_dev_option = false;
	unsigned v;

	struct bch_opt_strs fs_opt_strs = {};
	struct bch_opts fs_opts = bch2_opts_empty();

	if (getenv("BCACHEFS_KERNEL_ONLY"))
		initialize = false;

	while (true) {
		const struct bch_option *opt =
			bch2_cmdline_opt_parse(argc, argv, OPT_FORMAT|OPT_FS|OPT_DEVICE);
		if (opt) {
			unsigned id = opt - bch2_opt_table;
			u64 v;
			struct printbuf err = PRINTBUF;
			int ret = bch2_opt_parse(NULL, opt, optarg, &v, &err);
			if (ret == -BCH_ERR_option_needs_open_fs) {
				fs_opt_strs.by_id[id] = strdup(optarg);
				continue;
			}
			if (ret)
				die("invalid option: %s", err.buf);

			if (opt->flags & OPT_DEVICE) {
				bch2_opt_set_by_id(&dev_opts.opts, id, v);
				unconsumed_dev_option = true;
			} else if (opt->flags & OPT_FS) {
				bch2_opt_set_by_id(&fs_opts, id, v);
			} else {
				die("got bch_opt of wrong type %s", opt->attr.name);
			}

			continue;
		}

		int optid = getopt_long(argc, argv,
					"-L:l:U:g:fqhv",
					format_opts,
					NULL);
		if (optid == -1)
			break;

		switch (optid) {
		case O_replicas:
			if (kstrtouint(optarg, 10, &v) ||
			    !v ||
			    v > BCH_REPLICAS_MAX)
				die("invalid replicas");

			opt_set(fs_opts, metadata_replicas, v);
			opt_set(fs_opts, data_replicas, v);
			break;
		case O_source:
			opts.source = optarg;
			break;
		case O_encrypted:
			opts.encrypted = true;
			break;
		case O_no_passphrase:
			no_passphrase = true;
			break;
		case O_fs_label:
		case 'L':
			opts.label = optarg;
			break;
		case O_uuid:
		case 'U':
			if (uuid_parse(optarg, opts.uuid.b))
				die("Bad uuid");
			break;
		case O_force:
		case 'f':
			force = true;
			break;
		case O_fs_size:
			if (bch2_strtoull_h(optarg, &dev_opts.fs_size))
				die("invalid filesystem size");
			unconsumed_dev_option = true;
			break;
		case O_superblock_size:
			if (bch2_strtouint_h(optarg, &opts.superblock_size))
				die("invalid filesystem size");

			opts.superblock_size >>= 9;
			break;
		case O_label:
		case 'l':
			dev_opts.label = optarg;
			unconsumed_dev_option = true;
			break;
		case O_version:
			opts.version = version_parse(optarg);
			break;
		case O_no_initialize:
			initialize = false;
			break;
		case O_no_opt:
			darray_push(&device_paths, optarg);
			dev_opts.path = optarg;
			darray_push(&devices, dev_opts);
			dev_opts.fs_size = 0;
			unconsumed_dev_option = false;
			break;
		case O_quiet:
		case 'q':
			quiet = true;
			break;
		case 'v':
			verbose = true;
			break;
		case O_help:
		case 'h':
			format_usage();
			exit(EXIT_SUCCESS);
			break;
		case '?':
			exit(EXIT_FAILURE);
			break;
		default:
			die("getopt ret %i %c", optid, optid);
		}
	}

	if (unconsumed_dev_option)
		die("Options for devices apply to subsequent devices; got a device option with no device");

	if (opts.version != bcachefs_metadata_version_current)
		initialize = false;

	if (!devices.nr)
		die("Please supply a device");

	if (opts.encrypted && !no_passphrase) {
		opts.passphrase = read_passphrase_twice("Enter passphrase: ");
		initialize = false;
	}

	darray_for_each(devices, dev) {
		int ret = open_for_format(dev, force);
		if (ret)
			die("Error opening %s: %s", dev_opts.path, strerror(-ret));
	}

	struct bch_sb *sb = bch2_format(fs_opt_strs, fs_opts, opts, devices);
	bch2_opt_strs_free(&fs_opt_strs);

	if (!quiet) {
		struct printbuf buf = PRINTBUF;

		buf.human_readable_units = true;

		bch2_sb_to_text(&buf, sb, false, 1 << BCH_SB_FIELD_members_v2);
		printf("%s", buf.buf);

		printbuf_exit(&buf);
	}
	free(sb);

	if (opts.passphrase) {
		memzero_explicit(opts.passphrase, strlen(opts.passphrase));
		free(opts.passphrase);
	}

	darray_exit(&devices);

	/* don't skip initialization when we have to build an image from a source */
	if (opts.source && !initialize) {
		printf("Warning: Forcing the initialization because the source flag was supplied\n");
		initialize = 1;
	}

	if (initialize) {
		struct bch_opts mount_opts = bch2_opts_empty();


		opt_set(mount_opts, verbose, verbose);

		/*
		 * Start the filesystem once, to allocate the journal and create
		 * the root directory:
		 */
		struct bch_fs *c = bch2_fs_open(device_paths.data,
						device_paths.nr,
						mount_opts);
		if (IS_ERR(c))
			die("error opening %s: %s", device_paths.data[0],
			    bch2_err_str(PTR_ERR(c)));

		if (opts.source)
			build_fs(c, opts.source);

		bch2_fs_stop(c);
	}

	darray_exit(&device_paths);

	return 0;
}

static void show_super_usage(void)
{
	puts("bcachefs show-super \n"
	     "Usage: bcachefs show-super [OPTION].. device\n"
	     "\n"
	     "Options:\n"
	     "  -f, --fields=(fields)       list of sections to print\n"
	     "      --field-only=fiel)      print superblock section only, no header\n"
	     "  -l, --layout                print superblock layout\n"
	     "  -h, --help                  display this help and exit\n"
	     "Report bugs to <linux-bcachefs@vger.kernel.org>");
	exit(EXIT_SUCCESS);
}

int cmd_show_super(int argc, char *argv[])
{
	static const struct option longopts[] = {
		{ "fields",			1, NULL, 'f' },
		{ "field-only",			1, NULL, 'F' },
		{ "layout",			0, NULL, 'l' },
		{ "help",			0, NULL, 'h' },
		{ NULL }
	};
	unsigned fields = 0;
	int field_only = -1;
	bool print_layout = false;
	bool print_default_fields = true;
	int opt;

	while ((opt = getopt_long(argc, argv, "f:lh", longopts, NULL)) != -1)
		switch (opt) {
		case 'f':
			fields = !strcmp(optarg, "all")
				? ~0
				: read_flag_list_or_die(optarg,
					bch2_sb_fields, "superblock field");
			print_default_fields = false;
			break;
		case 'F':
			field_only = read_string_list_or_die(optarg,
					bch2_sb_fields, "superblock field");
			print_default_fields = false;
			break;
		case 'l':
			print_layout = true;
			break;
		case 'h':
			show_super_usage();
			break;
		}
	args_shift(optind);

	char *dev = arg_pop();
	if (!dev)
		die("please supply a device");
	if (argc)
		die("too many arguments");

	struct bch_opts opts = bch2_opts_empty();

	opt_set(opts, noexcl,	true);
	opt_set(opts, nochanges, true);

	struct bch_sb_handle sb;
	int ret = bch2_read_super(dev, &opts, &sb);
	if (ret)
		die("Error opening %s: %s", dev, bch2_err_str(ret));

	if (print_default_fields) {
		fields |= bch2_sb_field_get(sb.sb, members_v2)
			? 1 << BCH_SB_FIELD_members_v2
			: 1 << BCH_SB_FIELD_members_v1;
		fields |= 1 << BCH_SB_FIELD_errors;
	}

	struct printbuf buf = PRINTBUF;

	buf.human_readable_units = true;

	if (field_only >= 0) {
		struct bch_sb_field *f = bch2_sb_field_get_id(sb.sb, field_only);

		if (f)
			__bch2_sb_field_to_text(&buf, sb.sb, f);
	} else {
		printbuf_tabstop_push(&buf, 44);

		char *model = fd_to_dev_model(sb.bdev->bd_fd);
		prt_str(&buf, "Device:");
		prt_tab(&buf);
		prt_str(&buf, model);
		prt_newline(&buf);
		free(model);

		bch2_sb_to_text(&buf, sb.sb, print_layout, fields);
	}
	printf("%s", buf.buf);

	bch2_free_super(&sb);
	printbuf_exit(&buf);
	return 0;
}

#include "libbcachefs/super-io.h"
#include "libbcachefs/sb-members.h"

typedef DARRAY(struct bch_sb *) probed_sb_list;

static void probe_one_super(int dev_fd, unsigned sb_size, u64 offset,
			    probed_sb_list *sbs, bool verbose)
{
	darray_char sb_buf = {};
	darray_resize(&sb_buf, sb_size);

	xpread(dev_fd, sb_buf.data, sb_buf.size, offset);

	struct printbuf err = PRINTBUF;
	int ret = bch2_sb_validate((void *) sb_buf.data, offset >> 9, 0, &err);
	printbuf_exit(&err);

	if (!ret) {
		if (verbose) {
			struct printbuf buf = PRINTBUF;
			prt_human_readable_u64(&buf, offset);
			printf("found superblock at %s\n", buf.buf);
			printbuf_exit(&buf);
		}

		darray_push(sbs, (void *) sb_buf.data);
		sb_buf.data = NULL;
	}

	darray_exit(&sb_buf);
}

static void probe_sb_range(int dev_fd, u64 start_offset, u64 end_offset,
			   probed_sb_list *sbs, bool verbose)
{
	start_offset	&= ~((u64) 511);
	end_offset	&= ~((u64) 511);

	size_t buflen = end_offset - start_offset;
	void *buf = malloc(buflen);
	xpread(dev_fd, buf, buflen, start_offset);

	for (u64 offset = 0; offset < buflen; offset += 512) {
		struct bch_sb *sb = buf + offset;

		if (!uuid_equal(&sb->magic, &BCACHE_MAGIC) &&
		    !uuid_equal(&sb->magic, &BCHFS_MAGIC))
			continue;

		size_t bytes = vstruct_bytes(sb);
		if (offset + bytes > buflen) {
			fprintf(stderr, "found sb %llu size %zu that overran buffer\n",
				start_offset + offset, bytes);
			continue;
		}
		struct printbuf err = PRINTBUF;
		int ret = bch2_sb_validate(sb, (start_offset + offset) >> 9, 0, &err);
		if (ret)
			fprintf(stderr, "found sb %llu that failed to validate: %s\n",
				start_offset + offset, err.buf);
		printbuf_exit(&err);

		if (ret)
			continue;

		if (verbose) {
			struct printbuf buf = PRINTBUF;
			prt_human_readable_u64(&buf, start_offset + offset);
			printf("found superblock at %s\n", buf.buf);
			printbuf_exit(&buf);
		}

		void *sb_copy = malloc(bytes);
		memcpy(sb_copy, sb, bytes);
		darray_push(sbs, sb_copy);
	}

	free(buf);
}

static u64 bch2_sb_last_mount_time(struct bch_sb *sb)
{
	u64 ret = 0;
	for (unsigned i = 0; i < sb->nr_devices; i++)
		ret = max(ret, le64_to_cpu(bch2_sb_member_get(sb, i).last_mount));
	return ret;
}

static int bch2_sb_time_cmp(struct bch_sb *l, struct bch_sb *r)
{
	return cmp_int(bch2_sb_last_mount_time(l),
		       bch2_sb_last_mount_time(r));
}

static void recover_super_usage(void)
{
	puts("bcachefs recover-super \n"
	     "Usage: bcachefs recover-super [OPTION].. device\n"
	     "\n"
	     "Attempt to recover a filesystem on a device that has had the main superblock\n"
	     "and superblock layout overwritten.\n"
	     "All options will be guessed if not provided\n"
	     "\n"
	     "Options:\n"
	     "  -d, --dev_size              size of filessytem on device, in bytes \n"
	     "  -o, --offset                offset to probe, in bytes\n"
	     "  -y, --yes                   Recover without prompting\n"
	     "  -v, --verbose               Increase logging level\n"
	     "  -h, --help                  display this help and exit\n"
	     "Report bugs to <linux-bcachefs@vger.kernel.org>");
	exit(EXIT_SUCCESS);
}

int cmd_recover_super(int argc, char *argv[])
{
	static const struct option longopts[] = {
		{ "dev_size",			1, NULL, 'd' },
		{ "offset",			1, NULL, 'o' },
		{ "yes",			0, NULL, 'y' },
		{ "verbose",			0, NULL, 'v' },
		{ "help",			0, NULL, 'h' },
		{ NULL }
	};
	u64 dev_size = 0, offset = 0;
	bool yes = false, verbose = false;
	int opt;

	while ((opt = getopt_long(argc, argv, "d:o:yvh", longopts, NULL)) != -1)
		switch (opt) {
		case 'd':
			if (bch2_strtoull_h(optarg, &dev_size))
				die("invalid offset");
			break;
		case 'o':
			if (bch2_strtoull_h(optarg, &offset))
				die("invalid offset");

			if (offset & 511)
				die("offset must be a multiple of 512");
			break;
		case 'y':
			yes = true;
			break;
		case 'v':
			verbose = true;
			break;
		case 'h':
			recover_super_usage();
			break;
		}
	args_shift(optind);

	char *dev_path = arg_pop();
	if (!dev_path)
		die("please supply a device");
	if (argc)
		die("too many arguments");

	int dev_fd = xopen(dev_path, O_RDWR);

	if (!dev_size)
		dev_size = get_size(dev_fd);

	probed_sb_list sbs = {};

	if (offset) {
		probe_one_super(dev_fd, SUPERBLOCK_SIZE_DEFAULT, offset, &sbs, verbose);
	} else {
		unsigned scan_len = 16 << 20; /* 16MB, start and end of device */

		probe_sb_range(dev_fd, 4096, scan_len, &sbs, verbose);
		probe_sb_range(dev_fd, dev_size - scan_len, dev_size, &sbs, verbose);
	}

	if (!sbs.nr) {
		printf("Found no bcachefs superblocks\n");
		exit(EXIT_FAILURE);
	}

	struct bch_sb *best = NULL;
	darray_for_each(sbs, sb)
		if (!best || bch2_sb_time_cmp(best, *sb) < 0)
			best = *sb;

	struct printbuf buf = PRINTBUF;
	bch2_sb_to_text(&buf, best, true, BIT_ULL(BCH_SB_FIELD_members_v2));

	printf("Found superblock:\n%s", buf.buf);
	printf("Recover?");

	if (yes || ask_yn())
		bch2_super_write(dev_fd, best);

	printbuf_exit(&buf);
	darray_for_each(sbs, sb)
		kfree(*sb);
	darray_exit(&sbs);

	return 0;
}
