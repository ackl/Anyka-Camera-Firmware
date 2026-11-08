/* camtls — opaque TLS client handle for camrtsp. See camtls.c for the why. */
#ifndef CAMTLS_H
#define CAMTLS_H

#include <stddef.h>

/* Return codes for camtls_read/camtls_write. Negative so they cannot be
 * confused with a byte count. */
#define CAMTLS_E_EOF (-1)    /* peer closed cleanly */
#define CAMTLS_E_WANT (-2)   /* socket timed out / would block, not fatal */
#define CAMTLS_E_IO (-3)     /* protocol or socket failure */
#define CAMTLS_E_CLOSED (-4) /* handle already closed */

/*
 * Handshake over an already-connected fd. Verifies the peer certificate
 * against ca_pem (a PEM bundle) and against hostname (SNI + CN/SAN).
 * Returns an opaque handle, or NULL with a human-readable reason in err.
 * Takes ownership of nothing: the caller still closes fd.
 */
void *camtls_open(int fd, const char *hostname, const char *ca_pem,
		  size_t ca_len, char *err, size_t errlen);

int camtls_read(void *h, void *buf, size_t len);
int camtls_write(void *h, const void *buf, size_t len);

/* "TLSv1.2" / "TLS-ECDHE-RSA-WITH-AES-128-GCM-SHA256", for the log line. */
const char *camtls_version(void *h);
const char *camtls_ciphersuite(void *h);

void camtls_close(void *h);

#endif /* CAMTLS_H */
