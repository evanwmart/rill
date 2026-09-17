CC ?= gcc
CFLAGS ?= -O2 -Wall -Wextra -std=c11
LDLIBS = -lssl -lcrypto -lm

# The endpoint: protocol + document (endpoint.c), our BLAKE3, and the TLS
# seam's host implementation. The MCU port swaps tls_openssl.c for
# tls_mbedtls.c and rebuilds the same endpoint.c.
endpoint: endpoint.c tls_openssl.c blake3.c tls.h blake3.h
	$(CC) $(CFLAGS) -o $@ endpoint.c tls_openssl.c blake3.c $(LDLIBS)

# BLAKE3 self-test: published vectors, plus `./blake3_test FILE` to hash a
# file for comparison against an external oracle.
blake3_test: blake3.c blake3.h
	$(CC) $(CFLAGS) -DBLAKE3_SELFTEST -o $@ blake3.c

test: blake3_test
	./blake3_test

clean:
	rm -f endpoint blake3_test

.PHONY: test clean
