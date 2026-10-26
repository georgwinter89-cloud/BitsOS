//===----------------------------------------------------------------------===//
//
// Part of the LLVM Project, under the Apache License v2.0 with LLVM Exceptions.
// See https://llvm.org/LICENSE.txt for license information.
// SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception
//
//===----------------------------------------------------------------------===//

#include <charconv>
#include <string.h>

// BitsOS V2: upstream parses floating point with LLVM libc's shared/ headers,
// which are not in the local llvm-project checkout; this version checks the
// from_chars grammar itself and hands the accepted characters to musl's
// exact strtod/strtof. Copied from V1's clib (werkzeuge/plattform/clib).
#include <errno.h>
#include <stdlib.h>
#include "include/to_chars_floating_point.h"

_LIBCPP_BEGIN_NAMESPACE_STD
_LIBCPP_BEGIN_EXPLICIT_ABI_ANNOTATIONS

#if _LIBCPP_AVAILABILITY_MINIMUM_HEADER_VERSION < 15

namespace __itoa {

_LIBCPP_DIAGNOSTIC_PUSH
_LIBCPP_CLANG_DIAGNOSTIC_IGNORED("-Wmissing-prototypes")
// These functions exist for ABI compatibility, so we don't ever want a declaration prior to the definition.
_LIBCPP_EXPORTED_FROM_ABI char* __u32toa(uint32_t value, char* buffer) noexcept { return __base_10_u32(buffer, value); }
_LIBCPP_EXPORTED_FROM_ABI char* __u64toa(uint64_t value, char* buffer) noexcept { return __base_10_u64(buffer, value); }
_LIBCPP_DIAGNOSTIC_POP

} // namespace __itoa

#endif // _LIBCPP_AVAILABILITY_MINIMUM_HEADER_VERSION < 15

// The original version of floating-point to_chars was written by Microsoft and
// contributed with the following license.

// Copyright (c) Microsoft Corporation.
// SPDX-License-Identifier: Apache-2.0 WITH LLVM-exception

// This implementation is dedicated to the memory of Mary and Thavatchai.

to_chars_result to_chars(char* __first, char* __last, float __value) {
  return _Floating_to_chars<_Floating_to_chars_overload::_Plain>(__first, __last, __value, chars_format{}, 0);
}

to_chars_result to_chars(char* __first, char* __last, double __value) {
  return _Floating_to_chars<_Floating_to_chars_overload::_Plain>(__first, __last, __value, chars_format{}, 0);
}

to_chars_result to_chars(char* __first, char* __last, long double __value) {
  return _Floating_to_chars<_Floating_to_chars_overload::_Plain>(
      __first, __last, static_cast<double>(__value), chars_format{}, 0);
}

to_chars_result to_chars(char* __first, char* __last, float __value, chars_format __fmt) {
  return _Floating_to_chars<_Floating_to_chars_overload::_Format_only>(__first, __last, __value, __fmt, 0);
}

to_chars_result to_chars(char* __first, char* __last, double __value, chars_format __fmt) {
  return _Floating_to_chars<_Floating_to_chars_overload::_Format_only>(__first, __last, __value, __fmt, 0);
}

to_chars_result to_chars(char* __first, char* __last, long double __value, chars_format __fmt) {
  return _Floating_to_chars<_Floating_to_chars_overload::_Format_only>(
      __first, __last, static_cast<double>(__value), __fmt, 0);
}

to_chars_result to_chars(char* __first, char* __last, float __value, chars_format __fmt, int __precision) {
  return _Floating_to_chars<_Floating_to_chars_overload::_Format_precision>(
      __first, __last, __value, __fmt, __precision);
}

to_chars_result to_chars(char* __first, char* __last, double __value, chars_format __fmt, int __precision) {
  return _Floating_to_chars<_Floating_to_chars_overload::_Format_precision>(
      __first, __last, __value, __fmt, __precision);
}

to_chars_result to_chars(char* __first, char* __last, long double __value, chars_format __fmt, int __precision) {
  return _Floating_to_chars<_Floating_to_chars_overload::_Format_precision>(
      __first, __last, static_cast<double>(__value), __fmt, __precision);
}

static bool __bos_word(const char* __p, const char* __last, const char* __w) {
  for (; *__w; ++__w, ++__p)
    if (__p >= __last || (*__p | 32) != *__w)
      return false;
  return true;
}

