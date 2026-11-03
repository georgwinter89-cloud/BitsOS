LO-1: licence texts of named CachyOS packages that install none themselves, for the libraries the image carries
(host/xtask/src/package.rs, licence_texts). A folder here is used only for a package of that name, and only when the
host has no /usr/share/licenses/<package>/ of its own; there is no fallback for SPDX texts in general.
  e2fsprogs/   libcom_err.so.2 (by ldd of LibreOffice's libraries, through krb5): README.txt and com_err-SIPB.txt
  libassuan/   FSFULLR.txt, the one licence pacman names for libassuan without a text on CachyOS
