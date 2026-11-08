/*
 * camtls — the smallest possible TLS client that the camera can actually link.
 *
 * Why this exists: the board's own libssl.so.1.0.0 is uClibc (see
 * analysis/encoder-ownership.md for why that matters); our Rust is
 * armv5te-unknown-linux-musleabi, so it cannot use it. Pure-Rust TLS would mean
 * hand-writing X.509 + RSA/ECDSA verification. mbedTLS has all of that, in C,
 * with no assembly requirement, so it builds for ARM926EJ-S.
 *
 * The Rust side (camrtsp/src/tls.rs) sees exactly six functions and an opaque
 * handle. All mbedTLS struct layout stays on this side of the fence.
 *
 * Layer 4 of the stack is ours already (upload.rs does its own HTTP/1.1), so
 * mbedtls_net is deliberately not built: we hand it a connected fd.
 */

#include <errno.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include <sys/socket.h>
#include <unistd.h>

#include "mbedtls/ctr_drbg.h"
#include "mbedtls/debug.h"
#include "mbedtls/entropy.h"
#include "mbedtls/error.h"
#include "mbedtls/ssl.h"
#include "mbedtls/x509_crt.h"

#include "camtls.h"

/* mbedtls_net is not built (we own the socket), so these two error codes are
 * not in any header we include. Same values as mbedtls/net_sockets.h. */
#ifndef MBEDTLS_ERR_NET_SEND_FAILED
#define MBEDTLS_ERR_NET_SEND_FAILED -0x004C
#endif
#ifndef MBEDTLS_ERR_NET_RECV_FAILED
#define MBEDTLS_ERR_NET_RECV_FAILED -0x004E
#endif

typedef struct {
	mbedtls_ssl_context ssl;
	mbedtls_ssl_config conf;
	mbedtls_x509_crt ca;
	mbedtls_entropy_context entropy;
	mbedtls_ctr_drbg_context drbg;
	int fd;
	int in_use;
} camtls_ctx;

/* The fd is blocking with SO_RCVTIMEO/SO_SNDTIMEO set by the caller (upload.rs
 * sets both). A timeout therefore surfaces as EAGAIN. Do not turn that into
 * WANT_READ: mbedtls would spin forever re-calling us. A timeout is fatal. */

static int bio_send(void *p, const unsigned char *buf, size_t len)
{
	camtls_ctx *c = (camtls_ctx *)p;
	ssize_t n = send(c->fd, buf, len, MSG_NOSIGNAL);
	if (n < 0)
		return MBEDTLS_ERR_NET_SEND_FAILED;
	return (int)n;
}

static int bio_recv(void *p, unsigned char *buf, size_t len)
{
	camtls_ctx *c = (camtls_ctx *)p;
	ssize_t n = recv(c->fd, buf, len, 0);
	if (n < 0)
		return MBEDTLS_ERR_NET_RECV_FAILED;
	if (n == 0)
		return MBEDTLS_ERR_SSL_PEER_CLOSE_NOTIFY;
	return (int)n;
}

#if defined(MBEDTLS_DEBUG_C)
static void camtls_dbg(void *ctx, int level, const char *file, int line,
		       const char *msg)
{
	(void)ctx;
	(void)level;
	fprintf(stderr, "camtls %s:%d %s", file, line, msg);
}
#endif

static void put_err(char *err, size_t errlen, const char *what, int rc)
{
	if (err == NULL || errlen == 0)
		return;
#if defined(MBEDTLS_ERROR_C)
	char detail[128];
	mbedtls_strerror(rc, detail, sizeof detail);
	snprintf(err, errlen, "%s: %s (-0x%04x)", what, detail, (unsigned)-rc);
#else
	snprintf(err, errlen, "%s: rc -0x%04x", what, (unsigned)-rc);
#endif
}

