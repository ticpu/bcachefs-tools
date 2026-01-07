
#include <errno.h>
#include <getopt.h>
#include <sys/uio.h>
#include <unistd.h>

#include "cmds.h"
#include "libbcachefs.h"
#include "tools-util.h"

#include "fs/check.h"

#include "init/error.h"
#include "init/fs.h"
#include "init/passes.h"
#include "sb/io.h"

static void setnonblocking(int fd)
{
	int flags = fcntl(fd, F_GETFL);
	if (fcntl(fd, F_SETFL, flags|O_NONBLOCK))
		die("fcntl error: %m");
}

static int do_splice(int rfd, int wfd)
{
	char buf[4096], *b = buf;

	int r = read(rfd, buf, sizeof(buf));
	if (r < 0 && errno == EAGAIN)
		return 0;
	if (r < 0)
		return r;
	if (!r)
		return 1;
	do {
		ssize_t w = write(wfd, b, r);

		/*
		 * Ugly, but we have no way of doing nonblocking reads and
		 * blocking writes.
		 *
		 * Yes, this means that if one thread has stopped reading (or
		 * isn't keeping up) we block traffic on the other direction of
		 * the pipe. No, I don't care.
		 */
		if (w < 0 && errno == EAGAIN) {
			fd_set fds;
			FD_ZERO(&fds);
			FD_SET(wfd, &fds);
			if (select(wfd + 1, NULL, &fds, NULL, NULL) < 0)
				die("select error: %m");
			continue;
		}

		if (w < 0)
			die("%s: write error: %m", __func__);
		r -= w;
		b += w;
	} while (r);
	return 0;
}

static int splice_fd_to_stdinout(int fd)
{
	setnonblocking(STDIN_FILENO);
	setnonblocking(fd);

	bool stdin_closed = false;

	while (true) {
		fd_set fds;

		FD_ZERO(&fds);
		FD_SET(fd, &fds);
		if (!stdin_closed)
			FD_SET(STDIN_FILENO, &fds);

		if (select(fd + 1, &fds, NULL, NULL, NULL) < 0)
			die("select error: %m");

		int r = do_splice(fd, STDOUT_FILENO);
		if (r < 0)
			return r;
		if (r)
			break;

		r = do_splice(STDIN_FILENO, fd);
		if (r < 0)
			return r;
		if (r)
			stdin_closed = true;
	}

	/* the return code from fsck itself is returned via close() */
	return close(fd);
}

static int fsck_online(struct bchfs_handle fs, const char *opt_str)
{
	struct bch_ioctl_fsck_online fsck = {
		.opts = (unsigned long) opt_str
	};

	int fsck_fd = ioctl(fs.ioctl_fd, BCH_IOCTL_FSCK_ONLINE, &fsck);
	if (fsck_fd < 0)
		die("BCH_IOCTL_FSCK_ONLINE error: %s", bch2_err_str(errno));

	return splice_fd_to_stdinout(fsck_fd);
}

static void append_opt(struct printbuf *out, const char *opt)
{
	if (out->pos)
		prt_char(out, ',');
	prt_str(out, opt);
}

static bool should_use_kernel_fsck(darray_const_str devs)
{
	unsigned kernel_version = bcachefs_kernel_version();

	if (!kernel_version)
		return false;

	if (kernel_version == bcachefs_metadata_version_current)
		return false;

	struct bch_opts opts = bch2_opts_empty();
	opt_set(opts, nostart, true);
	opt_set(opts, noexcl, true);
	opt_set(opts, nochanges, true);
	opt_set(opts, read_only, true);

	struct bch_fs *c = bch2_fs_open(&devs, &opts);
	if (IS_ERR(c))
		return false;

	bool ret = ((bcachefs_metadata_version_current < kernel_version &&
		     kernel_version <= c->sb.version) ||
		    (c->sb.version <= kernel_version &&
		     kernel_version < bcachefs_metadata_version_current));

	if (ret) {
		struct printbuf buf = PRINTBUF;

		prt_str(&buf, "fsck binary is version ");
		bch2_version_to_text(&buf, bcachefs_metadata_version_current);
		prt_str(&buf, " but filesystem is ");
		bch2_version_to_text(&buf, c->sb.version);
		prt_str(&buf, " and kernel is ");
		bch2_version_to_text(&buf, kernel_version);
		prt_str(&buf, ", using kernel fsck\n");

		printf("%s", buf.buf);
		printbuf_exit(&buf);
	}

	bch2_fs_exit(c);

	return ret;
}

static bool is_blockdev(const char *path)
{
	struct stat s;
	if (stat(path, &s))
		return true;
	return S_ISBLK(s.st_mode);
}

static void loopdev_free(const char *path)
{
	char *cmd = mprintf("losetup -d %s", path);
	system(cmd);
	free(cmd);
}