static bool __bos_digit(char __c, bool __hex) {
  return (__c >= '0' && __c <= '9') || (__hex && (__c | 32) >= 'a' && (__c | 32) <= 'f');
}

// from_chars's grammar first (no space, no '+', no "0x"; the exponent as the
// format says), then only those characters to strtod/strtof
template <class _Fp>
__from_chars_result<_Fp> __from_chars_floating_point(
    _LIBCPP_NOESCAPE const char* __first, _LIBCPP_NOESCAPE const char* __last, chars_format __fmt) {
  __from_chars_result<_Fp> __r = {0, 0, errc::invalid_argument};
  const char* __p = __first;
  bool __neg      = __p < __last && *__p == '-';
  if (__neg)
    ++__p;
  if (__p < __last && ((*__p | 32) == 'i' || (*__p | 32) == 'n')) {
    const char* __q;
    _Fp __v;
    if (__bos_word(__p, __last, "infinity") || __bos_word(__p, __last, "inf")) {
      __q = __p + (__bos_word(__p, __last, "infinity") ? 8 : 3);
      __v = static_cast<_Fp>(__builtin_inf());
    } else if (__bos_word(__p, __last, "nan")) {
      __q = __p + 3;
      if (__q < __last && *__q == '(') {
        const char* __c = __q + 1;
        while (__c < __last && (__bos_digit(*__c, false) || ((*__c | 32) >= 'a' && (*__c | 32) <= 'z') || *__c == '_'))
          ++__c;
        if (__c < __last && *__c == ')')
          __q = __c + 1;
      }
      __v = static_cast<_Fp>(__builtin_nan(""));
    } else
      return __r;
    __r = {__neg ? -__v : __v, __q - __first, errc{}};
    return __r;
  }
  bool __hex        = __fmt == chars_format::hex;
  bool __exp_ok     = __hex || (__fmt & chars_format::scientific) == chars_format::scientific;
  bool __exp_needed = !__hex && (__fmt & chars_format::fixed) != chars_format::fixed;
  const char* __m   = __p;
  size_t __digits   = 0;
  for (; __m < __last && __bos_digit(*__m, __hex); ++__m)
    ++__digits;
  if (__m < __last && *__m == '.')
    for (++__m; __m < __last && __bos_digit(*__m, __hex); ++__m)
      ++__digits;
  if (!__digits)
    return __r;
  const char* __end = __m;
  if (__exp_ok && __m < __last && (*__m | 32) == (__hex ? 'p' : 'e')) {
    const char* __e = __m + 1;
    if (__e < __last && (*__e == '+' || *__e == '-'))
      ++__e;
    if (__e < __last && __bos_digit(*__e, false)) {
      while (__e < __last && __bos_digit(*__e, false))
        ++__e;
      __end = __e;
    }
  }
  if (__exp_needed && __end == __m)
    return __r;
  size_t __n = __end - __p, __at = 0;
  char __small[512];
  char* __buf = __n + 4 <= sizeof __small ? __small : static_cast<char*>(malloc(__n + 4));
  if (!__buf)
    return __r;
  if (__neg)
    __buf[__at++] = '-';
  if (__hex) {
    __buf[__at++] = '0';
    __buf[__at++] = 'x';
  }
  memcpy(__buf + __at, __p, __n);
  __buf[__at + __n] = 0;
  int __saved = errno;
  errno       = 0;
  _Fp __v;
  if constexpr (sizeof(_Fp) == sizeof(float))
    __v = strtof(__buf, nullptr);
  else
    __v = strtod(__buf, nullptr);
  __r.__n     = __end - __first;
  __r.__ec    = errno == ERANGE ? errc::result_out_of_range : errc{};
  __r.__value = __v;
  errno       = __saved;
  if (__buf != __small)
    free(__buf);
  return __r;
}

template __from_chars_result<float> __from_chars_floating_point(
    _LIBCPP_NOESCAPE const char* __first, _LIBCPP_NOESCAPE const char* __last, chars_format __fmt);

template __from_chars_result<double> __from_chars_floating_point(
    _LIBCPP_NOESCAPE const char* __first, _LIBCPP_NOESCAPE const char* __last, chars_format __fmt);

_LIBCPP_END_EXPLICIT_ABI_ANNOTATIONS
_LIBCPP_END_NAMESPACE_STD
