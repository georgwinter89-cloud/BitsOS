// BitsOS V2 Cobalt backend: settings, read once from the environment.

#include "starboard/shared/bitsos/fb/fb_config.h"

#include <stdlib.h>

namespace bitsos_fb {
namespace {

bool ParseSide(const std::string& s, int* out) {
  if (s.empty() || s.size() > 5) {
    return false;
  }
  int v = 0;
  for (char c : s) {
    if (c < '0' || c > '9') {
      return false;
    }
    v = v * 10 + (c - '0');
  }
  if (v < 1 || v > kMaxSide) {
    return false;
  }
  *out = v;
  return true;
}

std::vector<std::string> SplitColon(const std::string& s) {
  std::vector<std::string> parts;
  std::string cur;
  for (char c : s) {
    if (c == ':') {
      if (!cur.empty()) {
        parts.push_back(cur);
      }
      cur.clear();
    } else {
      cur.push_back(c);
    }
  }
  if (!cur.empty()) {
    parts.push_back(cur);
  }
  return parts;
}

}  // namespace

bool ParseSize(const std::string& s, int* w, int* h) {
  const size_t x = s.find_first_of("xX");
  if (x == std::string::npos) {
    return false;
  }
  int a, b;
  if (!ParseSide(s.substr(0, x), &a) || !ParseSide(s.substr(x + 1), &b)) {
    return false;
  }
  *w = a;
  *h = b;
  return true;
}

Config ReadConfig(GetEnvFn getenv_fn) {
  Config c;
  auto get = [&](const char* name) -> const char* {
    const char* v = getenv_fn(name);
    return (v && *v) ? v : nullptr;
  };
  auto ignore = [&](const char* name, const char* value) {
    c.ignored.push_back(std::string(name) + "=" + value);
  };
  auto flag = [&](const char* name, bool* out) {
    if (const char* v = get(name)) {
      const std::string s(v);
      if (s == "1" || s == "on" || s == "yes") {
        *out = true;
      } else if (s == "0" || s == "off" || s == "no") {
        *out = false;
      } else {
        ignore(name, v);
      }
    }
  };

  if (const char* v = get("BITSOS_BACKEND")) {
    c.fb_backend = std::string(v) == "fb";
  }
  if (const char* v = get("BITSOS_FB")) {
    c.fb_path = v;
  }
  if (const char* v = get("BITSOS_FB_FAKE")) {
    if (ParseSize(v, &c.fake_w, &c.fake_h)) {
      c.fb_fake = true;
    } else {
      ignore("BITSOS_FB_FAKE", v);
    }
  }
  if (const char* v = get("BITSOS_WINDOW")) {
    int w, h;
    if (ParseSize(v, &w, &h) && w <= kMaxWindowW && h <= kMaxWindowH) {
      c.window_w = w;
      c.window_h = h;
    } else {
      ignore("BITSOS_WINDOW", v);
    }
  }
  if (c.fb_fake) {
    // A fake screen means a test: real input devices only when named.
    c.evdev_patterns.clear();
    c.evdev_grab = false;
  }
  if (const char* v = get("BITSOS_EVDEV")) {
    std::vector<std::string> parts = SplitColon(v);
    if (parts.empty()) {
      ignore("BITSOS_EVDEV", v);
    } else {
      c.evdev_patterns = parts;
    }
  }
  flag("BITSOS_EVDEV_GRAB", &c.evdev_grab);
  if (const char* v = get("BITSOS_KEYMAP")) {
    const std::string s(v);
    if (s == "de") {
      c.keymap = Keymap::kDe;
    } else if (s == "us") {
      c.keymap = Keymap::kUs;
    } else {
      ignore("BITSOS_KEYMAP", v);
    }
  }
  if (const char* v = get("BITSOS_CURSOR")) {
    const std::string s(v);
    if (s == "auto") {
      c.cursor = CursorMode::kAuto;
    } else if (s == "on" || s == "1") {
      c.cursor = CursorMode::kOn;
    } else if (s == "off" || s == "0") {
      c.cursor = CursorMode::kOff;
    } else {
      ignore("BITSOS_CURSOR", v);
    }
  }
  flag("BITSOS_KD_GRAPHICS", &c.kd_graphics);
  return c;
}

}  // namespace bitsos_fb
