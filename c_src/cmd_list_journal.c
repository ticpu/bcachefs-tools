#include <fcntl.h>
#include <getopt.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/types.h>
#include <unistd.h>

#include "cmds.h"
#include "libbcachefs.h"
#include "tools-util.h"

#include "libbcachefs/bcachefs.h"
#include "libbcachefs/btree_iter.h"
#include "libbcachefs/errcode.h"
#include "libbcachefs/error.h"
#include "libbcachefs/journal_io.h"
#include "libbcachefs/journal_seq_blacklist.h"
#include "libbcachefs/super.h"

#include <linux/fs_parser.h>

static const char *NORMAL	= "\x1B[0m";
static const char *RED		= "\x1B[31m";

static void star_start_of_lines(char *buf)
{
	char *p = buf;

	if (*p == ' ')
		*p = '*';

	while ((p = strstr(p, "\n ")))
		p[1] = '*';
}

static inline bool entry_is_transaction_start(struct jset_entry *entry)
{
	return entry->type == BCH_JSET_ENTRY_log && !entry->level;
}

static inline bool entry_is_log_msg(struct jset_entry *entry)
{
	return entry->type == BCH_JSET_ENTRY_log && entry->level;
}

static inline bool entry_is_print_key(struct jset_entry *entry)
{
	switch (entry->type) {
	case BCH_JSET_ENTRY_btree_root:
	case BCH_JSET_ENTRY_btree_keys:
	case BCH_JSET_ENTRY_write_buffer_keys:
	case BCH_JSET_ENTRY_overwrite:
		return true;
	default:
		return false;
	}
}

typedef struct {
	int				sign;
	darray_str			f;
} transaction_msg_filter;

typedef struct {
	int				sign;
	DARRAY(struct bbpos_range)	f;
} transaction_key_filter;

typedef struct {
	u64				btree_filter;
	transaction_msg_filter		transaction;
	transaction_key_filter		key;
	bool				bkey_val;
} journal_filter;

static int parse_sign(char **str)
{
	if (**str == '+') {
		(*str)++;
		return 1;
	}

	if (**str == '-') {
		(*str)++;
		return -1;
	}

	return 0;
}

static bool entry_matches_btree_filter(journal_filter f, struct jset_entry *entry)
{
	return BIT_ULL(entry->btree_id) & f.btree_filter;
}

static bool transaction_matches_btree_filter(journal_filter f,
					struct jset_entry *entry, struct jset_entry *end)
{
	bool have_keys = false;

	for (entry = vstruct_next(entry);
	     entry != end;
	     entry = vstruct_next(entry))
		if (entry_is_print_key(entry)) {
			if (entry_matches_btree_filter (f, entry))
				return true;
			have_keys = true;
		}

	return !have_keys;
}

static bool bkey_matches_filter(transaction_key_filter f,
				struct jset_entry *entry,
				struct bkey_i *k)
{
	darray_for_each(f.f, i) {
		struct bbpos k_start	= BBPOS(entry->btree_id, bkey_start_pos(&k->k));
		struct bbpos k_end	= BBPOS(entry->btree_id, k->k.p);

		if (!i->start.pos.snapshot &&
		    !i->end.pos.snapshot) {
			k_start.pos.snapshot = 0;
			k_end.pos.snapshot = 0;
		}

		if (!k->k.size) {
			if (bbpos_cmp(k_start, i->start) >= 0 &&
			    bbpos_cmp(k_end, i->end) <= 0)
				return true;
		} else {
			if (bbpos_cmp(i->start, k_end) <= 0 &&
			    bbpos_cmp(i->end, k_start) >= 0)
				return true;
		}
	}
	return false;
}

static bool entry_matches_transaction_filter(transaction_key_filter f,
					     struct jset_entry *entry)
{
	if (!entry->level &&
	    (entry->type == BCH_JSET_ENTRY_btree_keys ||
	     entry->type == BCH_JSET_ENTRY_overwrite))
		jset_entry_for_each_key(entry, k) {
			if (!k->k.u64s)
				break;

			if (bkey_matches_filter(f, entry, k))
				return true;
		}
	return false;
}

static bool transaction_matches_transaction_filter(transaction_key_filter f,
					     struct jset_entry *entry, struct jset_entry *end)
{
	for (entry = vstruct_next(entry);
	     entry != end;
	     entry = vstruct_next(entry))
		if (entry_matches_transaction_filter(f, entry))
			return true;

	return false;
}

static bool entry_matches_msg_filter(transaction_msg_filter f,
				     struct jset_entry *entry)
{
	struct jset_entry_log *l = container_of(entry, struct jset_entry_log, entry);
	unsigned b = jset_entry_log_msg_bytes(l);

	darray_for_each(f.f, i)
		if (!strncmp(*i, l->d, b))
			return true;
	return false;
}

