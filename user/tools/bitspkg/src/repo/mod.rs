//! PK-6a: the Arch and CachyOS repositories as sources (plan/PACKAGES.md 6.5; model pacman
//! 7.1, libalpm): sources and mirror lists, the download through fetch, gzip/zstd and tar, the sync database,
//! its index, freshness, search and pacman's version order.
//! PK-6c: the resolver (resolve/), one closure per program.
//! PK-6d: a pacman package becomes a BitsOS package (plan 1.6, 1.7): fetchpkg (download, sha256, signature),
//! pkgs (unpack into `/apps/.pkgs`, .MTREE check), alpmhook and trigger (hooks emulated in the staging view),
//! tree (the per-program root tree, manifest format 2) and apply (the whole install, its report).
//! PK-6e: update (re-resolve every program, D5's schedule) and ownpart (keyring and mirror list packages).

pub mod alpmhook;
pub mod apply;
pub mod cli;
pub mod dbsig;
pub mod download;
pub mod fetchpkg;
pub mod index;
pub mod meta;
pub mod ownpart;
pub mod mtree;
pub mod pkgs;
pub mod refresh;
pub mod resolve;
pub mod search;
pub mod sources;
pub mod syncdb;
pub mod tar;
pub mod tree;
pub mod trigger;
pub mod unpack;
pub mod update;
pub mod vercmp;
pub mod xxh64;
