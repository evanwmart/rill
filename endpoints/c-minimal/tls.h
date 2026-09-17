/* The TLS seam. Everything the endpoint needs from a TLS stack, and nothing
 * more — so the microcontroller port supplies tls_mbedtls.c against this
 * header and the protocol code above it does not change.
 *
 * Contract (specs/security.md §3, connection.md §2):
 *   - TLS 1.3 only.
 *   - ALPN is required and must negotiate exactly "rill/1".
 *   - The server presents its self-signed certificate.
 *   - A client certificate is requested but not required (absent → the
 *     Anonymous identity). This endpoint never inspects one.
 */
#ifndef RILL_TLS_H
#define RILL_TLS_H
#include <stddef.h>

typedef struct tls_ctx tls_ctx;   /* server-side configuration, one per process */
typedef struct tls_conn tls_conn; /* one accepted, handshaken session */

/* Load the PEM certificate + key and configure TLS 1.3 / ALPN / cert request.
 * NULL on failure (the implementation reports why to stderr). */
tls_ctx *tls_server_new(const char *cert_path, const char *key_path);

/* Perform the handshake on an accepted TCP socket. NULL on failure. */
tls_conn *tls_accept(tls_ctx *ctx, int fd);

/* Positive byte count, or <= 0 on EOF / error. May return short. */
int tls_read(tls_conn *c, void *buf, size_t n);
int tls_write(tls_conn *c, const void *buf, size_t n);

/* Send close_notify; then free. */
void tls_shutdown(tls_conn *c);
void tls_conn_free(tls_conn *c);
void tls_ctx_free(tls_ctx *ctx);

#endif
