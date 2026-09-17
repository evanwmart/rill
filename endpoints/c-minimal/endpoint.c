/*
 * A minimal Rill *serve-only* endpoint, written from the specs alone.
 *
 * This is the first foreign implementation of the wire's endpoint role: it
 * shares no code with the Rust crates. Everything here was written from
 *   specs/protocol.md         — the frame layout, types, flags, statuses
 *   specs/connection.md       — the session lifecycle, TLS 1.3 + ALPN
 *   specs/security.md         — self-signed cert, pinned by fingerprint
 *   specs/document-format.md  — the RDOC document a device emits
 * If a stranger can do this from those documents, the documents are the
 * spec. Where they were not enough, that is a finding, recorded in README.md.
 *
 * What it does: serves one live document at "/temp" — a real reading from
 * the host's thermal sensor — with a `live` node so a Rill viewer polls it.
 * That is the whole role of a sensor endpoint: values plus a clock, no
 * rendering, no code shipped to the viewer.
 *
 * Structure: the protocol and document code here is portable C with two
 * seams beneath it — tls.h (OpenSSL on the host, mbedTLS on the chip) and
 * blake3.h (ours, from the spec). The microcontroller port changes the TLS
 * file and the sensor read, nothing above.
 *
 * Deliberately minimal (each a documented choice, not an omission):
 *   - No zstd: ACCEPT_ZSTD is an ignorable flag, so we send raw. Legal.
 *   - No ACTION support: a read-only sensor has nothing to act on, so
 *     ACTION answers NOT_FOUND (the only status a handler may choose, §8).
 *   - Policy is "everything public to anonymous": a client cert is
 *     requested but not required (security.md §3), and never inspected.
 *   - One forked child per connection; no pipelining (v1 is strict
 *     request/response anyway, §2).
 *
 * Build:  make            (gcc + OpenSSL 3)
 * Cert:   ./gen-cert.sh   (self-signed P-256; the viewer pins its SHA-256)
 * Run:    ./endpoint [port]   (default 7430)
 */

#include "blake3.h"
#include "tls.h"

#include <arpa/inet.h>
#include <errno.h>
#include <math.h>
#include <netinet/in.h>
#include <netinet/tcp.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

/* ---- protocol constants (specs/protocol.md §3–§9) ---------------------- */

#define HDR_LEN 16
#define MAX_PAYLOAD (1u << 20) /* 1 MiB, checked before any allocation */
#define MAX_PATH 1024
#define MAX_PING 64

enum { T_GET = 0x01, T_HEAD = 0x02, T_PING = 0x03, T_CLOSE = 0x04, T_GET_IF = 0x05, T_ACTION = 0x07 };
enum { T_RESOURCE = 0x81, T_METADATA = 0x82, T_ERROR = 0x83, T_PONG = 0x84, T_NOT_MODIFIED = 0x85 };

enum {
    S_PROTOCOL_MALFORMED = 0x0100,
    S_UNSUPPORTED_VERSION = 0x0101,
    S_UNKNOWN_FRAME_TYPE = 0x0102,
    S_FRAME_TOO_LARGE = 0x0103,
    S_UNKNOWN_CRITICAL_FLAG = 0x0104,
    S_PATH_INVALID = 0x0105,
    S_NOT_FOUND = 0x0200,
};

/* flags (§5): bits 8–15 critical, 0–7 ignorable */
#define F_ACTION_CAS 0x0400   /* critical; on ACTION only */
#define KNOWN_CRITICAL 0x0700 /* MORE | CONTENT_ZSTD | ACTION_CAS */

/* ---- byte order helpers: everything on the wire is big-endian ---------- */

