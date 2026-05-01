Name:           nextnfs
Version:        %{_version}
Release:        1%{?dist}
Summary:        High-performance NFSv4 server
License:        MIT
URL:            https://github.com/glennswest/nextnfs

Source0:        nextnfs
Source1:        nextnfs.toml
Source2:        nextnfs.service
Source3:        nextnfs-stress

%description
NextNFS is a high-performance standalone NFSv4 server (v4.0/v4.1/v4.2)
with support for pNFS, delegations, RPCSEC_GSS, RPC-over-TLS, RDMA framing,
per-export QoS rate limiting, quota enforcement, and OverlayFS backends.
Static musl binary — no shared library dependencies.

%install
install -D -m 0755 %{SOURCE0} %{buildroot}/usr/bin/nextnfs
install -D -m 0644 %{SOURCE1} %{buildroot}/etc/nextnfs/nextnfs.toml
install -D -m 0644 %{SOURCE2} %{buildroot}/usr/lib/systemd/system/nextnfs.service
install -D -m 0755 %{SOURCE3} %{buildroot}/usr/bin/nextnfs-stress
install -d -m 0755 %{buildroot}/var/lib/nextnfs
install -d -m 0755 %{buildroot}/export

%post
systemctl daemon-reload
systemctl enable nextnfs.service

%preun
if [ "$1" = 0 ]; then
    systemctl stop nextnfs.service || true
    systemctl disable nextnfs.service || true
fi

%postun
systemctl daemon-reload

%files
/usr/bin/nextnfs
/usr/bin/nextnfs-stress
%config(noreplace) /etc/nextnfs/nextnfs.toml
/usr/lib/systemd/system/nextnfs.service
%dir /var/lib/nextnfs
%dir /export

%changelog
* Wed Apr 02 2026 Glenn West <glenn@nextnfs.dev> - 0.12.0-1
- NFSv4.1 sessions (EXCHANGE_ID, CREATE_SESSION, SEQUENCE, etc.)
- NFSv4.2 operations (COPY, SEEK, ALLOCATE)
- pNFS layout operations (LAYOUTGET, LAYOUTCOMMIT, LAYOUTRETURN, GETDEVICEINFO)
- RPCSEC_GSS / Kerberos 5 credential parsing (krb5, krb5i, krb5p)
- SECINFO_NO_NAME for pseudo-root security negotiation
- RPC-over-TLS (RFC 9289) and RDMA framing (RFC 8166)
- Per-export QoS rate limiting and quota enforcement
- OverlayFS VFS backend with dm-verity integrity
- Hardened systemd service unit
- Session trunking (multiple TCP connections per session)
- Delegation support (DELEGPURGE, DELEGRETURN)
- State recovery with periodic JSON snapshots
