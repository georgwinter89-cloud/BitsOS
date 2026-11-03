e2fsprogs (CachyOS package licences: GPL, LGPL, MIT; the package installs no licence folder and no NOTICE).

The image carries one file of e2fsprogs: libcom_err.so.2, the common error-description library (e2fsprogs'
lib/et), which LibreOffice's libraries load through krb5 (curl's GSSAPI). Its copyright line as the package's own
header /usr/include/et/com_err.h states it: "Copyright 1988, Student Information Processing Board of the
Massachusetts Institute of Technology"; the header refers to "the documentation supplied with this package" for the
terms, which the package does not install. The terms of lib/et are in com_err-SIPB.txt in this folder (the
notice of e2fsprogs' lib/et sources, MIT-style). e2fsprogs' other parts (its programs under the GNU GPL version 2,
lib/ext2fs and lib/e2p under the GNU LGPL version 2) are not in the image; the texts of those licences are
/usr/share/licenses/spdx/GPL-2.0-only.txt and LGPL-2.0-only.txt.