static void put_u16(uint8_t *p, uint16_t v) { p[0] = v >> 8; p[1] = v; }
static void put_u32(uint8_t *p, uint32_t v) { p[0] = v >> 24; p[1] = v >> 16; p[2] = v >> 8; p[3] = v; }
static void put_u64(uint8_t *p, uint64_t v) { put_u32(p, v >> 32); put_u32(p + 4, (uint32_t)v); }
static void put_f32(uint8_t *p, float f) { uint32_t u; memcpy(&u, &f, 4); put_u32(p, u); }
static uint16_t get_u16(const uint8_t *p) { return (uint16_t)(p[0] << 8 | p[1]); }
static uint32_t get_u32(const uint8_t *p) { return (uint32_t)p[0] << 24 | (uint32_t)p[1] << 16 | (uint32_t)p[2] << 8 | p[3]; }

/* ---- the document (specs/document-format.md, format version 8) --------- */

/* A tagged dimension: 5 bytes, tag 1 = px + f32 BE. */
static uint8_t *put_dim_px(uint8_t *p, float px) { *p++ = 1; put_f32(p, px); return p + 4; }

/* Read the host's thermal sensor in °C, or a slow sine if there is none —
 * the value is live either way, and the document says which. */
static float read_temperature(int *simulated) {
    FILE *f = fopen("/sys/class/thermal/thermal_zone0/temp", "r");
    if (f) {
        long milli = 0;
        int ok = fscanf(f, "%ld", &milli) == 1;
        fclose(f);
        if (ok) { *simulated = 0; return milli / 1000.0f; }
    }
    *simulated = 1;
    return 21.0f + 3.0f * (float)sin((double)time(NULL) / 30.0);
}

/* Build the RDOC for "/temp". Returns malloc'd bytes, sets *len.
 *
 * Layout: header(32) | strings | (no styles) | (no state) | (no actions) | nodes
 * Tree (post-order, children before parents — §6 canonicalization):
 *   0 Text   "Temperature"
 *   1 Text   "<value> °C"
 *   2 Text   "<source>"
 *   3 Live   target "/temp", every 1000 ms
 *   4 Column [0,1,2,3]           ← root, referenced by nobody
 */
static uint8_t *build_document(size_t *len) {
    int simulated;
    float t = read_temperature(&simulated);
    char value[32], source[64];
    snprintf(value, sizeof value, "%.1f \xC2\xB0" "C", t); /* "°" as UTF-8 */
    snprintf(source, sizeof source, simulated ? "simulated sensor" : "host thermal_zone0, live");

    /* The string table must be strictly ascending bytewise (§3). */
    const char *s[4] = { "/temp", value, source, "Temperature" };
    int idx[4] = { 0, 1, 2, 3 }; /* idx[k] = position of s[k] after sort */
    const char *sorted[4];
    memcpy(sorted, s, sizeof s);
    for (int i = 0; i < 4; i++)
        for (int j = i + 1; j < 4; j++)
            if (strcmp(sorted[j], sorted[i]) < 0) { const char *tmp = sorted[i]; sorted[i] = sorted[j]; sorted[j] = tmp; }
    for (int k = 0; k < 4; k++)
        for (int p = 0; p < 4; p++)
            if (sorted[p] == s[k]) idx[k] = p;
    int i_path = idx[0], i_value = idx[1], i_source = idx[2], i_label = idx[3];

    size_t strings_len = 0;
    for (int p = 0; p < 4; p++) strings_len += 2 + strlen(sorted[p]);

    /* node bodies: Text = style_ref(2)+value_idx(2) = 4; Live = 2+2+2 = 6;
     * Column (v8) = style_ref(2) + gap(5) + padding(5) + target(2) + count(2)
     *             + 4 children*4 = 32. */
    size_t nodes_len = 3 * (4 + 4) + (4 + 6) + (4 + 32);
    size_t total = 32 + strings_len + nodes_len;

    uint8_t *doc = malloc(total), *p = doc;
    memset(doc, 0, total);

    /* header */
    memcpy(p, "RDOC", 4); p[4] = 0x08;              /* magic, version 8, 3 reserved */
    put_u32(p + 8, (uint32_t)total);
    put_u16(p + 12, 4);                              /* string count */
    put_u16(p + 14, 0);                              /* style count */
    put_u32(p + 16, 5);                              /* node count */
    put_u32(p + 20, 4);                              /* root index */
    put_u16(p + 24, 0); put_u16(p + 26, 0);          /* state, action counts */
    p += 32;

    /* string table */
    for (int k = 0; k < 4; k++) {
        size_t n = strlen(sorted[k]);
        put_u16(p, (uint16_t)n); p += 2;
        memcpy(p, sorted[k], n); p += n;
    }

    /* nodes */
    int text_idx[3] = { i_label, i_value, i_source };
    for (int k = 0; k < 3; k++) {
        put_u16(p, 0x0001); put_u16(p + 2, 4); p += 4;   /* Text, body_len 4 */
        put_u16(p, 0xFFFF); put_u16(p + 2, (uint16_t)text_idx[k]); p += 4;
    }
    put_u16(p, 0x0011); put_u16(p + 2, 6); p += 4;       /* Live, body_len 6 */
    put_u16(p, 0xFFFF); put_u16(p + 2, (uint16_t)i_path); put_u16(p + 4, 1000); p += 6;
    put_u16(p, 0x0004); put_u16(p + 2, 32); p += 4;      /* Column, body_len 32 */
    put_u16(p, 0xFFFF); p += 2;                          /* style: none */
    p = put_dim_px(p, 8.0f);                             /* gap */
    p = put_dim_px(p, 12.0f);                            /* padding */
    put_u16(p, 0xFFFF); p += 2;                          /* target: none (v8 field) */
    put_u16(p, 4); p += 2;                               /* child count */
    for (uint32_t c = 0; c < 4; c++) { put_u32(p, c); p += 4; }

    *len = (size_t)(p - doc);
    if (*len != total) { fprintf(stderr, "document size mismatch %zu != %zu\n", *len, total); abort(); }
    return doc;
}

