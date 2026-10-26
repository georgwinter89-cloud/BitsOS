# user/sysroot: C and C++ for BitsOS V2 programs

V2 runs Linux programs. C and C++ programs are built **static-pie** against **musl 1.2.6** with **libc++,
libc++abi and libunwind** (LLVM 23), so the same file runs on CachyOS and on V2. Rust programs use the
stable `x86_64-unknown-linux-musl` target (it also makes static-pie), so V2's ELF loader needs one path only:
an `ET_DYN` file without `PT_INTERP` that relocates itself (musl's `rcrt1.o`, Rust's self-contained crt).

## Files

| Path | What |
|---|---|
| `build.sh` | Builds everything into `out/` (git-ignored). Idempotent: a second run with nothing changed takes about 1 s. `--clean` starts over. At most 8 parallel jobs (`V2_JOBS` can lower it). |
| `mirror.py` | Copies a read-only source tree into `out/src/` with LF line endings (the sources came from Windows with CRLF); rewrites a file only when it changed and keeps its time, so nothing is rebuilt needlessly. |
| `config/__config_site` | libc++'s configuration (musl, pthreads, filesystem, locale, wide chars on; time zone database off). |
| `patches/libcxx/charconv.cpp` | libc++'s `from_chars` for `float`/`double` on musl's `strtod`/`strtof` (upstream needs LLVM libc's `shared/` headers, not in the local checkout). Taken from V1's clib. |
| `bin/v2-cc`, `bin/v2-c++` | The compilers to use. Any clang flags pass through. |
| `v2-toolchain.cmake` | CMake toolchain file for ports (`-DCMAKE_TOOLCHAIN_FILE=…`). |
| `tests/run.sh` | Builds the C and C++ hello programs with the wrappers and through CMake, checks that they are static-pie without an interpreter, runs them on the host and compares the output. |

## What `build.sh` produces

`out/sysroot/usr/` (prefix `/usr`, like a normal Linux sysroot):

- `lib/`: `libc.a` (+ `libm.a`, `libpthread.a`, … as musl's empty stubs), `crt1.o`, `rcrt1.o`, `Scrt1.o`,
  `crti.o`, `crtn.o`, `libunwind.a`, `libc++abi.a`, `libc++.a` (libc++abi's objects are inside too, so
  `-lc++` is enough). Everything is compiled with `-fPIC`, as static-pie needs.
- `include/`: musl's headers, Linux UAPI headers (`linux/`, `asm/`, `asm-generic/`, `drm/`, `sound/`, …,
  copied from the host's `linux-api-headers` package; musl keeps its own `scsi/` headers), and
  `include/c++/v1/` with libc++'s headers, `__config_site`, `__assertion_handler`, `cxxabi.h`.

Sources (never written): musl from `werkzeuge/plattform/clib/fremd/musl`, libc++/libc++abi/libunwind from
`werkzeuge/plattform/clib/fremd/llvm-project` (override with `V2_MUSL_SRC`, `V2_LLVM_SRC`, `V2_KHDR`).
The compiler-rt builtins and `crtbegin`/`crtend` come from the host clang's resource directory
(`/usr/lib/clang/23/lib/linux/`).

**Why ninja without CMake for the LLVM part:** the local `llvm-project` is a sparse, blob-less checkout that
holds only `libcxx`, `libcxxabi`, `libunwind` and `compiler-rt/lib/builtins`; LLVM's runtimes build needs
`runtimes/` and the top-level `cmake/` modules, which are not on disk and may not be downloaded. `build.sh`
therefore writes `out/build/llvm/build.ninja` itself, with the source lists and flags of the CMake runtimes
build for Linux + musl (LLVM 23). musl uses its own `configure` + `make`.

## The wrappers

```
v2-cc  -O2 -o hello hello.c
v2-c++ -O2 -std=c++20 -o hello hello.cpp      # iostream, std::thread, exceptions, RTTI
v2-c++ -c a.cpp && v2-c++ -o a a.o            # separate compile and link work too
```

They call the host `clang`/`clang++` with `--target=x86_64-unknown-linux-musl --sysroot=out/sysroot
--gcc-toolchain=out/sysroot/usr -fPIE`, and when linking `-static-pie -fuse-ld=lld -rtlib=compiler-rt
--unwindlib=libunwind` (C++ also `-stdlib=libc++`). `v2-c++` uses `-nostdinc++` plus the sysroot's
`c++/v1`, so a host libc++ can never slip in; `--gcc-toolchain` keeps the host GCC directory out of the
library search. Link order clang produces: `rcrt1.o crti.o crtbegin … -lc++ -lm builtins.a -l:libunwind.a
-lc crtend crtn.o`. `V2_SYSROOT` points the wrappers at another sysroot.

## How xtask calls it (the desk wires this in; `host/xtask` is not changed here)

`cargo xtask user` (V2-LANES UL-1):

1. `user/sysroot/build.sh` (exit code ≠ 0 = stop; prints the library sizes). Clean ≈ 10 s, no-op ≈ 1 s
   on the Gaming-PC.
2. C/C++ programs: build with `CC=user/sysroot/bin/v2-cc CXX=user/sysroot/bin/v2-c++` (make-based ports,
   e.g. busybox in UL-3: `make CC=… HOSTCC=cc`), or CMake with
   `-DCMAKE_TOOLCHAIN_FILE=user/sysroot/v2-toolchain.cmake`.
3. Rust programs: `cargo build --release --offline --target x86_64-unknown-linux-musl
   --manifest-path user/Cargo.toml` (stable channel, selected by `user/rust-toolchain.toml`; not a fixed version), with
   `CARGO_TARGET_DIR=<worktree>/target`.
4. `user/sysroot/tests/run.sh` as the toolchain's own check (runs build.sh twice and requires the second
   run to do no work, builds the C, C++ and Rust tests, checks each binary is static-pie without INTERP,
   NEEDED or glibc/libstdc++ traces, and compares the output) (exit code 0 = all good); on CachyOS the
   programs run directly, on V2 the same files go into the initramfs (UL-5).

Host packages needed (all official CachyOS/Arch): `clang`, `lld`, `llvm`, `compiler-rt`, `ninja`, `make`,
`rsync`, `python`, `linux-api-headers`, and `cmake` for the CMake check.

## Limits and choices

- Static only (no `libc.so`, no dynamic loader): V2 runs dynamic glibc programs later through their own
  `ld.so` (AB-8, UL-5), not through this sysroot.
- Not built: `libc++experimental` (time zone database, so `std::chrono::tzdb` is unavailable), sanitizers,
  libc++'s C++ modules.
- No debug info in the libraries (`-O2`); programs choose their own `-g`.
- Wrappers: `-shared` is refused (no shared libraries in V2's program world); `-r` (partial link) works
  without `-static-pie`; `-M`/`-MM` count as compile-only. `V2_JOBS` is clamped to 1..8.
- `out/sysroot/VERSIONS` records musl, libc++, the Linux UAPI header package and the host clang version
  (the UAPI headers follow the host's `linux-api-headers`; pin them with `V2_KHDR=<copy>` if needed).
- `__muloti4` and friends come from compiler-rt builtins (`-rtlib=compiler-rt`); libc++ also carries
  `filesystem/int128_builtins` as upstream does, so libc++ itself does not depend on that.
- Licences: musl MIT; libc++, libc++abi, libunwind, compiler-rt Apache-2.0 WITH LLVM-exception; Linux UAPI
  headers GPL-2.0 WITH Linux-syscall-note (headers only, allowed for programs of any licence).
