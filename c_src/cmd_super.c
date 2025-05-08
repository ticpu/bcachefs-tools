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
#include "libbcachefs.h"
#include "libbcachefs/opts.h"
#include "libbcachefs/super-io.h"
#include "libbcachefs/util.h"

#include "libbcachefs/darray.h"

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