static char *loopdev_alloc(const char *path)
{
	char *cmd = mprintf("losetup --show -f %s", path);
	FILE *f = popen(cmd, "r");
	free(cmd);
	if (!f) {
		fprintf(stderr, "error executing losetup: %m\n");
		return NULL;
	}

	char *line = NULL;
	size_t n = 0;
	getline(&line, &n, f);
	int ret = pclose(f);
	if (ret) {
		fprintf(stderr, "error executing losetup: %i\n", ret);
		free(line);
		return NULL;
	}

	strim(line);
	return line;
}

static void fsck_usage(void)
{
	puts("bcachefs fsck - filesystem check and repair\n"
	     "Usage: bcachefs fsck [OPTION]... <devices>\n"
	     "\n"
	     "Options:\n"
	     "  -p                           Automatic repair (no questions)\n"
	     "  -n                           Don't repair, only check for errors\n"
	     "  -y                           Assume \"yes\" to all questions\n"
	     "  -f                           Force checking even if filesystem is marked clean\n"
	     "  -r, --ratelimit_errors       Don't display more than 10 errors of a given type\n"
	     "  -k, --kernel                 Use the in-kernel fsck implementation\n"
	     "  -K, --no-kernel\n"
	     "  -v                           Be verbose\n"
	     "  -h, --help                   Display this help and exit\n"
	     "\n"
	     "Report bugs to <linux-bcachefs@vger.kernel.org>");
	exit(EXIT_SUCCESS);
}

int cmd_fsck(int argc, char *argv[])
{
	static const struct option longopts[] = {
		{ "ratelimit_errors",	no_argument,		NULL, 'r' },
		{ "kernel",		no_argument,		NULL, 'k' },
		{ "no-kernel",		no_argument,		NULL, 'K' },
		{ "help",		no_argument,		NULL, 'h' },
		{ NULL }
	};
	int kernel = -1; /* unset */
	int opt, ret = 0;
	CLASS(printbuf, opts_str)();

	if (getenv("BCACHEFS_KERNEL_ONLY"))
		kernel = true;

	append_opt(&opts_str, "degraded");
	append_opt(&opts_str, "fsck");
	append_opt(&opts_str, "fix_errors=ask");
	append_opt(&opts_str, "read_only");

	while ((opt = getopt_long(argc, argv,
				  "apynfo:rkKvh",
				  longopts, NULL)) != -1)
		switch (opt) {
		case 'a':
		case 'p':
			/* "automatic" run, called by the system, for us to do checks as needed.
			 *  we don't need checks here: */
			exit(EXIT_SUCCESS);
		case 'y':
			append_opt(&opts_str, "fix_errors=yes");
			break;
		case 'n':
			append_opt(&opts_str, "nochanges");
			append_opt(&opts_str, "fix_errors=no");
			break;
		case 'f':
			/* force check, even if filesystem marked clean: */
			break;
		case 'o':
			append_opt(&opts_str, optarg);
			break;
		case 'r':
			append_opt(&opts_str, "ratelimit_errors");
			break;
		case 'k':
			kernel = true;
			break;
		case 'K':
			kernel = false;
			break;
		case 'v':
			append_opt(&opts_str, "verbose");
			break;
		case 'h':
			fsck_usage();
			exit(16);
		}
	args_shift(optind);

	if (!argc) {
		fprintf(stderr, "Please supply device(s) to check\n");
		exit(8);
	}

	darray_const_str devs = get_or_split_cmdline_devs(argc, argv);

	if (devs.nr == 1 &&
	    S_ISDIR(xstat(devs.data[0]).st_mode)) {
		printf("Running fsck online\n");

		struct bchfs_handle fs = bcache_fs_open(devs.data[0]);
		return fsck_online(fs, opts_str.buf);
	}

	darray_for_each(devs, i)
		if (dev_mounted(*i)) {
			printf("Running fsck online\n");

			int dev_idx;
			struct bchfs_handle fs = bchu_fs_open_by_dev(*i, &dev_idx);
			return fsck_online(fs, opts_str.buf);
		}

	if (kernel)
		system("modprobe bcachefs");

	int kernel_probed = kernel;
	if (kernel_probed < 0)
		kernel_probed = should_use_kernel_fsck(devs);

	struct bch_opts opts = bch2_opts_empty();
	struct printbuf parse_later = PRINTBUF;

	if (kernel_probed) {
		darray_str loopdevs = {};
		int fsck_fd = -1;

		printf("Running in-kernel offline fsck\n");
		struct bch_ioctl_fsck_offline *fsck = calloc(sizeof(*fsck) + sizeof(u64) * devs.nr, 1);

		fsck->opts = (unsigned long)opts_str.buf;
		darray_for_each(devs, i) {
			if (is_blockdev(*i)) {
				fsck->devs[i - devs.data] = (unsigned long) *i;
			} else {
				char *l = loopdev_alloc(*i);
				if (!l)
					goto kernel_fsck_err;
				darray_push(&loopdevs, l);
				fsck->devs[i - devs.data] = (unsigned long) l;
			}
		}
		fsck->nr_devs = devs.nr;

		int ctl_fd = bcachectl_open();
		fsck_fd = ioctl(ctl_fd, BCH_IOCTL_FSCK_OFFLINE, fsck);
kernel_fsck_err:
		free(fsck);

		darray_for_each(loopdevs, i)
			loopdev_free(*i);
		darray_exit(&loopdevs);

		if (fsck_fd < 0 && kernel < 0)
			goto userland_fsck;

		if (fsck_fd < 0)
			die("BCH_IOCTL_FSCK_OFFLINE error: %s", bch2_err_str(errno));

		ret = splice_fd_to_stdinout(fsck_fd);
	} else {
userland_fsck:
		printf("Running userspace offline fsck\n");
		ret = bch2_parse_mount_opts(NULL, &opts, &parse_later, opts_str.buf, false);
		if (ret)
			return ret;

		struct bch_fs *c = bch2_fs_open(&devs, &opts);
		if (IS_ERR(c))
			exit(8);

		CLASS(printbuf, buf)();
		ret = bch2_fs_fsck_errcode(c, &buf);
		if (ret)
			fputs(buf.buf, stderr);

		int ret2 = bch2_fs_exit(c);
		if (ret2) {
			fprintf(stderr, "error shutting down filesystem: %s\n", bch2_err_str(ret2));
			ret |= 8;
		}
	}

	return ret;
}

