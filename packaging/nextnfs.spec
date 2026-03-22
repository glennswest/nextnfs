Name:           nextnfs
Version:        %{_version}
Release:        1%{?dist}
Summary:        High-performance NFSv4.0 server
License:        MIT
URL:            https://github.com/glennswest/nextnfs

Source0:        nextnfs
Source1:        nextnfs.toml
Source2:        nextnfs.service

%description
NextNFS is a high-performance standalone NFSv4.0 server over a real filesystem.
Static musl binary — no shared library dependencies.

%install
install -D -m 0755 %{SOURCE0} %{buildroot}/usr/bin/nextnfs
install -D -m 0644 %{SOURCE1} %{buildroot}/etc/nextnfs/nextnfs.toml
install -D -m 0644 %{SOURCE2} %{buildroot}/usr/lib/systemd/system/nextnfs.service

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
%config(noreplace) /etc/nextnfs/nextnfs.toml
/usr/lib/systemd/system/nextnfs.service

%changelog
