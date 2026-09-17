/* BLAKE3-256 from the specification (https://github.com/BLAKE3-team/BLAKE3-specs).
 * Unkeyed hash mode only, which is all the Rill wire uses (§7.1a: BLAKE3-256
 * of the resource bytes). Portable C, no SIMD — a microcontroller's version.
 */
#include "blake3.h"
#include <string.h>

#define CHUNK_LEN 1024
#define BLOCK_LEN 64

enum { CHUNK_START = 1, CHUNK_END = 2, PARENT = 4, ROOT = 8 };

static const uint32_t IV[8] = {
    0x6A09E667, 0xBB67AE85, 0x3C6EF372, 0xA54FF53A,
    0x510E527F, 0x9B05688C, 0x1F83D9AB, 0x5BE0CD19,
};

static const uint8_t MSG_PERMUTATION[16] = { 2, 6, 3, 10, 7, 0, 4, 13, 1, 11, 12, 5, 9, 14, 15, 8 };

static inline uint32_t rotr32(uint32_t w, int c) { return (w >> c) | (w << (32 - c)); }
static inline uint32_t load32(const uint8_t *p) {
    return (uint32_t)p[0] | (uint32_t)p[1] << 8 | (uint32_t)p[2] << 16 | (uint32_t)p[3] << 24;
}
static inline void store32(uint8_t *p, uint32_t w) { p[0] = w; p[1] = w >> 8; p[2] = w >> 16; p[3] = w >> 24; }

static inline void g(uint32_t *s, int a, int b, int c, int d, uint32_t mx, uint32_t my) {
    s[a] = s[a] + s[b] + mx; s[d] = rotr32(s[d] ^ s[a], 16);
    s[c] = s[c] + s[d];      s[b] = rotr32(s[b] ^ s[c], 12);
    s[a] = s[a] + s[b] + my; s[d] = rotr32(s[d] ^ s[a], 8);
    s[c] = s[c] + s[d];      s[b] = rotr32(s[b] ^ s[c], 7);
}

static void round_fn(uint32_t *s, const uint32_t *m) {
    g(s, 0, 4, 8, 12, m[0], m[1]);   g(s, 1, 5, 9, 13, m[2], m[3]);
    g(s, 2, 6, 10, 14, m[4], m[5]);  g(s, 3, 7, 11, 15, m[6], m[7]);
    g(s, 0, 5, 10, 15, m[8], m[9]);  g(s, 1, 6, 11, 12, m[10], m[11]);
    g(s, 2, 7, 8, 13, m[12], m[13]); g(s, 3, 4, 9, 14, m[14], m[15]);
}

/* The compression function: 16 output words. The first 8 are the chaining
 * value; all 16 are the root output block. */
static void compress(const uint32_t cv[8], const uint8_t block[BLOCK_LEN], uint8_t block_len,
                     uint64_t counter, uint32_t flags, uint32_t out[16]) {
    uint32_t m[16], p[16];
    for (int i = 0; i < 16; i++) m[i] = load32(block + 4 * i);
    uint32_t s[16] = {
        cv[0], cv[1], cv[2], cv[3], cv[4], cv[5], cv[6], cv[7],
        IV[0], IV[1], IV[2], IV[3],
        (uint32_t)counter, (uint32_t)(counter >> 32), block_len, flags,
    };
    for (int r = 0; r < 7; r++) {
        round_fn(s, m);
        if (r < 6) {
            for (int i = 0; i < 16; i++) p[i] = m[MSG_PERMUTATION[i]];
            memcpy(m, p, sizeof m);
        }
    }
    for (int i = 0; i < 8; i++) { s[i] ^= s[i + 8]; s[i + 8] ^= cv[i]; }
    memcpy(out, s, sizeof s);
}

/* An "output" is the pending compression of a node: everything but the
 * final flags decision (chaining value vs root). */
typedef struct {
    uint32_t cv[8];
    uint8_t block[BLOCK_LEN];
    uint8_t block_len;
    uint64_t counter;
    uint32_t flags;
} output_t;

static void output_chaining_value(const output_t *o, uint32_t cv[8]) {
    uint32_t w[16];
    compress(o->cv, o->block, o->block_len, o->counter, o->flags, w);
    memcpy(cv, w, 8 * sizeof(uint32_t));
}

static void output_root_bytes(const output_t *o, uint8_t out[BLAKE3_OUT_LEN]) {
    uint32_t w[16];
    compress(o->cv, o->block, o->block_len, 0, o->flags | ROOT, w); /* output block 0 */
    for (int i = 0; i < 8; i++) store32(out + 4 * i, w[i]);
}

static void chunk_init(blake3_chunk_state *c, uint64_t counter) {
    memcpy(c->cv, IV, sizeof IV);
    c->chunk_counter = counter;
    memset(c->block, 0, BLOCK_LEN);
    c->block_len = 0;
    c->blocks_compressed = 0;
}

static size_t chunk_len(const blake3_chunk_state *c) {
    return (size_t)BLOCK_LEN * c->blocks_compressed + c->block_len;
}

static uint32_t chunk_start_flag(const blake3_chunk_state *c) {
    return c->blocks_compressed == 0 ? CHUNK_START : 0;
}

