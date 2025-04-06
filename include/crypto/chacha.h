/*
 * Common values for the ChaCha20 algorithm
 */

#ifndef _CRYPTO_CHACHA20_H
#define _CRYPTO_CHACHA20_H

#include <linux/types.h>
#include <linux/crypto.h>
#include <linux/unaligned.h>

#define CHACHA_IV_SIZE	16
#define CHACHA_KEY_SIZE	32
#define CHACHA_BLOCK_SIZE	64
#define CHACHA_STATE_WORDS	(CHACHA_BLOCK_SIZE / sizeof(u32))

enum chacha_constants { /* expand 32-byte k */
	CHACHA_CONSTANT_EXPA = 0x61707865U,
	CHACHA_CONSTANT_ND_3 = 0x3320646eU,
	CHACHA_CONSTANT_2_BY = 0x79622d32U,
	CHACHA_CONSTANT_TE_K = 0x6b206574U
};

static inline void chacha_init_consts(u32 *state)
{
	state[0]  = CHACHA_CONSTANT_EXPA;
	state[1]  = CHACHA_CONSTANT_ND_3;
	state[2]  = CHACHA_CONSTANT_2_BY;
	state[3]  = CHACHA_CONSTANT_TE_K;
}

static inline void chacha_init(u32 *state, const u32 *key, const u8 *iv)
{
	chacha_init_consts(state);
	state[4]  = key[0];
	state[5]  = key[1];
	state[6]  = key[2];
	state[7]  = key[3];
	state[8]  = key[4];
	state[9]  = key[5];
	state[10] = key[6];
	state[11] = key[7];
	state[12] = get_unaligned_le32(iv +  0);
	state[13] = get_unaligned_le32(iv +  4);
	state[14] = get_unaligned_le32(iv +  8);
	state[15] = get_unaligned_le32(iv + 12);
}

#include <sodium/crypto_stream_chacha20.h>

static inline void chacha20_crypt(u32 *state, u8 *dst, const u8 *src,
				  unsigned int bytes)
{
	u32 *key = state + 4;
	u32 *iv  = state + 12;
	int ret = crypto_stream_chacha20_xor_ic(dst, src, bytes,
						(void *) &iv[2],
						iv[0] | ((u64) iv[1] << 32),
						(void *) key);
	BUG_ON(ret);
}

#endif