/* ---- framed I/O over the TLS seam -------------------------------------- */

static int read_exact(tls_conn *c, uint8_t *buf, size_t n) {
    size_t got = 0;
    while (got < n) {
        int r = tls_read(c, buf + got, n - got);
        if (r <= 0) return 0;
        got += (size_t)r;
    }
    return 1;
}

static int write_all(tls_conn *c, const uint8_t *buf, size_t n) {
    size_t sent = 0;
    while (sent < n) {
        int r = tls_write(c, buf + sent, n - sent);
        if (r <= 0) return 0;
        sent += (size_t)r;
    }
    return 1;
}

static int send_frame(tls_conn *c, uint8_t type, uint16_t flags, uint32_t req, const uint8_t *payload, uint32_t len) {
    uint8_t hdr[HDR_LEN];
    memcpy(hdr, "RILL", 4); hdr[4] = 0x01; hdr[5] = type;
    put_u16(hdr + 6, flags); put_u32(hdr + 8, req); put_u32(hdr + 12, len);
    return write_all(c, hdr, HDR_LEN) && (len == 0 || write_all(c, payload, len));
}

static int send_error(tls_conn *c, uint32_t req, uint16_t status, const char *msg) {
    uint8_t buf[4 + 512]; size_t n = strlen(msg); if (n > 512) n = 512;
    put_u16(buf, status); put_u16(buf + 2, (uint16_t)n); memcpy(buf + 4, msg, n);
    return send_frame(c, T_ERROR, 0, req, buf, (uint32_t)(4 + n));
}

/* Fatal per §2 of connection.md: ERROR (request 0), then CLOSE, then shut down. */
static void fatal(tls_conn *c, uint16_t status, const char *msg) {
    send_error(c, 0, status, msg);
    send_frame(c, T_CLOSE, 0, 0, NULL, 0);
    tls_shutdown(c);
}

/* ---- path validation (§7.1) -------------------------------------------- */