/* Verify-result explanation, e.g. "The certificate has expired". */
static void put_verify_err(char *err, size_t errlen, uint32_t flags)
{
	if (err == NULL || errlen == 0)
		return;
	int n = snprintf(err, errlen, "certificate verify failed (flags 0x%08x)",
			 (unsigned)flags);
	if (n < 0 || (size_t)n >= errlen)
		return;
	char info[192];
	info[0] = '\0';
	if (mbedtls_x509_crt_verify_info(info, sizeof info, ": ", flags) > 0) {
		size_t room = errlen - (size_t)n - 1;
		strncat(err, info, room);
	}
}

void *camtls_open(int fd, const char *hostname, const char *ca_pem,
		  size_t ca_len, char *err, size_t errlen)
{
	if (err != NULL && errlen > 0)
		err[0] = '\0';
	if (ca_pem == NULL || ca_len == 0 || hostname == NULL) {
		put_err(err, errlen, "camtls_open", MBEDTLS_ERR_SSL_BAD_INPUT_DATA);
		return NULL;
	}

	camtls_ctx *c = (camtls_ctx *)calloc(1, sizeof *c);
	if (c == NULL) {
		if (err && errlen)
			snprintf(err, errlen, "out of memory");
		return NULL;
	}
	c->fd = fd;

	mbedtls_ssl_init(&c->ssl);
	mbedtls_ssl_config_init(&c->conf);
	mbedtls_x509_crt_init(&c->ca);
	mbedtls_entropy_init(&c->entropy);
	mbedtls_ctr_drbg_init(&c->drbg);

	int rc;

	/* mbedTLS 2.28 sniffs PEM vs DER with `buf[buflen-1] == '\0'` (see
	 * x509_crt_parse in library/x509_crt.c). Rust's include_bytes! gives a
	 * slice with no terminator, so a perfectly good PEM bundle is mistaken
	 * for DER and rejected as "invalid format". Copy and terminate here;
	 * callers should never have to know about this. */
	{
		unsigned char *term = malloc(ca_len + 1);
		if (term == NULL) {
			if (err && errlen)
				snprintf(err, errlen, "out of memory");
			goto fail;
		}
		memcpy(term, ca_pem, ca_len);
		term[ca_len] = 0;
		rc = mbedtls_x509_crt_parse(&c->ca, term, ca_len + 1);
		free(term);
		if (rc != 0) {
			put_err(err, errlen, "ca bundle", rc);
			goto fail;
		}
	}

	rc = mbedtls_ctr_drbg_seed(&c->drbg, mbedtls_entropy_func, &c->entropy,
				   (const unsigned char *)"camrtsp", 7);
	if (rc != 0) {
		put_err(err, errlen, "drbg seed", rc);
		goto fail;
	}

	rc = mbedtls_ssl_config_defaults(&c->conf, MBEDTLS_SSL_IS_CLIENT,
					 MBEDTLS_SSL_TRANSPORT_STREAM,
					 MBEDTLS_SSL_PRESET_DEFAULT);
	if (rc != 0) {
		put_err(err, errlen, "config defaults", rc);
		goto fail;
	}

	/* CAMTLS_DEBUG=1 gives mbedTLS's own handshake trace on stderr. The board
	 * has no packet capture worth the name, so when an upload fails this is
	 * how you find out which alert came back. */
	if (getenv("CAMTLS_DEBUG") != NULL) {
		mbedtls_ssl_conf_dbg(&c->conf, camtls_dbg, NULL);
		mbedtls_debug_set_threshold(4);
	}

	mbedtls_ssl_conf_authmode(&c->conf, MBEDTLS_SSL_VERIFY_REQUIRED);
	mbedtls_ssl_conf_ca_chain(&c->conf, &c->ca, NULL);
	mbedtls_ssl_conf_rng(&c->conf, mbedtls_ctr_drbg_random, &c->drbg);

	/* Only TLS 1.2 is compiled in (see build.sh); say so in a comment, because
	 * a future reader will wonder why there is no max_version call. */

	rc = mbedtls_ssl_setup(&c->ssl, &c->conf);
	if (rc != 0) {
		put_err(err, errlen, "ssl setup", rc);
		goto fail;
	}

	rc = mbedtls_ssl_set_hostname(&c->ssl, hostname);
	if (rc != 0) {
		put_err(err, errlen, "set hostname", rc);
		goto fail;
	}

	mbedtls_ssl_set_bio(&c->ssl, c, bio_send, bio_recv, NULL);

	while ((rc = mbedtls_ssl_handshake(&c->ssl)) != 0) {
		if (rc != MBEDTLS_ERR_SSL_WANT_READ &&
		    rc != MBEDTLS_ERR_SSL_WANT_WRITE) {
			put_err(err, errlen, "handshake", rc);
			goto fail;
		}
	}

	/* mbedtls only reports the verify result after a mandatory-verify
	 * handshake in some versions; be explicit. */
	uint32_t flags = mbedtls_ssl_get_verify_result(&c->ssl);
	if (flags != 0) {
		put_verify_err(err, errlen, flags);
		goto fail;
	}

	c->in_use = 1;
	return c;

fail:
	mbedtls_ssl_free(&c->ssl);
	mbedtls_ssl_config_free(&c->conf);
	mbedtls_x509_crt_free(&c->ca);
	mbedtls_ctr_drbg_free(&c->drbg);
	mbedtls_entropy_free(&c->entropy);
	free(c);
	return NULL;
}

