
#include <getopt.h>
#include <signal.h>
#include <stdio.h>
#include <sys/ioctl.h>
#include <unistd.h>

#include "bcachefs_ioctl.h"
#include "alloc/accounting.h"
#include "btree/cache.h"
#include "data/move.h"

#include "cmds.h"
#include "libbcachefs.h"

/* Obsolete, will be deleted */

static void data_rereplicate_usage(void)
{
	puts("bcachefs data rereplicate\n"
	     "Usage: bcachefs data rereplicate filesystem\n"
	     "\n"
	     "Walks existing data in a filesystem, writing additional copies\n"
	     "of any degraded data\n"
	     "\n"
	     "Options:\n"
	     "  -h, --help                   Display this help and exit\n"
	     "\n"
	     "Report bugs to <linux-bcachefs@vger.kernel.org>");
	exit(EXIT_SUCCESS);
}

static int cmd_data_rereplicate(int argc, char *argv[])
{
	static const struct option longopts[] = {
		{ "help",		no_argument, NULL, 'h' },
		{ NULL }
	};
	int opt;

	while ((opt = getopt_long(argc, argv, "h", longopts, NULL)) != -1)
		switch (opt) {
		case 'h':
			data_rereplicate_usage();
		}
	args_shift(optind);

	if (bcachefs_kernel_version() >= bcachefs_metadata_version_reconcile)
		die("rereplicate no longer required or support >= reconcile; use 'bcachefs reconcile wait'");

	char *fs_path = arg_pop();
	if (!fs_path)
		die("Please supply a filesystem");

	if (argc)
		die("too many arguments");

	return bchu_data(bcache_fs_open(fs_path), (struct bch_ioctl_data) {
		.op		= BCH_DATA_OP_rereplicate,
		.start_btree	= 0,
		.start_pos	= POS_MIN,
		.end_btree	= BTREE_ID_NR,
		.end_pos	= POS_MAX,
	});
}

static void data_job_usage(void)
{
	puts("bcachefs data job\n"
	     "Usage: bcachefs data job [job} filesystem\n"
	     "\n"
	     "Kick off a data job and report progress\n"
	     "\n"
	     "job: one of scrub, rereplicate, migrate, rewrite_old_nodes, or drop_extra_replicas\n"
	     "\n"
	     "Options:\n"
	     "  -b, --btree btree            Btree to operate on\n"
	     "  -s, --start inode:offset     Start position\n"
	     "  -e, --end   inode:offset     End position\n"
	     "  -h, --help                   Display this help and exit\n"
	     "\n"
	     "Report bugs to <linux-bcachefs@vger.kernel.org>");
	exit(EXIT_SUCCESS);
}

static int cmd_data_job(int argc, char *argv[])
{
	static const struct option longopts[] = {
		{ "btree",		required_argument,	NULL, 'b' },
		{ "start",		required_argument,	NULL, 's' },
		{ "end",		required_argument,	NULL, 'e' },
		{ "help",		no_argument,		NULL, 'h' },
		{ NULL }
	};
	struct bch_ioctl_data op = {
		.start_btree	= 0,
		.start_pos	= POS_MIN,
		.end_btree	= BTREE_ID_NR,
		.end_pos	= POS_MAX,
	};
	int opt;

	while ((opt = getopt_long(argc, argv, "b:s:e:h", longopts, NULL)) != -1)
		switch (opt) {
		case 'b':
			op.start_btree = read_string_list_or_die(optarg,
						__bch2_btree_ids, "btree id");
			op.end_btree = op.start_btree;
			break;
		case 's':
			op.start_pos	= bpos_parse(optarg);
			break;
		case 'e':
			op.end_pos	= bpos_parse(optarg);
			break;
		case 'h':
			data_job_usage();
		}
	args_shift(optind);

	char *job = arg_pop();
	if (!job)
		die("please specify which type of job");

	op.op = read_string_list_or_die(job, bch2_data_ops_strs, "bad job type");

	if (op.op == BCH_DATA_OP_scrub)
		die("scrub should be invoked with 'bcachefs data scrub'");

	if ((op.op == BCH_DATA_OP_rereplicate ||
	     op.op == BCH_DATA_OP_migrate ||
	     op.op == BCH_DATA_OP_drop_extra_replicas) &&
	    bcachefs_kernel_version() >= bcachefs_metadata_version_reconcile)
		die("%s no longer required or support >= reconcile; use 'bcachefs reconcile wait'", job);

	char *fs_path = arg_pop();
	if (!fs_path)
		fs_path = ".";

	if (argc)
		die("too many arguments");

	return bchu_data(bcache_fs_open(fs_path), op);
}

