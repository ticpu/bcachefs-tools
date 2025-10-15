#include <getopt.h>

#include "cmds.h"
#include "libbcachefs.h"
#include "init/fs.h"
#include "sb/io.h"

static void reset_counters_usage(void)
{
	puts("bcachefs reset-counters \n"
	     "Usage: bcachefs reset-counters device\n"
	     "\n"
	     "Options:\n"
	     "  -h, --help                  display this help and exit\n"
	     "\n"
	     "Report bugs to <linux-bcachefs@vger.kernel.org>");
	exit(EXIT_SUCCESS);
}

int cmd_reset_counters(int argc, char *argv[])
{
	static const struct option longopts[] = {
		{ "help",			no_argument, NULL, 'h' },
		{ NULL }
	};
	int opt;

	while ((opt = getopt_long(argc, argv, "h", longopts, NULL)) != -1)
		switch (opt) {
		case 'h':
			reset_counters_usage();
			break;
		}
	args_shift(optind);

	char *dev = arg_pop();
	if (!dev) {
		reset_counters_usage();
		die("please supply a device");
	}
	if (argc)
		die("too many arguments");

	/* scan for devices, open full fs */

	struct bch_opts opts = bch2_opts_empty();
	opt_set(opts, nostart,	true);
	opt_set(opts, degraded, BCH_DEGRADED_very);

	darray_const_str devs = get_or_split_cmdline_devs(1, &dev);

	struct bch_fs *c = bch2_fs_open(&devs, &opts);
	int ret = PTR_ERR_OR_ZERO(c);
	if (ret)
		die("Error opening filesystem: %s", bch2_err_str(ret));

	scoped_guard(mutex, &c->sb_lock) {
		bch2_sb_field_resize(&c->disk_sb, counters, 0);
		bch2_write_super(c);
	}
	bch2_fs_stop(c);
	return 0;
}