int camtls_read(void *h, void *buf, size_t len)
{
	camtls_ctx *c = (camtls_ctx *)h;
	if (c == NULL || !c->in_use)
		return CAMTLS_E_CLOSED;
	if (len > (size_t)INT32_MAX)
		len = INT32_MAX;

	int rc = mbedtls_ssl_read(&c->ssl, (unsigned char *)buf, len);
	if (rc == 0 || rc == MBEDTLS_ERR_SSL_PEER_CLOSE_NOTIFY)
		return CAMTLS_E_EOF;
	if (rc < 0) {
		if (rc == MBEDTLS_ERR_SSL_WANT_READ ||
		    rc == MBEDTLS_ERR_SSL_WANT_WRITE)
			return CAMTLS_E_WANT; /* caller's socket timed out */
		return CAMTLS_E_IO;
	}
	return rc;
}

int camtls_write(void *h, const void *buf, size_t len)
{
	camtls_ctx *c = (camtls_ctx *)h;
	if (c == NULL || !c->in_use)
		return CAMTLS_E_CLOSED;

	const unsigned char *p = (const unsigned char *)buf;
	size_t done = 0;
	while (done < len) {
		int rc = mbedtls_ssl_write(&c->ssl, p + done, len - done);
		if (rc < 0) {
			if (rc == MBEDTLS_ERR_SSL_WANT_WRITE ||
			    rc == MBEDTLS_ERR_SSL_WANT_READ)
				return CAMTLS_E_WANT;
			return CAMTLS_E_IO;
		}
		if (rc == 0)
			return CAMTLS_E_IO;
		done += (size_t)rc;
	}
	return (int)(done > (size_t)INT32_MAX ? INT32_MAX : done);
}

const char *camtls_version(void *h)
{
	camtls_ctx *c = (camtls_ctx *)h;
	if (c == NULL)
		return "";
	const char *v = mbedtls_ssl_get_version(&c->ssl);
	return v ? v : "";
}

const char *camtls_ciphersuite(void *h)
{
	camtls_ctx *c = (camtls_ctx *)h;
	if (c == NULL)
		return "";
	const char *s = mbedtls_ssl_get_ciphersuite(&c->ssl);
	return s ? s : "";
}

void camtls_close(void *h)
{
	camtls_ctx *c = (camtls_ctx *)h;
	if (c == NULL)
		return;
	if (c->in_use) {
		/* Best-effort close_notify: a truncated upload is caught by the
		 * receiver's sha256 anyway, so never block on this. */
		c->in_use = 0;
		mbedtls_ssl_close_notify(&c->ssl);
	}
	mbedtls_ssl_free(&c->ssl);
	mbedtls_ssl_config_free(&c->conf);
	mbedtls_x509_crt_free(&c->ca);
	mbedtls_ctr_drbg_free(&c->drbg);
	mbedtls_entropy_free(&c->entropy);
	free(c);
}