static int data_usage(void)
{
	puts("bcachefs data - manage filesystem data\n"
	     "Usage: bcachefs data <rereplicate|scrub|job> [OPTION]...\n"
	     "\n"
	     "Commands:\n"
	     "  rereplicate                  Rereplicate degraded data\n"
	     "  scrub                        Verify checksums and correct errors, if possible\n"
	     "  job                          Kick off low level data jobs\n"
	     "\n"
	     "Report bugs to <linux-bcachefs@vger.kernel.org>");
	exit(EXIT_SUCCESS);
}

int data_cmds(int argc, char *argv[])
{
	char *cmd = pop_cmd(&argc, argv);

	if (argc < 1)
		return data_usage();
	if (!strcmp(cmd, "rereplicate"))
		return cmd_data_rereplicate(argc, argv);
	if (!strcmp(cmd, "job"))
		return cmd_data_job(argc, argv);

	data_usage();
	return -EINVAL;
}

/* Reconcile commands */

static void reconcile_wait_usage(void)
{
	CLASS(printbuf, buf)();
	prt_bitflags(&buf, __bch2_reconcile_accounting_types, ~0UL);

	printf("bcachefs reconcile wait - wait for reconcile to finish background data processing\n"
	     "Usage: bcachefs reconcile wait [OPTION]... <mountpoint>\n"
	     "\n"
	     "Options:\n"
	     "  -t, --types=TYPES            List of reconcile types to wait on\n"
	     "                               %s\n"
	     "  -h, --help                   Display this help and exit\n"
	     "\n"
	     "Report bugs to <linux-bcachefs@vger.kernel.org>\n",
	     buf.buf);
	exit(EXIT_SUCCESS);
}

static bool reconcile_status(struct printbuf *out,
			     struct bchfs_handle fs,
			     unsigned types)
{
	bool scan_pending = read_file_u64(fs.sysfs_fd, "reconcile_scan_pending");

	u64 v[BCH_RECONCILE_ACCOUNTING_NR][2];
	memset(v, 0, sizeof(v));

	struct bch_ioctl_query_accounting *a =
		bchu_fs_accounting(fs, BIT(BCH_DISK_ACCOUNTING_reconcile_work));

	/*
	 * This would be cleaner if we had an interface for doing
	 * lookups on specific keys
	 */
	for_each_accounting(a, k) {
		struct disk_accounting_pos acc_k;
		bpos_to_disk_accounting_pos(&acc_k, k->k.p);

		v[acc_k.reconcile_work.type][0] = k->v.d[0];
		v[acc_k.reconcile_work.type][1] = k->v.d[1];
	}
	free(a);

	if (!out->nr_tabstops) {
		printbuf_tabstop_push(out, 32);
		printbuf_tabstop_push(out, 12);
		printbuf_tabstop_push(out, 12);
	}

	prt_printf(out, "Scan pending:\t%u\n", scan_pending);
	prt_printf(out, "\tdata\rmetadata\r\n");

	bool have_pending = scan_pending;

	for (unsigned i = 0; i < ARRAY_SIZE(v); i++)
		if (types & BIT(i)) {
			prt_printf(out, "  %s:\t", __bch2_reconcile_accounting_types[i]);
			prt_human_readable_u64(out, v[i][0] << 9);
			prt_tab_rjust(out);
			prt_human_readable_u64(out, v[i][1] << 9);
			prt_tab_rjust(out);
			prt_newline(out);
			have_pending |= v[i][0] != 0;
			have_pending |= v[i][1] != 0;
		}

	return have_pending;
}

static size_t count_newlines(const char *str)
{
	size_t ret = 0;
	const char *n;
	while ((n = strchr(str, '\n'))) {
		str = n + 1;
		ret++;
	}

	return ret;
}

static const char *restore_screen_str = "\033[?1049l";

static void exit_restore_screen(int n)
{
	write(STDOUT_FILENO, restore_screen_str, strlen(restore_screen_str));
	exit(EXIT_SUCCESS);
}

