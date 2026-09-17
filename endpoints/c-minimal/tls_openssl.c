/* tls.h over OpenSSL 3 — the host implementation. The MCU port replaces this
 * file with tls_mbedtls.c; nothing above the seam changes. */
#include "tls.h"
#include <openssl/err.h>
#include <openssl/ssl.h>
#include <stdio.h>
#include <stdlib.h>

struct tls_ctx { SSL_CTX *ssl; };
struct tls_conn { SSL *ssl; };

static int alpn_select(SSL *ssl, const unsigned char **out, unsigned char *outlen,
                       const unsigned char *in, unsigned int inlen, void *arg) {
    (void)ssl; (void)arg;
    static const unsigned char want[] = "\x06rill/1";
    if (SSL_select_next_proto((unsigned char **)out, outlen, want, sizeof(want) - 1, in, inlen) == OPENSSL_NPN_NEGOTIATED)
        return SSL_TLSEXT_ERR_OK;
    return SSL_TLSEXT_ERR_ALERT_FATAL; /* ALPN is required: no "rill/1" → no session */
}

static int accept_any_client_cert(int ok, X509_STORE_CTX *ctx) { (void)ok; (void)ctx; return 1; }

tls_ctx *tls_server_new(const char *cert_path, const char *key_path) {
    SSL_CTX *ctx = SSL_CTX_new(TLS_server_method());
    if (!ctx) return NULL;
    SSL_CTX_set_min_proto_version(ctx, TLS1_3_VERSION);
    if (SSL_CTX_use_certificate_file(ctx, cert_path, SSL_FILETYPE_PEM) != 1 ||
        SSL_CTX_use_PrivateKey_file(ctx, key_path, SSL_FILETYPE_PEM) != 1) {
        fprintf(stderr, "tls: need %s + %s (run ./gen-cert.sh)\n", cert_path, key_path);
        ERR_print_errors_fp(stderr);
        SSL_CTX_free(ctx);
        return NULL;
    }
    SSL_CTX_set_alpn_select_cb(ctx, alpn_select, NULL);
    SSL_CTX_set_verify(ctx, SSL_VERIFY_PEER, accept_any_client_cert); /* request, don't require */
    tls_ctx *t = malloc(sizeof *t); t->ssl = ctx; return t;
}

tls_conn *tls_accept(tls_ctx *ctx, int fd) {
    SSL *ssl = SSL_new(ctx->ssl);
    SSL_set_fd(ssl, fd);
    if (SSL_accept(ssl) != 1) {
        ERR_print_errors_fp(stderr);
        SSL_free(ssl);
        return NULL;
    }
    tls_conn *c = malloc(sizeof *c); c->ssl = ssl; return c;
}

int tls_read(tls_conn *c, void *buf, size_t n) { return SSL_read(c->ssl, buf, (int)n); }
int tls_write(tls_conn *c, const void *buf, size_t n) { return SSL_write(c->ssl, buf, (int)n); }
void tls_shutdown(tls_conn *c) { SSL_shutdown(c->ssl); }
void tls_conn_free(tls_conn *c) { if (c) { SSL_free(c->ssl); free(c); } }
void tls_ctx_free(tls_ctx *ctx) { if (ctx) { SSL_CTX_free(ctx->ssl); free(ctx); } }