static void recovery_pass_usage(void)
{
	puts("bcachefs recovery-pass - list and manage scheduled recovery passes\n"
	     "Usage: bcachefs recovery-pass [OPTION]... <devices>\n"
	     "\n"
	     "Currently only supports unmounted/offline filesystems\n"
	     "\n"
	     "Options:\n"
	     "  -s, --set                    Schedule a recovery pass in the superblock\n"
	     "  -u, --unset                  Deschedule a recovery pass\n"
	     "  -h, --help                   Display this help and exit\n"
	     "\n"
	     "Report bugs to <linux-bcachefs@vger.kernel.org>");
	exit(EXIT_SUCCESS);
}

int cmd_recovery_pass(int argc, char *argv[])
{
	static const struct option longopts[] = {
		{ "set",		required_argument,	NULL, 's' },
		{ "unset",		required_argument,	NULL, 'u' },
		{ "help",		no_argument,		NULL, 'h' },
		{ NULL }
	};
	u64 passes_to_set = 0, passes_to_unset = 0;
	int opt;

	while ((opt = getopt_long(argc, argv, "s:u:h", longopts, NULL)) != -1)
		switch (opt) {
		case 's':
			passes_to_set |= read_flag_list_or_die(optarg,
						bch2_recovery_passes,
						"recovery pass");
			break;
		case 'u':
			passes_to_unset |= read_flag_list_or_die(optarg,
						bch2_recovery_passes,
						"recovery pass");
			break;
		case 'h':
			recovery_pass_usage();
			exit(EXIT_SUCCESS);
		}
	args_shift(optind);

	passes_to_set	= bch2_recovery_passes_to_stable(passes_to_set);
	passes_to_unset	= bch2_recovery_passes_to_stable(passes_to_unset);

	darray_const_str devs = get_or_split_cmdline_devs(argc, argv);

	struct bch_opts opts = bch2_opts_empty();
	opt_set(opts, nostart, true);

	struct bch_fs *c = bch2_fs_open(&devs, &opts);
	int ret = PTR_ERR_OR_ZERO(c);
	if (ret)
		die("Error opening filesystem: %s", bch2_err_str(ret));

	scoped_guard(mutex, &c->sb_lock) {
		struct bch_sb_field_ext *ext =
			bch2_sb_field_get_minsize(&c->disk_sb, ext,
				sizeof(struct bch_sb_field_ext) / sizeof(u64));
		if (!ext) {
			fprintf(stderr, "Error getting sb_field_ext\n");
			goto err;
		}

		u64 scheduled = le64_to_cpu(ext->recovery_passes_required[0]);

		if (passes_to_set || passes_to_unset) {
			ext->recovery_passes_required[0] &= ~cpu_to_le64(passes_to_unset);
			ext->recovery_passes_required[0] |=  cpu_to_le64(passes_to_set);

			scheduled = le64_to_cpu(ext->recovery_passes_required[0]);

			bch2_write_super(c);
		}

		CLASS(printbuf, buf)();
		prt_str(&buf, "Scheduled recovery passes: ");
		if (scheduled)
			prt_bitflags(&buf, bch2_recovery_passes,
				     bch2_recovery_passes_from_stable(scheduled));
		else
			prt_str(&buf, "(none)");
		printf("%s\n", buf.buf);
	}
err:
	bch2_fs_exit(c);
	return ret;
}

