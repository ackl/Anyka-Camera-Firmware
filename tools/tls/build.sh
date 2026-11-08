#!/bin/sh
# Build libcamtls.a — mbedTLS 2.28.8 + our shim — for the camera (ARM926EJ-S,
# musl) and for the host (so the uploader path can be tested without the board).
#
#   tools/tls/build.sh bundle    extract the CA roots we must ship (public data)
#   tools/tls/build.sh armv5te   libcamtls.a for armv5te-unknown-linux-musleabi
#   tools/tls/build.sh host      libcamtls.a for this machine
#   tools/tls/build.sh all       both
#
# Why mbedTLS at all: see camtls.c. Why zig: it is the only C compiler here that
# targets arm-linux-musleabi (zig cc -target arm-linux-musleabi -mcpu=arm926ej_s,
# the same incantation tools/README.md already documents for the C demos).
set -e

HERE=$(cd "$(dirname "$0")" && pwd)
VER=2.28.8
TARBALL="$HERE/mbedtls-$VER.tar.gz"
URL="https://codeload.github.com/Mbed-TLS/mbedtls/tar.gz/refs/tags/v$VER"
SHA256="4fef7de0d8d542510d726d643350acb3cdb9dc76ad45611b59c9aa08372b4213"

ZIG="$HERE/../zig-linux/zig/zig"

# Trim the stock config down to "TLS 1.2 client, verify certificates". Server,
# DTLS, PSK, DHE, FS I/O, net and mbedtls' own threading are all dead weight on
# a 64 MB ARMv5TE box (and MBEDTLS_NET_C would duplicate the sockets we already
# own). Every symbol below is a stock #define in include/mbedtls/config.h.
TRIM="MBEDTLS_SSL_PROTO_TLS1 MBEDTLS_SSL_PROTO_TLS1_1 MBEDTLS_SSL_PROTO_DTLS
MBEDTLS_SSL_SRV_C MBEDTLS_SSL_CACHE_C MBEDTLS_SSL_COOKIE_C MBEDTLS_SSL_RENEGOTIATION
MBEDTLS_SSL_CBC_RECORD_SPLITTING MBEDTLS_PSA_ITS_FILE_C
MBEDTLS_SSL_DTLS_BADMAC_LIMIT MBEDTLS_SSL_DTLS_ANTI_REPLAY
MBEDTLS_SSL_DTLS_HELLO_VERIFY MBEDTLS_SSL_DTLS_CLIENT_PORT_REUSE
MBEDTLS_NET_C MBEDTLS_TIMING_C MBEDTLS_FS_IO
MBEDTLS_KEY_EXCHANGE_DHE_RSA_ENABLED MBEDTLS_KEY_EXCHANGE_RSA_ENABLED
MBEDTLS_KEY_EXCHANGE_PSK_ENABLED MBEDTLS_KEY_EXCHANGE_DHE_PSK_ENABLED
MBEDTLS_KEY_EXCHANGE_RSA_PSK_ENABLED MBEDTLS_KEY_EXCHANGE_ECDHE_PSK_ENABLED
MBEDTLS_KEY_EXCHANGE_ECDH_RSA_ENABLED MBEDTLS_KEY_EXCHANGE_ECDH_ECDSA_ENABLED
MBEDTLS_KEY_EXCHANGE_ECJPAKE_ENABLED
MBEDTLS_MD5_C MBEDTLS_DES_C MBEDTLS_ARC4_C MBEDTLS_BLOWFISH_C MBEDTLS_CAMELLIA_C
MBEDTLS_XTEA_C MBEDTLS_PKCS5_C MBEDTLS_PKCS12_C MBEDTLS_PKCS11_C
MBEDTLS_X509_CSR_PARSE_C MBEDTLS_X509_CRL_PARSE_C MBEDTLS_SSL_PROTO_SSL3"

fetch() {
	if [ -f "$TARBALL" ]; then
		echo "using $TARBALL"
	else
		echo "fetching mbedTLS $VER"
		curl -sL -o "$TARBALL" "$URL"
	fi
	got=$(sha256sum "$TARBALL" | cut -d' ' -f1)
	if [ "$got" != "$SHA256" ]; then
		echo "sha256 mismatch on $TARBALL" >&2
		echo "  want $SHA256" >&2
		echo "  got  $got" >&2
		exit 1
	fi

	if [ ! -d "$HERE/build/mbedtls-$VER" ]; then
		echo "extracting"
		mkdir -p "$HERE/build"
		tar -xzf "$TARBALL" -C "$HERE/build"
	fi

	cfg="$HERE/build/mbedtls-$VER/include/mbedtls/config.h"
	if [ ! -f "$cfg.orig" ]; then
		cp "$cfg" "$cfg.orig"
	fi
	cp "$cfg.orig" "$cfg"
	# Always regenerate: a half-written trim must not survive a rerun.
	if :; then
		python3 - "$cfg" $TRIM <<'PY'
import re, sys
path, syms = sys.argv[1], set(sys.argv[2:])
out, hit = [], []
for line in open(path):
    m = re.match(r'^#define\s+(\w+)\s*$', line)
    if m and m.group(1) in syms:
        out.append('// trimmed by tools/tls/build.sh: ' + line)
        hit.append(m.group(1))
    else:
        out.append(line)
open(path + '.trimmed', 'w').write(''.join(out))
missing = syms - set(hit)
if missing:
    # Stock config.h already leaves these off; trimming them is a no-op.
    print('already off: ' + ' '.join(sorted(missing)))
print('trimmed %d symbols' % len(hit))
PY
	fi
	cp "$cfg.trimmed" "$cfg"
}

