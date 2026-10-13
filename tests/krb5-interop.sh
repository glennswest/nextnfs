#!/usr/bin/env bash
# MIT Kerberos interop test for RPCSEC_GSS (#106), unprivileged.
#
# Starts a throw-away MIT KDC (krb5kdc on a high port, realm NEXTNFS.TEST),
# makes one nfs/ service principal per AES enctype and puts them in a
# keytab, starts nextnfs with that keytab (one AUTH_SYS export and one
# krb5/krb5i/krb5p export), gets alice a TGT with kinit, and runs
# tests/krb5-interop (MIT's libgssapi_krb5 as the initiator) once per
# service principal. Needs krb5-server, krb5-workstation, cargo.
#
# usage: tests/krb5-interop.sh            (from the repo root)
#   NEXTNFS=path/to/nextnfs to use an already built binary.
set -euo pipefail

root=$(cd "$(dirname "$0")/.." && pwd)
work=$(mktemp -d "${TMPDIR:-/tmp}/krb5-interop.XXXXXX")
kdc_pid=""
nfs_pid=""
cleanup() {
    [ -n "$nfs_pid" ] && kill "$nfs_pid" 2>/dev/null || true
    [ -n "$kdc_pid" ] && kill "$kdc_pid" 2>/dev/null || true
    rm -rf "$work"
}
trap cleanup EXIT

for tool in krb5kdc kdb5_util kadmin.local kinit; do
    command -v "$tool" >/dev/null || { echo "SKIP: $tool not installed"; exit 77; }
done

realm=NEXTNFS.TEST
kdc_port=$((20000 + RANDOM % 10000))
nfs_port=$((30000 + RANDOM % 10000))
api_port=$((nfs_port + 1))
enctypes="aes256-cts-hmac-sha1-96 aes128-cts-hmac-sha1-96 aes256-cts-hmac-sha384-192 aes128-cts-hmac-sha256-128"

export KRB5_CONFIG=$work/krb5.conf
export KRB5_KDC_PROFILE=$work/kdc.conf
export KRB5CCNAME=FILE:$work/ccache
cat >"$KRB5_CONFIG" <<EOF
[libdefaults]
    default_realm = $realm
    dns_lookup_kdc = false
    dns_lookup_realm = false
    dns_canonicalize_hostname = false
    rdns = false
    udp_preference_limit = 1
    permitted_enctypes = $enctypes
[realms]
    $realm = {
        kdc = 127.0.0.1:$kdc_port
    }
EOF
cat >"$KRB5_KDC_PROFILE" <<EOF
[kdcdefaults]
    kdc_ports = $kdc_port
    kdc_tcp_ports = $kdc_port
[realms]
    $realm = {
        database_name = $work/principal
        key_stash_file = $work/stash
        acl_file = $work/kadm5.acl
        supported_enctypes = $(for e in $enctypes; do printf '%s:normal ' "$e"; done)
        max_life = 1h
    }
[logging]
    kdc = FILE:$work/kdc.log
EOF
: >"$work/kadm5.acl"

kdb5_util -r "$realm" create -s -P "master-$RANDOM-$RANDOM" >/dev/null
kadmin.local -r "$realm" -q "addprinc -pw alice-password alice" >/dev/null
n=0
principals=()
for e in $enctypes; do
    n=$((n + 1))
    p="nfs/server$n.nextnfs.test"
    kadmin.local -r "$realm" -q "addprinc -randkey -e $e:normal $p" >/dev/null
    kadmin.local -r "$realm" -q "ktadd -k $work/nfs.keytab -e $e:normal $p" >/dev/null
    principals+=("$p@$realm:$e")
done
krb5kdc -n -r "$realm" &
kdc_pid=$!
sleep 1
echo alice-password | kinit alice >/dev/null
klist | sed -n 1,3p

# nextnfs
if [ -z "${NEXTNFS:-}" ]; then
    (cd "$root" && cargo build -q --bin nextnfs)
    NEXTNFS=$root/target/debug/nextnfs
fi
mkdir -p "$work/plain" "$work/secure"
for i in $(seq 1 200); do : >"$work/secure/file-with-a-fairly-long-name-$i"; done
cat >"$work/nextnfs.toml" <<EOF
[server]
listen = "127.0.0.1:$nfs_port"
api_listen = "127.0.0.1:$api_port"
krb5_keytab = "$work/nfs.keytab"

[[exports]]
name = "plain"
path = "$work/plain"

[[exports]]
name = "secure"
path = "$work/secure"
squash = "none"
sec = ["krb5p", "krb5i", "krb5"]
EOF
"$NEXTNFS" serve --config "$work/nextnfs.toml" >"$work/nextnfs.log" 2>&1 &
nfs_pid=$!
for _ in $(seq 1 50); do
    (echo >/dev/tcp/127.0.0.1/$nfs_port) 2>/dev/null && break
    sleep 0.2
done

(cd "$root/tests/krb5-interop" && cargo build -q)
client=$root/tests/krb5-interop/target/debug/krb5-interop
status=0
for entry in "${principals[@]}"; do
    p=${entry%%:*}
    e=${entry#*:}
    echo "== $e ($p)"
    if ! "$client" "127.0.0.1:$nfs_port" "$p" secure; then
        status=1
    fi
done
if [ $status -ne 0 ]; then
    echo "---- nextnfs log"; tail -50 "$work/nextnfs.log"
    echo "---- kdc log"; tail -20 "$work/kdc.log"
    exit 1
fi
grep -c "RPCSEC_GSS context established" "$work/nextnfs.log" | sed 's/^/contexts established: /'
echo "krb5 interop: PASS"