static int path_valid(const uint8_t *s, size_t n) {
    if (n < 1 || n > MAX_PATH || s[0] != '/') return 0;
    if (n > 1 && s[n - 1] == '/') return 0;
    size_t seg = 0;
    for (size_t i = 1; i <= n; i++) {
        if (i == n || s[i] == '/') {
            if (i < n && seg == 0) return 0;                       /* "//" */
            if (seg == 1 && s[i - 1] == '.') return 0;             /* "." */
            if (seg == 2 && s[i - 1] == '.' && s[i - 2] == '.') return 0; /* ".." */
            seg = 0;
        } else {
            if (s[i] == 0) return 0;                               /* NUL */
            seg++;
        }
    }
    for (size_t i = 0; i < n; ) {                                  /* UTF-8 shape */
        uint8_t ch = s[i];
        size_t w = ch < 0x80 ? 1 : (ch >> 5) == 6 ? 2 : (ch >> 4) == 14 ? 3 : (ch >> 3) == 30 ? 4 : 0;
        if (!w || i + w > n) return 0;
        for (size_t k = 1; k < w; k++) if ((s[i + k] & 0xC0) != 0x80) return 0;
        i += w;
    }
    return 1;
}

/* ---- one connection ---------------------------------------------------- */

static void serve(tls_conn *c) {
    uint32_t last_req = 0;
    uint8_t hdr[HDR_LEN];
    for (;;) {
        if (!read_exact(c, hdr, HDR_LEN)) return;                  /* EOF or error */
        /* §3 validation order — magic, version, length, type, flags, then read. */
        if (memcmp(hdr, "RILL", 4) != 0) { fatal(c, S_PROTOCOL_MALFORMED, "bad magic"); return; }
        if (hdr[4] != 0x01) { fatal(c, S_UNSUPPORTED_VERSION, "version"); return; }
        uint8_t type = hdr[5]; uint16_t flags = get_u16(hdr + 6);
        uint32_t req = get_u32(hdr + 8), len = get_u32(hdr + 12);
        if (len > MAX_PAYLOAD) { fatal(c, S_FRAME_TOO_LARGE, "payload"); return; }
        if (type & 0x80) { fatal(c, S_PROTOCOL_MALFORMED, "server-direction frame from client"); return; }
        if (type != T_GET && type != T_HEAD && type != T_PING && type != T_CLOSE && type != T_GET_IF && type != T_ACTION) {
            fatal(c, S_UNKNOWN_FRAME_TYPE, "type"); return;
        }
        uint16_t crit = flags & 0xFF00;
        if (crit & ~KNOWN_CRITICAL) { fatal(c, S_UNKNOWN_CRITICAL_FLAG, "flag"); return; }
        if (crit && !(type == T_ACTION && crit == F_ACTION_CAS)) { fatal(c, S_PROTOCOL_MALFORMED, "flag on wrong type"); return; }
        /* bits 0–7 are ignorable; ACCEPT_ZSTD in particular we simply ignore. */

        uint8_t *payload = len ? malloc(len) : NULL;
        if (len && !read_exact(c, payload, len)) { free(payload); return; }

        if (type == T_CLOSE) { free(payload); tls_shutdown(c); return; }
        if (type == T_PING) {
            if (len > MAX_PING) { free(payload); fatal(c, S_PROTOCOL_MALFORMED, "ping"); return; }
            send_frame(c, T_PONG, 0, req, payload, len); free(payload); continue;
        }
        /* Request frames carry strictly increasing IDs from 1 (§6). */
        if (req == 0 || req <= last_req) { free(payload); fatal(c, S_PROTOCOL_MALFORMED, "request id"); return; }
        last_req = req;

        if (type == T_ACTION) { free(payload); send_error(c, req, S_NOT_FOUND, ""); continue; }

        /* GET / HEAD / GET_IF all begin with the §7.1 path payload. */
        if (len < 2) { free(payload); fatal(c, S_PROTOCOL_MALFORMED, "short path"); return; }
        uint16_t plen = get_u16(payload);
        size_t expect = 2u + plen + (type == T_GET_IF ? 33u : 0u);
        if (len != expect) { free(payload); fatal(c, S_PROTOCOL_MALFORMED, "path payload length"); return; }
        if (type == T_GET_IF && payload[2 + plen] != 0x01) { free(payload); fatal(c, S_PROTOCOL_MALFORMED, "hash algo"); return; }
        const uint8_t *path = payload + 2;
        if (!path_valid(path, plen)) { free(payload); fatal(c, S_PATH_INVALID, "path"); return; }

        int is_temp = (plen == 5 && memcmp(path, "/temp", 5) == 0) || (plen == 1 && path[0] == '/');
        if (!is_temp) { free(payload); send_error(c, req, S_NOT_FOUND, ""); continue; }

        size_t dlen; uint8_t *doc = build_document(&dlen);
        if (type == T_HEAD) {
            /* METADATA v2: size + reserved + hash algo + BLAKE3 of the bytes (§7.3). */
            uint8_t meta[43]; put_u64(meta, dlen); put_u16(meta + 8, 0); meta[10] = 0x01;
            blake3_hash(doc, dlen, meta + 11);
            send_frame(c, T_METADATA, 0, req, meta, 43);
            fprintf(stderr, "endpoint: req %u HEAD %.*s\n", req, (int)plen, (const char *)path);
        } else if (type == T_GET_IF) {
            /* "Send the resource unless its current bytes hash to this value"
             * (§7.1a). The viewer sent BLAKE3 of what it holds; if our fresh
             * document hashes the same, nothing has changed. */
            uint8_t now[BLAKE3_OUT_LEN]; blake3_hash(doc, dlen, now);
            if (memcmp(now, payload + 2 + plen + 1, BLAKE3_OUT_LEN) == 0) {
                send_frame(c, T_NOT_MODIFIED, 0, req, NULL, 0);
                fprintf(stderr, "endpoint: req %u GET_IF %.*s -> NOT_MODIFIED\n", req, (int)plen, (const char *)path);
            } else {
                send_frame(c, T_RESOURCE, 0, req, doc, (uint32_t)dlen);
                fprintf(stderr, "endpoint: req %u GET_IF %.*s -> RESOURCE (changed)\n", req, (int)plen, (const char *)path);
            }
        } else {
            send_frame(c, T_RESOURCE, 0, req, doc, (uint32_t)dlen);
            fprintf(stderr, "endpoint: req %u GET %.*s\n", req, (int)plen, (const char *)path);
        }
        free(doc); free(payload);
    }
}