static bool entry_is_log_only(struct jset_entry *entry, struct jset_entry *end)
{
	bool have_log = false;

	for (entry = vstruct_next(entry);
	     entry != end;
	     entry = vstruct_next(entry)) {
		if (entry->u64s && !entry_is_log_msg(entry))
			return false;
		have_log = true;
	}

	return have_log;
}

static struct jset_entry *transaction_end(struct jset_entry *entry, struct jset_entry *end)
{
	do
		entry = vstruct_next(entry);
	while (entry != end && !entry_is_transaction_start(entry));

	return entry;
}

static bool should_print_transaction(journal_filter f,
				     struct jset_entry *entry, struct jset_entry *end)
{
	BUG_ON(entry->type != BCH_JSET_ENTRY_log);

	if (entry_is_log_only(entry, end))
		return true;

	if (!transaction_matches_btree_filter(f, entry, end))
		return false;

	if (f.transaction.f.nr &&
	    entry_matches_msg_filter(f.transaction, entry) != (f.transaction.sign >= 0))
		return false;

	if (f.key.f.nr &&
	    transaction_matches_transaction_filter(f.key, entry, end) != (f.key.sign >= 0))
		return false;

	return true;
}

static void journal_entry_header_to_text(struct printbuf *out, struct bch_fs *c,
					 struct journal_replay *p,
					 bool blacklisted, bool *printed_header)
{
	if (*printed_header)
		return;
	*printed_header = true;

	prt_printf(out,
		   "\n%s"
		   "journal entry     %llu\n"
		   "  version         %u\n"
		   "  last seq        %llu\n"
		   "  flush           %u\n"
		   "  written at      ",
		   blacklisted ? "blacklisted " : "",
		   le64_to_cpu(p->j.seq),
		   le32_to_cpu(p->j.version),
		   le64_to_cpu(p->j.last_seq),
		   !JSET_NO_FLUSH(&p->j));
	bch2_journal_ptrs_to_text(out, c, p);
	prt_newline(out);
}

static void bch2_journal_entry_keys_noval_to_text(struct printbuf *out, struct jset_entry *entry)
{
	jset_entry_for_each_key(entry, k) {
		/* We may be called on entries that haven't been validated: */
		if (!k->k.u64s)
			break;

		bch2_prt_jset_entry_type(out, entry->type);
		prt_str(out, ": ");
		bch2_btree_id_level_to_text(out, entry->btree_id, entry->level);
		prt_char(out, ' ');
		bch2_bkey_to_text(out, &k->k);
		prt_newline(out);
	}
}

static unsigned journal_entry_indent(struct jset_entry *entry)
{
	if (entry_is_transaction_start(entry) ||
	    entry->type == BCH_JSET_ENTRY_btree_root ||
	    entry->type == BCH_JSET_ENTRY_datetime ||
	    entry->type == BCH_JSET_ENTRY_usage)
		return 2;
	return 4;
}

static void print_one_entry(struct printbuf		*out,
			    struct bch_fs		*c,
			    journal_filter		f,
			    struct journal_replay	*p,
			    bool			blacklisted,
			    bool			*printed_header,
			    struct jset_entry		*entry)
{
	if (entry_is_print_key(entry) && !entry->u64s)
		return;

	if (entry_is_print_key(entry) && !entry_matches_btree_filter(f, entry))
		return;

	if ((f.key.f.nr || f.transaction.f.nr) &&
	    entry->type == BCH_JSET_ENTRY_btree_root)
		return;

	journal_entry_header_to_text(out, c, p, blacklisted, printed_header);

	bool highlight = entry_matches_transaction_filter(f.key, entry);
	if (highlight)
		fputs(RED, stdout);

	unsigned indent = journal_entry_indent(entry);
	printbuf_indent_add(out, indent);

	if (!f.bkey_val && entry_is_print_key(entry))
		bch2_journal_entry_keys_noval_to_text(out, entry);
	else {
		bch2_journal_entry_to_text(out, c, entry);
		prt_newline(out);
	}

	printbuf_indent_sub(out, indent);

	if (highlight)
		prt_str(out, NORMAL);
}

static void journal_replay_print(struct bch_fs *c,
				 journal_filter f,
				 struct journal_replay *p)
{
	struct printbuf buf = PRINTBUF;
	bool blacklisted = p->ignore_blacklisted ||
		bch2_journal_seq_is_blacklisted(c, le64_to_cpu(p->j.seq), false);
	bool printed_header = false;

	if (!f.transaction.f.nr &&
	    !f.key.f.nr)
		journal_entry_header_to_text(&buf, c, p, blacklisted, &printed_header);

	struct jset_entry *entry = p->j.start;
	struct jset_entry *end = vstruct_last(&p->j);

	while (entry < end &&
	       vstruct_next(entry) <= end &&
	       !entry_is_transaction_start(entry)) {
		print_one_entry(&buf, c, f, p, blacklisted, &printed_header, entry);
		entry = vstruct_next(entry);
	}

