/* BLAKE3-256, written from the BLAKE3 specification for the Rill endpoint.
 * One-shot and streaming; arbitrary input length (the chunk tree included).
 * No dependencies. Verified against the published test vectors and against
 * the Rust client's own hash via GET_IF → NOT_MODIFIED (see README).
 */
#ifndef RILL_BLAKE3_H
#define RILL_BLAKE3_H
#include <stddef.h>
#include <stdint.h>

#define BLAKE3_OUT_LEN 32

typedef struct {
    uint32_t cv[8];
    uint64_t chunk_counter;
    uint8_t block[64];
    uint8_t block_len;
    uint8_t blocks_compressed;
} blake3_chunk_state;

typedef struct {
    blake3_chunk_state chunk;
    uint32_t cv_stack[54][8]; /* enough for 2^54 chunks */
    uint8_t cv_stack_len;
} blake3_hasher;

void blake3_hasher_init(blake3_hasher *h);
void blake3_hasher_update(blake3_hasher *h, const void *input, size_t len);
void blake3_hasher_finalize(const blake3_hasher *h, uint8_t out[BLAKE3_OUT_LEN]);
/* Convenience: hash `len` bytes in one call. */
void blake3_hash(const void *input, size_t len, uint8_t out[BLAKE3_OUT_LEN]);

#endif
