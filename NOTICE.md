# Licence

BitsOS is licensed under either of

- Apache License, Version 2.0 (`LICENSE-APACHE`)
- MIT license (`LICENSE-MIT`)

at your option. Unless you state otherwise, any contribution you submit for inclusion in BitsOS is dual licensed
as above, without any additional terms or conditions.

# Third-party parts

These parts carry code or data derived from other projects and keep those projects' notices.

| Where | Derived from | Licence used |
|---|---|---|
| `crates/dcn314-regs/src/generated/` | Linux `drivers/gpu/drm/amd/include/asic_reg/dcn/dcn_3_1_4_*.h`, Copyright 2022 Advanced Micro Devices, Inc. | MIT (notice below) |
| `crates/rtw89-core/`, `crates/drivers/rtw89/` | Linux `drivers/net/wireless/realtek/rtw89/`, Copyright(c) 2019-2020 Realtek Corporation, `GPL-2.0 OR BSD-3-Clause` | BSD-3-Clause (notice below) |
| `crates/display/src/font/cp437-8x16.bin` | the CP437 8x16 VGA font from SeaVGABIOS (see `crates/display/src/font/README.md`) | public domain |
| `user/sysroot/patches/libcxx/` | LLVM libc++ | Apache-2.0 WITH LLVM-exception |
| `user/ports/*` | the ported programs (BusyBox, Brave, Cobalt, ...): build recipes and patches for them | each program's own licence; a patch follows the licence of the program it changes |

Other drivers name Linux files and lines in their comments as the reference for a sequence or a register value;
their code is written for BitsOS and not copied from Linux.

## AMD register headers (MIT)

Copyright (C) 2022 Advanced Micro Devices, Inc.

Permission is hereby granted, free of charge, to any person obtaining a copy of this software and associated
documentation files (the "Software"), to deal in the Software without restriction, including without limitation
the rights to use, copy, modify, merge, publish, distribute, sublicense, and/or sell copies of the Software, and to
permit persons to whom the Software is furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all copies or substantial portions of
the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR IMPLIED, INCLUDING BUT NOT LIMITED TO
THE WARRANTIES OF MERCHANTABILITY, FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
COPYRIGHT HOLDER(S) OR AUTHOR(S) BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER LIABILITY, WHETHER IN AN ACTION OF
CONTRACT, TORT OR OTHERWISE, ARISING FROM, OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS
IN THE SOFTWARE.

## Realtek rtw89 (BSD-3-Clause)

Copyright(c) 2019-2020 Realtek Corporation

Redistribution and use in source and binary forms, with or without modification, are permitted provided that the
following conditions are met:

1. Redistributions of source code must retain the above copyright notice, this list of conditions and the following
   disclaimer.
2. Redistributions in binary form must reproduce the above copyright notice, this list of conditions and the
   following disclaimer in the documentation and/or other materials provided with the distribution.
3. Neither the name of the copyright holder nor the names of its contributors may be used to endorse or promote
   products derived from this software without specific prior written permission.

THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS "AS IS" AND ANY EXPRESS OR IMPLIED WARRANTIES,
INCLUDING, BUT NOT LIMITED TO, THE IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR A PARTICULAR PURPOSE ARE
DISCLAIMED. IN NO EVENT SHALL THE COPYRIGHT HOLDER OR CONTRIBUTORS BE LIABLE FOR ANY DIRECT, INDIRECT, INCIDENTAL,
SPECIAL, EXEMPLARY, OR CONSEQUENTIAL DAMAGES (INCLUDING, BUT NOT LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS OR
SERVICES; LOSS OF USE, DATA, OR PROFITS; OR BUSINESS INTERRUPTION) HOWEVER CAUSED AND ON ANY THEORY OF LIABILITY,
WHETHER IN CONTRACT, STRICT LIABILITY, OR TORT (INCLUDING NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE USE
OF THIS SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.