	while (entry < end &&
	       vstruct_next(entry) <= end) {
		struct jset_entry *t_end = transaction_end(entry, end);

		if (should_print_transaction(f, entry, t_end)) {
			while (entry < t_end &&
			       vstruct_next(entry) <= t_end) {
				print_one_entry(&buf, c, f, p, blacklisted, &printed_header, entry);
				entry = vstruct_next(entry);
			}
		}

		entry = t_end;
	}

	if (buf.buf) {
		if (blacklisted)
			star_start_of_lines(buf.buf);
		write(STDOUT_FILENO, buf.buf, buf.pos);
	}
	printbuf_exit(&buf);
}

static void list_journal_usage(void)
{
	puts("bcachefs list_journal - print contents of journal\n"
	     "Usage: bcachefs list_journal [OPTION]... <devices>\n"
	     "\n"
	     "Options:\n"
	     "  -a                                    Read entire journal, not just dirty entries\n"
	     "  -n, --nr-entries=nr                   Number of journal entries to print, starting from the most recent\n"
	     "  -k, --btree-filter=(+|-)btree1,btree2 Filter keys matching or not updating btree(s)\n"
	     "  -t, --transaction=(+|-)fn1,fn2        Filter transactions matching or not matching fn(s)"
	     "  -k, --key-filter=(+-1)bbpos1,bbpos2x  Filter transactions updating bbpos\n"
	     "                                        Or entries not matching the range bbpos-bbpos\n"
	     "  -v, --verbose                         Verbose mode\n"
	     "  -h, --help                            Display this help and exit\n"
	     "Report bugs to <linux-bcachefs@vger.kernel.org>");
}

int cmd_list_journal(int argc, char *argv[])
{
	static const struct option longopts[] = {
		{ "nr-entries",		required_argument,	NULL, 'n' },
		{ "btree",		required_argument,	NULL, 'b' },
		{ "transaction",	required_argument,	NULL, 't' },
		{ "key",		required_argument,	NULL, 'k' },
		{ "bkey-val",		required_argument,	NULL, 'V' },
		{ "verbose",		no_argument,		NULL, 'v' },
		{ "help",		no_argument,		NULL, 'h' },
		{ NULL }
	};
	struct bch_opts opts = bch2_opts_empty();
	u32 nr_entries = U32_MAX;
	journal_filter f = { .btree_filter = ~0ULL };
	char *t;
	int opt, ret;

	opt_set(opts, noexcl,		true);
	opt_set(opts, nochanges,	true);
	opt_set(opts, norecovery,	true);
	opt_set(opts, read_only,	true);
	opt_set(opts, degraded,		BCH_DEGRADED_very);
	opt_set(opts, errors,		BCH_ON_ERROR_continue);
	opt_set(opts, fix_errors,	FSCK_FIX_yes);
	opt_set(opts, retain_recovery_info ,true);
	opt_set(opts, read_journal_only,true);

	while ((opt = getopt_long(argc, argv, "an:m:t:k:vh",
				  longopts, NULL)) != -1)
		switch (opt) {
		case 'a':
			opt_set(opts, read_entire_journal, true);
			break;
		case 'n':
			if (kstrtouint(optarg, 10, &nr_entries))
				die("error parsing nr_entries");
			opt_set(opts, read_entire_journal, true);
			break;
		case 'b':
			ret = parse_sign(&optarg);
			f.btree_filter =
				read_flag_list_or_die(optarg, __bch2_btree_ids, "btree id");
			if (ret < 0)
				f.btree_filter = ~f.btree_filter;
			break;
		case 't':
			f.transaction.sign = parse_sign(&optarg);
			while ((t = strsep(&optarg, ",")))
				darray_push(&f.transaction.f, strdup(t));
			break;
		case 'k':
			f.key.sign = parse_sign(&optarg);
			while ((t = strsep(&optarg, ",")))
				darray_push(&f.key.f, bbpos_range_parse(t));
			break;
		case 'V':
			ret = lookup_constant(bool_names, optarg, -EINVAL);
			if (ret < 0)
				die("error parsing %s", optarg);
			f.bkey_val = ret;
			break;
		case 'v':
			opt_set(opts, verbose, true);
			break;
		case 'h':
			list_journal_usage();
			exit(EXIT_SUCCESS);
		}
	args_shift(optind);

	if (!argc)
		die("Please supply device(s) to open");

	darray_const_str devs = get_or_split_cmdline_devs(argc, argv);

	struct bch_fs *c = bch2_fs_open(&devs, &opts);
	if (IS_ERR(c))
		die("error opening %s: %s", argv[0], bch2_err_str(PTR_ERR(c)));

	struct journal_replay *p, **_p;
	struct genradix_iter iter;
	genradix_for_each(&c->journal_entries, iter, _p) {
		p = *_p;
		if (p && le64_to_cpu(p->j.seq) + nr_entries >= atomic64_read(&c->journal.seq))
			journal_replay_print(c, f, p);
	}

	bch2_fs_stop(c);
	return 0;
}