int main(int argc, char **argv) {
    int port = argc > 1 ? atoi(argv[1]) : 7430;
    signal(SIGCHLD, SIG_IGN); /* reap forked connections automatically */

    tls_ctx *ctx = tls_server_new("cert.pem", "key.pem");
    if (!ctx) return 1;

    int lfd = socket(AF_INET, SOCK_STREAM, 0);
    int one = 1; setsockopt(lfd, SOL_SOCKET, SO_REUSEADDR, &one, sizeof one);
    struct sockaddr_in addr = { .sin_family = AF_INET, .sin_port = htons((uint16_t)port), .sin_addr.s_addr = htonl(INADDR_ANY) };
    if (bind(lfd, (struct sockaddr *)&addr, sizeof addr) < 0 || listen(lfd, 8) < 0) { perror("bind/listen"); return 1; }
    fprintf(stderr, "endpoint: serving /temp on port %d (TLS 1.3, ALPN rill/1)\n", port);

    for (;;) {
        int cfd = accept(lfd, NULL, NULL);
        if (cfd < 0) { if (errno == EINTR) continue; perror("accept"); break; }
        pid_t pid = fork();
        if (pid == 0) {
            close(lfd);
            setsockopt(cfd, IPPROTO_TCP, TCP_NODELAY, &one, sizeof one);
            tls_conn *c = tls_accept(ctx, cfd);
            if (c) { fprintf(stderr, "endpoint: session\n"); serve(c); tls_conn_free(c); }
            else { fprintf(stderr, "endpoint: handshake failed\n"); }
            close(cfd); _exit(0);
        }
        close(cfd);
    }
    tls_ctx_free(ctx);
    return 0;
}