int cmd_reconcile_wait(int argc, char *argv[])
{
	static const struct option longopts[] = {
		{ "types",		required_argument,	NULL, 't' },
		{ "help",		no_argument,		NULL, 'h' },
		{ NULL }
	};
	unsigned types = ~0U & ~BIT(BCH_RECONCILE_ACCOUNTING_pending);
	int opt;

	while ((opt = getopt_long(argc, argv, "t:h", longopts, NULL)) != -1)
		switch (opt) {
		case 't':
			types = read_flag_list_or_die(optarg,
					__bch2_reconcile_accounting_types, "reconcile type");
			break;
		case 'h':
			reconcile_wait_usage();
		}
	args_shift(optind);

	char *fs_path = arg_pop();
	if (!fs_path)
		die("Please supply a filesystem");

	if (argc)
		die("too many arguments");

	struct bchfs_handle fs = bcache_fs_open(fs_path);

	write_file_str(fs.sysfs_fd, "internal/trigger_reconcile_wakeup", "1");

	struct sigaction act = { .sa_handler = exit_restore_screen };
	sigaction(SIGINT, &act, NULL);

	fputs("\033[?1049h", stdout);

	while (true) {
		CLASS(printbuf, buf)();
		bool pending = reconcile_status(&buf, fs, types);

		printf("\033[%zuF\033[J", count_newlines(buf.buf));
		fputs(buf.buf, stdout);

		if (!pending)
			break;

		sleep(1);
	}

	fputs(restore_screen_str, stdout);

	return 0;
}

static void reconcile_status_usage(void)
{
	CLASS(printbuf, buf)();
	prt_bitflags(&buf, __bch2_reconcile_accounting_types, ~0UL);

	printf("bcachefs reconcile status - show the status of a background reconciliation processing\n"
	       "Usage: bcachefs reconcile status [OPTION]... <mountpoint>\n"
	       "\n"
	       "Options:\n"
	       "  -t, --types=TYPES            List of reconcile types to wait on\n"
	     "                               %s\n"
	       "  -h, --help                   Display this help and exit\n"
	       "\n"
	       "Report bugs to <linux-bcachefs@vger.kernel.org>\n",
	       buf.buf);
	exit(EXIT_SUCCESS);
}

int cmd_reconcile_status(int argc, char *argv[])
{
	static const struct option longopts[] = {
		{ "types",		required_argument,	NULL, 't' },
		{ "help",		no_argument,		NULL, 'h' },
		{ NULL }
	};
	unsigned types = ~0U;
	int opt;

	while ((opt = getopt_long(argc, argv, "t:h", longopts, NULL)) != -1)
		switch (opt) {
		case 't':
			types = read_flag_list_or_die(optarg,
					__bch2_reconcile_accounting_types, "reconcile type");
			break;
		case 'h':
			reconcile_status_usage();
		}
	args_shift(optind);

	char *fs_path = arg_pop();
	if (!fs_path)
		die("Please supply a filesystem");

	if (argc)
		die("too many arguments");

	struct bchfs_handle fs = bcache_fs_open(fs_path);

	CLASS(printbuf, buf)();
	reconcile_status(&buf, fs, types);

	prt_newline(&buf);

	char *sysfs_status = read_file_str(fs.sysfs_fd, "reconcile_status");
	prt_str(&buf, sysfs_status);
	prt_newline(&buf);
	free(sysfs_status);

	fputs(buf.buf, stdout);

	return 0;
}

static int reconcile_usage(void)
{
	puts("bcachefs reconcile - manage data reconcile\n"
	     "Usage: bcachefs reconcile <status|wait> [OPTION]...\n"
	     "\n"
	     "Commands:\n"
	     "  status                       Show status of background data processing\n"
	     "  wait                         Wait on background data processing to complete\n"
	     "\n"
	     "Report bugs to <linux-bcachefs@vger.kernel.org>");
	exit(EXIT_SUCCESS);
}

int reconcile_cmds(int argc, char *argv[])
{
	char *cmd = pop_cmd(&argc, argv);

	if (argc < 1)
		return reconcile_usage();
	if (!strcmp(cmd, "status"))
		return cmd_reconcile_status(argc, argv);
	if (!strcmp(cmd, "wait"))
		return cmd_reconcile_wait(argc, argv);

	reconcile_usage();
	return -EINVAL;
}