static void chunk_update(blake3_chunk_state *c, const uint8_t *in, size_t len) {
    while (len > 0) {
        /* A full buffered block is compressed only once more input arrives:
         * the last block of a chunk is finalized with CHUNK_END, not here. */
        if (c->block_len == BLOCK_LEN) {
            uint32_t w[16];
            compress(c->cv, c->block, BLOCK_LEN, c->chunk_counter, chunk_start_flag(c), w);
            memcpy(c->cv, w, 8 * sizeof(uint32_t));
            c->blocks_compressed++;
            memset(c->block, 0, BLOCK_LEN);
            c->block_len = 0;
        }
        size_t want = BLOCK_LEN - c->block_len;
        size_t take = len < want ? len : want;
        memcpy(c->block + c->block_len, in, take);
        c->block_len += (uint8_t)take;
        in += take;
        len -= take;
    }
}

static void chunk_output(const blake3_chunk_state *c, output_t *o) {
    memcpy(o->cv, c->cv, sizeof o->cv);
    memcpy(o->block, c->block, BLOCK_LEN);
    o->block_len = c->block_len;
    o->counter = c->chunk_counter;
    o->flags = chunk_start_flag(c) | CHUNK_END;
}

static void parent_output(const uint32_t left[8], const uint32_t right[8], output_t *o) {
    memcpy(o->cv, IV, sizeof IV);
    for (int i = 0; i < 8; i++) { store32(o->block + 4 * i, left[i]); store32(o->block + 32 + 4 * i, right[i]); }
    o->block_len = BLOCK_LEN;
    o->counter = 0;
    o->flags = PARENT;
}

static void parent_cv(const uint32_t left[8], const uint32_t right[8], uint32_t out[8]) {
    output_t o; parent_output(left, right, &o); output_chaining_value(&o, out);
}

void blake3_hasher_init(blake3_hasher *h) {
    chunk_init(&h->chunk, 0);
    h->cv_stack_len = 0;
}

/* Merge the finished chunk's CV into the tree: for every trailing zero bit
 * of the chunk count, the top of the stack is a left sibling to combine. */
static void add_chunk_cv(blake3_hasher *h, uint32_t new_cv[8], uint64_t total_chunks) {
    while ((total_chunks & 1) == 0) {
        h->cv_stack_len--;
        parent_cv(h->cv_stack[h->cv_stack_len], new_cv, new_cv);
        total_chunks >>= 1;
    }
    memcpy(h->cv_stack[h->cv_stack_len++], new_cv, 8 * sizeof(uint32_t));
}

void blake3_hasher_update(blake3_hasher *h, const void *input, size_t len) {
    const uint8_t *in = input;
    while (len > 0) {
        if (chunk_len(&h->chunk) == CHUNK_LEN) {
            output_t o; chunk_output(&h->chunk, &o);
            uint32_t cv[8]; output_chaining_value(&o, cv);
            uint64_t total = h->chunk.chunk_counter + 1;
            add_chunk_cv(h, cv, total);
            chunk_init(&h->chunk, total);
        }
        size_t want = CHUNK_LEN - chunk_len(&h->chunk);
        size_t take = len < want ? len : want;
        chunk_update(&h->chunk, in, take);
        in += take;
        len -= take;
    }
}

void blake3_hasher_finalize(const blake3_hasher *h, uint8_t out[BLAKE3_OUT_LEN]) {
    output_t o; chunk_output(&h->chunk, &o);
    int remaining = h->cv_stack_len;
    while (remaining > 0) {
        remaining--;
        uint32_t cv[8]; output_chaining_value(&o, cv);
        parent_output(h->cv_stack[remaining], cv, &o);
    }
    output_root_bytes(&o, out);
}

void blake3_hash(const void *input, size_t len, uint8_t out[BLAKE3_OUT_LEN]) {
    blake3_hasher h; blake3_hasher_init(&h); blake3_hasher_update(&h, input, len); blake3_hasher_finalize(&h, out);
}

#ifdef BLAKE3_SELFTEST
#include <stdio.h>
#include <stdlib.h>
static void hex(const uint8_t *b, char *out) { for (int i = 0; i < 32; i++) sprintf(out + 2 * i, "%02x", b[i]); }
int main(int argc, char **argv) {
    /* Published vectors for the empty string and "abc". */
    static const struct { const char *in; const char *want; } v[] = {
        { "", "af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262" },
        { "abc", "6437b3ac38465133ffb63b75273a8db548c558465d79db03fd359c6cd5bd9d85" },
    };
    int fail = 0; char got[65]; uint8_t out[32];
    for (unsigned i = 0; i < sizeof v / sizeof v[0]; i++) {
        blake3_hash(v[i].in, strlen(v[i].in), out); hex(out, got);
        int ok = strcmp(got, v[i].want) == 0; fail |= !ok;
        printf("%s  \"%s\"\n", ok ? "ok  " : "FAIL", v[i].in);
    }
    /* Hash a file when asked, so an external oracle can compare (any length). */
    if (argc > 1) {
        FILE *f = fopen(argv[1], "rb"); if (!f) return 2;
        blake3_hasher h; blake3_hasher_init(&h); uint8_t buf[4096]; size_t n;
        while ((n = fread(buf, 1, sizeof buf, f)) > 0) blake3_hasher_update(&h, buf, n);
        fclose(f); blake3_hasher_finalize(&h, out); hex(out, got); printf("%s  %s\n", got, argv[1]);
    }
    return fail;
}
#endif