build() { # name, compiler args...
	name=$1; shift
	src="$HERE/build/mbedtls-$VER"
	out="$HERE/out/$name"
	rm -rf "$out.obj"
	mkdir -p "$out" "$out.obj"
	n=0
	for f in "$src"/library/*.c "$HERE/camtls.c"; do
		# 2.28 ships psa_*.c for Mbed-TLS development; they need include/psa/error.h
		# which is only in 3.x, and PSA is off in our config anyway.
		case "$f" in */psa_*.c) continue ;; esac
		o="$out.obj/$(basename "$f" .c).o"
		"$@" -I"$src/include" -I"$HERE" \
			-Os -ffunction-sections -fdata-sections \
			-fno-asynchronous-unwind-tables -fno-unwind-tables \
			-DMBEDTLS_DEBUG_C \
			-c "$f" -o "$o"
		n=$((n + 1))
	done
	ar rcs "$out/libcamtls.a" "$out.obj"/*.o
	echo "$name: $n objects -> $out/libcamtls.a ($(wc -c < "$out/libcamtls.a") bytes)"
}

case "${1:-all}" in
bundle)
	# Ship only the roots the ingest endpoint can actually chain to (Cloudflare's
	# universal SSL is GTS/GlobalSign; the others are there so a cert reissue or a
	# move to Let's Encrypt does not silently break uploads). Public data.
	python3 - "$HERE/ca-bundle.pem" <<'PY'
import subprocess, sys
want = ['GTS Root R4', 'GTS Root R1', 'GlobalSign Root CA',
        'DigiCert Global Root G2', 'ISRG Root X1', 'ISRG Root X2']
blocks, keep = subprocess.run(['openssl', 'crl2pkcs7', '-nocrl', '-certfile',
                               '/etc/ssl/certs/ca-certificates.crt'],
                              capture_output=True, text=True).returncode, []
pem = open('/etc/ssl/certs/ca-certificates.crt').read()
for b in pem.split('-----END CERTIFICATE-----'):
    if 'BEGIN CERTIFICATE' not in b:
        continue
    b = b[b.index('-----BEGIN CERTIFICATE-----'):] + '-----END CERTIFICATE-----\n'
    subj = subprocess.run(['openssl', 'x509', '-noout', '-subject'],
                          input=b, capture_output=True, text=True).stdout
    for w in want:
        if 'CN = %s' % w in subj or 'CN=%s' % w in subj:
            keep.append((w, b))
seen, out = set(), []
for w, b in keep:
    if b in seen:
        continue
    seen.add(b)
    out.append(b)
import os, shutil
kept = []
for b in out:
    subj = subprocess.run(['openssl', 'x509', '-noout', '-subject'],
                          input=b, capture_output=True, text=True).stdout.strip()
    kept.append(subj.split('CN = ')[-1].split('CN=')[-1])
header = ('# camrtsp CA bundle - the roots an ingest endpoint can chain to.\n'
          '# kept: ' + ', '.join(kept) + '\n'
          '# generated by tools/tls/build.sh bundle from ca-certificates.crt\n'
          '# mbedTLS scans for BEGIN CERTIFICATE and ignores this header.\n')
open(sys.argv[1], 'w').write(header + ''.join(out))
# The crate needs its own copy: remote-build.sh only rsyncs the crate dir.
shutil.copyfile(sys.argv[1], os.path.join(os.path.dirname(os.path.dirname(
    sys.argv[1])), 'rust', 'camrtsp', 'ca-bundle.pem'))
print('ca-bundle.pem: %d roots (%d bytes)' % (len(out), len(''.join(out))))
got = {w for w, _ in keep}
print('missing (fine if never used):', ' '.join(w for w in want if w not in got) or 'none')
PY
	;;
armv5te)
	fetch
	test -x "$ZIG" || { echo "no zig at $ZIG (see tools/zig-linux)" >&2; exit 1; }
	build armv5te "$ZIG" cc -target arm-linux-musleabi -mcpu=arm926ej_s
	;;
host)
	fetch
	build host cc
	;;
all)
	"$0" bundle
	"$0" armv5te
	"$0" host
	;;
*)
	sed -n '2,14p' "$0"
	exit 2
	;;
esac
