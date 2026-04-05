Name:           nextnfs-tests
Version:        %{_version}
Release:        1%{?dist}
Summary:        Shell integration tests for nextnfs
License:        MIT
URL:            https://github.com/glennswest/nextnfs
Requires:       nfs-utils coreutils util-linux

%description
Shell-based NFS integration tests for nextnfs. Includes basic functional
tests, edge case tests, stress tests, and a test runner that mounts NFS
exports and runs all suites.

%install
install -d -m 0755 %{buildroot}/usr/share/nextnfs/tests
install -d -m 0755 %{buildroot}/usr/bin
install -m 0755 %{_sourcedir}/helpers.sh          %{buildroot}/usr/share/nextnfs/tests/
install -m 0755 %{_sourcedir}/nfs4_basic.sh       %{buildroot}/usr/share/nextnfs/tests/
install -m 0755 %{_sourcedir}/nfs4_edge.sh        %{buildroot}/usr/share/nextnfs/tests/
install -m 0755 %{_sourcedir}/nfs4_stress.sh      %{buildroot}/usr/share/nextnfs/tests/
install -m 0755 %{_sourcedir}/nfs41_session.sh    %{buildroot}/usr/share/nextnfs/tests/
install -m 0755 %{_sourcedir}/nfs_bench_suite.sh  %{buildroot}/usr/share/nextnfs/tests/
install -m 0755 %{_sourcedir}/nfs_integrity.sh    %{buildroot}/usr/share/nextnfs/tests/
install -m 0755 %{_sourcedir}/nfs_performance.sh  %{buildroot}/usr/share/nextnfs/tests/
install -m 0755 %{_sourcedir}/nextnfs-run-tests   %{buildroot}/usr/bin/nextnfs-run-tests

%files
/usr/share/nextnfs/tests/helpers.sh
/usr/share/nextnfs/tests/nfs4_basic.sh
/usr/share/nextnfs/tests/nfs4_edge.sh
/usr/share/nextnfs/tests/nfs4_stress.sh
/usr/share/nextnfs/tests/nfs41_session.sh
/usr/share/nextnfs/tests/nfs_bench_suite.sh
/usr/share/nextnfs/tests/nfs_integrity.sh
/usr/share/nextnfs/tests/nfs_performance.sh
/usr/bin/nextnfs-run-tests

%changelog
* Sun Apr 05 2026 Glenn West <glenn@nextnfs.dev> - 0.13.1-1
- Initial test RPM with basic, edge, stress, session, benchmark suites
