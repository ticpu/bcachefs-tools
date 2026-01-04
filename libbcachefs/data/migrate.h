/* SPDX-License-Identifier: GPL-2.0 */
#ifndef _BCACHEFS_MIGRATE_H
#define _BCACHEFS_MIGRATE_H

int bch2_dev_data_drop_by_backpointers(struct bch_fs *, unsigned, unsigned, struct printbuf *);
int bch2_dev_data_drop(struct bch_fs *, unsigned, unsigned, struct printbuf *);
int bch2_dev_evacuate_bucket_range(struct bch_fs *, struct bch_dev *, u64, u64);

#endif /* _BCACHEFS_MIGRATE_H */
