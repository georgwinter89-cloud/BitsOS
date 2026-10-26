// Host tests for the BitsOS V2 Cobalt framebuffer backend (YT-1).
// Built and run by host/tests/yt/run-host-tests.sh; prints "ok <name>" per
// test and "ALL PASSED" at the end, exits 1 on the first failure.

#include <fcntl.h>
#include <limits.h>
#include <poll.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <signal.h>
#include <sys/stat.h>
#include <sys/wait.h>
#include <unistd.h>

#include <linux/input-event-codes.h>

#include <chrono>
#include <cmath>
#include <functional>
#include <map>
#include <random>
#include <string>
#include <vector>

#include "starboard/shared/bitsos/fb/av_stats.h"
#include "starboard/shared/bitsos/fb/console_guard.h"
#include "starboard/shared/bitsos/fb/evdev_decoder.h"
#include "starboard/shared/bitsos/fb/evdev_devices.h"
#include "starboard/shared/bitsos/fb/fb_compositor.h"
#include "starboard/shared/bitsos/fb/fb_config.h"
#include "starboard/shared/bitsos/fb/fb_device.h"
#include "starboard/shared/bitsos/fb/fb_pixels.h"
#include "starboard/shared/bitsos/fb/fb_yuv.h"

using namespace bitsos_fb;

#define CHECK(c)                                                       \
  do {                                                                 \
    if (!(c)) {                                                        \
      fprintf(stderr, "FAIL %s:%d: %s\n", __FILE__, __LINE__, #c);     \
      exit(1);                                                         \
    }                                                                  \
  } while (0)

namespace {

std::string g_tmp;
std::map<std::string, std::string> g_env;

const char* FakeEnv(const char* name) {
  auto it = g_env.find(name);
  return it == g_env.end() ? nullptr : it->second.c_str();
}

void Ok(const char* name) {
  printf("ok %s\n", name);
}

uint32_t At(const FbDevice& fb, int x, int y) {
  uint32_t v;
  memcpy(&v, fb.pixels() + static_cast<size_t>(y) * fb.pitch() + x * 4, 4);
  return v & 0xFFFFFF;
}

void TestRects() {
  Rect a = MakeRect(0, 0, 10, 10), b = MakeRect(5, 5, 10, 10);
  CHECK(Intersect(a, b) == MakeRect(5, 5, 5, 5));
  CHECK(Union(a, b) == MakeRect(0, 0, 15, 15));
  CHECK(Intersect(a, MakeRect(20, 20, 5, 5)).Empty());
  CHECK(Union(Rect(), a) == a);
  // Hostile sizes must not overflow.
  Rect big = MakeRect(INT_MAX - 5, INT_MAX - 5, INT_MAX, INT_MAX);
  CHECK(Intersect(big, a).Empty());
  Rect u = Union(big, MakeRect(INT_MIN, INT_MIN, 10, 10));
  CHECK(!u.Empty());
  CHECK(Intersect(MakeRect(-5, -5, 10, 10), a) == MakeRect(0, 0, 5, 5));
  CHECK(Intersect(MakeRect(0, 0, -3, 4), a).Empty());
  Ok("rects");
}

void TestConfig() {
  int w, h;
  CHECK(ParseSize("2560x1440", &w, &h) && w == 2560 && h == 1440);
  CHECK(!ParseSize("x1", &w, &h));
  CHECK(!ParseSize("0x10", &w, &h));
  CHECK(!ParseSize("99999x10", &w, &h));
  CHECK(!ParseSize("12a4x10", &w, &h));
  CHECK(!ParseSize("", &w, &h));
  g_env.clear();
  Config c = ReadConfig(FakeEnv);
  CHECK(!c.fb_backend && c.fb_path == "/dev/fb0" && !c.fb_fake);
  CHECK(c.window_w == 1920 && c.window_h == 1080);
  CHECK(c.evdev_patterns.size() == 1 && c.evdev_grab);
  g_env = {{"BITSOS_BACKEND", "fb"},
           {"BITSOS_FB", "/tmp/x"},
           {"BITSOS_FB_FAKE", "800x600"},
           {"BITSOS_WINDOW", "1280x720"},
           {"BITSOS_EVDEV", "/a:/b*::"},
           {"BITSOS_EVDEV_GRAB", "0"},
           {"BITSOS_KEYMAP", "us"},
           {"BITSOS_CURSOR", "on"}};
  c = ReadConfig(FakeEnv);
  CHECK(c.fb_backend && c.fb_path == "/tmp/x" && c.fb_fake);
  CHECK(c.fake_w == 800 && c.fake_h == 600);
  CHECK(c.window_w == 1280 && c.window_h == 720);
  CHECK(c.evdev_patterns.size() == 2 && c.evdev_patterns[1] == "/b*");
  CHECK(!c.evdev_grab && c.keymap == Keymap::kUs);
  CHECK(c.cursor == CursorMode::kOn && c.ignored.empty());
  g_env = {{"BITSOS_WINDOW", "3840x2160"}, {"BITSOS_CURSOR", "maybe"},
           {"BITSOS_EVDEV", ":::"}};
  c = ReadConfig(FakeEnv);
  CHECK(c.window_w == 1920 && c.ignored.size() == 3);
  // A fake screen never reads or grabs real input unless told to.
  g_env = {{"BITSOS_FB_FAKE", "800x600"}};
  c = ReadConfig(FakeEnv);
  CHECK(c.fb_fake && c.evdev_patterns.empty() && !c.evdev_grab);
  g_env = {{"BITSOS_FB_FAKE", "800x600"}, {"BITSOS_EVDEV", "/tmp/fifo"}};
  c = ReadConfig(FakeEnv);
  CHECK(c.evdev_patterns.size() == 1 && c.evdev_patterns[0] == "/tmp/fifo");
  CHECK(!c.evdev_grab);
  g_env = {{"BITSOS_FB_FAKE", "800x600"}, {"BITSOS_EVDEV_GRAB", "1"}};
  c = ReadConfig(FakeEnv);
  CHECK(c.evdev_patterns.empty() && c.evdev_grab);
  g_env = {{"BITSOS_FB_FAKE", "junk"}};  // not understood: real screen rules
  c = ReadConfig(FakeEnv);
  CHECK(!c.fb_fake && c.evdev_patterns.size() == 1 && c.evdev_grab);
  g_env.clear();
  Ok("config");
}

void TestFit() {
  Placement p = FitWindow(1920, 1080, 2560, 1440);
  CHECK(p.x == 0 && p.y == 0 && p.w == 2560 && p.h == 1440);
  p = FitWindow(1920, 1080, 1280, 1024);
  CHECK(p.w == 1280 && p.h == 720 && p.x == 0 && p.y == 152);
  p = FitWindow(1920, 1080, 1080, 1920);
  CHECK(p.w == 1080 && p.y > 0);
  p = FitWindow(0, 1080, 100, 100);
  CHECK(p.w == 0);
  Ok("fit");
}

void TestFormats() {
  PixelFormat f;
  CHECK(f.Validate(nullptr) && f.IsXrgb8888());
  CHECK(f.Pack(0x123456) == 0xFF123456u);
  PixelFormat rgb565;
  rgb565.bits_per_pixel = 16;
  rgb565.red_offset = 11;
  rgb565.red_length = 5;
  rgb565.green_offset = 5;
  rgb565.green_length = 6;
  rgb565.blue_offset = 0;
  rgb565.blue_length = 5;
  CHECK(rgb565.Validate(nullptr));
  CHECK(rgb565.Pack(0xFFFFFF) == 0xFFFF);
  CHECK(rgb565.Pack(0xFF0000) == 0xF800);
  PixelFormat bgr;
  bgr.red_offset = 0;
  bgr.blue_offset = 16;
  CHECK(bgr.Validate(nullptr) && !bgr.IsXrgb8888());
  CHECK((bgr.Pack(0x112233) & 0xFFFFFF) == 0x332211);
  PixelFormat bad;
  bad.bits_per_pixel = 24;
  const char* why = nullptr;
  CHECK(!bad.Validate(&why) && why);
  bad.bits_per_pixel = 32;
  bad.red_length = 10;
  CHECK(!bad.Validate(nullptr));
  Ok("formats");
}

void TestScaler() {
  const int sw = 1920, sh = 1080, dw = 2560, dh = 1440;
  Scaler s;
  CHECK(!s.Init(0, 10, FitWindow(10, 10, 10, 10)));
  CHECK(s.Init(sw, sh, FitWindow(sw, sh, dw, dh)));
  std::vector<uint32_t> src(static_cast<size_t>(sw) * sh, 0x00336699);
  std::vector<uint8_t> fb(static_cast<size_t>(dw) * dh * 4, 0);
  PixelFormat f;
  s.Render(src.data(), sw, MakeRect(0, 0, dw, dh), fb.data(), dw * 4, f);
  for (size_t i = 0; i < fb.size(); i += 4 * 997) {
    uint32_t v;
    memcpy(&v, &fb[i], 4);
    CHECK((v & 0xFFFFFF) == 0x336699);
  }
  // A horizontal ramp stays monotonic after scaling.
  for (int y = 0; y < sh; ++y) {
    for (int x = 0; x < sw; ++x) {
      src[static_cast<size_t>(y) * sw + x] = static_cast<uint32_t>(x * 255 / (sw - 1));
    }
  }
  s.Render(src.data(), sw, MakeRect(0, 0, dw, dh), fb.data(), dw * 4, f);
  int last = -1;
  for (int x = 0; x < dw; ++x) {
    int v = fb[(static_cast<size_t>(700) * dw + x) * 4];
    CHECK(v >= last);
    last = v;
  }
  CHECK(fb[(static_cast<size_t>(700) * dw + 0) * 4] == 0);
  CHECK(fb[(static_cast<size_t>(700) * dw + dw - 1) * 4] == 255);
  // Damage mapping: every screen pixel that changes when a source rectangle
  // changes lies inside SrcToDst of that rectangle.
  std::mt19937 rng(7);
  const int w2 = 160, h2 = 90, dw2 = 213, dh2 = 120;  // 4/3, odd sizes
  Scaler s2;
  CHECK(s2.Init(w2, h2, FitWindow(w2, h2, dw2, dh2)));
  for (int round = 0; round < 300; ++round) {
    std::vector<uint32_t> a(static_cast<size_t>(w2) * h2);
    for (auto& v : a) v = rng() & 0xFFFFFF;
    std::vector<uint32_t> b = a;
    Rect r = MakeRect(rng() % w2, rng() % h2, 1 + rng() % 20, 1 + rng() % 20);
    Rect rc = Intersect(r, MakeRect(0, 0, w2, h2));
    for (int y = rc.y; y < rc.y + rc.h; ++y)
      for (int x = rc.x; x < rc.x + rc.w; ++x) b[y * w2 + x] ^= 0x808080;
    std::vector<uint8_t> fa(static_cast<size_t>(dw2) * dh2 * 4), fbb(fa.size());
    const Placement& p = s2.placement();
    Rect all = MakeRect(0, 0, dw2, dh2);
    s2.Render(a.data(), w2, all, fa.data(), dw2 * 4, f);
    s2.Render(b.data(), w2, all, fbb.data(), dw2 * 4, f);
    Rect d = s2.SrcToDst(r);
    for (int y = 0; y < dh2; ++y)
      for (int x = 0; x < dw2; ++x) {
        size_t i = (static_cast<size_t>(y) * dw2 + x) * 4;
        if (memcmp(&fa[i], &fbb[i], 4) != 0) {
          CHECK(x >= d.x && x < d.x + d.w && y >= d.y && y < d.y + d.h);
        }
      }
    (void)p;
  }
  CHECK(s2.SrcToDst(MakeRect(-50, -50, 10, 10)).Empty());
  const Placement& p2 = s2.placement();
  CHECK(s2.SrcToDst(MakeRect(0, 0, 1000000, 1000000)) ==
        MakeRect(p2.x, p2.y, p2.w, p2.h));
  // No seams: rendering in random pieces gives the same bytes as one full
  // render (the SSE2 4/3 kernel and the edge loop must agree), for 4/3 and
  // for an odd ratio.
  for (int pass = 0; pass < 2; ++pass) {
    const int pw = pass ? 213 : 256, ph = pass ? 120 : 144;
    const int qw = pass ? 160 : 192, qh = pass ? 90 : 108;
    Scaler s3;
    CHECK(s3.Init(qw, qh, FitWindow(qw, qh, pw, ph)));
    std::vector<uint32_t> img(static_cast<size_t>(qw) * qh);
    for (auto& v : img) v = rng();
    std::vector<uint8_t> whole(static_cast<size_t>(pw) * ph * 4, 0), parts(whole.size(), 0);
    s3.Render(img.data(), qw, MakeRect(0, 0, pw, ph), whole.data(), pw * 4, f);
    for (int i = 0; i < 400; ++i) {
      Rect r = MakeRect(rng() % pw, rng() % ph, 1 + rng() % 40, 1 + rng() % 7);
      s3.Render(img.data(), qw, r, parts.data(), pw * 4, f);
    }
    for (int y = 0; y < ph; ++y)  // fill what the pieces missed
      s3.Render(img.data(), qw, MakeRect(0, y, pw, 1), parts.data(), pw * 4, f);
    CHECK(whole == parts);
    // Against a plain reference: bilinear within 2 of the exact value.
    const Placement& pp = s3.placement();
    for (int i = 0; i < 2000; ++i) {
      int x = rng() % pp.w, y = rng() % pp.h;
      double sx = std::max(0.0, (x + 0.5) * qw / pp.w - 0.5);
      double sy = std::max(0.0, (y + 0.5) * qh / pp.h - 0.5);
      int ax = std::min(static_cast<int>(sx), qw - 1), ay = std::min(static_cast<int>(sy), qh - 1);
      int bx = std::min(ax + 1, qw - 1), by = std::min(ay + 1, qh - 1);
      double fx = sx - ax, fy = sy - ay;
      for (int sh = 0; sh < 24; sh += 8) {
        auto ch = [&](int xx, int yy) { return (img[yy * qw + xx] >> sh) & 0xFF; };
        double want = (ch(ax, ay) * (1 - fx) + ch(bx, ay) * fx) * (1 - fy) +
                      (ch(ax, by) * (1 - fx) + ch(bx, by) * fx) * fy;
        int got = whole[(static_cast<size_t>(y + pp.y) * pw + x + pp.x) * 4 + sh / 8];
        CHECK(std::abs(got - want) <= 2.5);
      }
    }
  }
  // Identity: exact copy.
  Scaler id;
  CHECK(id.Init(4, 3, FitWindow(4, 3, 4, 3)));
  uint32_t px[12];
  for (int i = 0; i < 12; ++i) px[i] = 0x010203u * (i + 1);
  uint32_t out[12] = {0};
  id.Render(px, 4, MakeRect(0, 0, 4, 3), reinterpret_cast<uint8_t*>(out), 16, f);
  for (int i = 0; i < 12; ++i) CHECK((out[i] & 0xFFFFFF) == (px[i] & 0xFFFFFF));
  double x, y;
  s.DstToSrc(2560 - 1, 0, &x, &y);
  CHECK(x > 1918 && x <= 1919 && y == 0);
  Ok("scaler");
}

void TestBlend() {
  uint32_t ui[4] = {0xFF112233u, 0x00000000u, 0x80402010u, 0x80000000u};
  uint32_t out[4] = {0x00AABBCC, 0x00AABBCC, 0x00FFFFFF, 0x00FF0000};
  BlendUiOver(ui, 4, out, 4, MakeRect(0, 0, 4, 1));
  CHECK(out[0] == 0x112233);
  CHECK(out[1] == 0xAABBCC);
  // 0x40 + 0xFF * 127/255 = 64 + 127 = 191
  CHECK(std::abs(static_cast<int>((out[2] >> 16) & 0xFF) - 191) <= 1);
  CHECK(std::abs(static_cast<int>(out[2] & 0xFF) - (0x10 + 127)) <= 1);
  CHECK(std::abs(static_cast<int>((out[3] >> 16) & 0xFF) - 127) <= 1);
  // Exhaustive: never above 255, never off by more than 1.
  for (int a = 1; a < 255; ++a)
    for (int c = 0; c <= a; c += 7)
      for (int d = 0; d < 256; d += 15) {
        uint32_t u = (static_cast<uint32_t>(a) << 24) | (c << 16) | (c << 8) | c;
        uint32_t o = (d << 16) | (d << 8) | d;
        BlendUiOver(&u, 1, &o, 1, MakeRect(0, 0, 1, 1));
        double want = c + d * (255.0 - a) / 255.0;
        for (int sh = 0; sh < 24; sh += 8) {
          int got = (o >> sh) & 0xFF;
          CHECK(std::abs(got - want) <= 1.0);
        }
        CHECK((o >> 24) == 0);
      }
  // Stretch: 2x2 picture into 4x4.
  uint32_t pic[4] = {1, 2, 3, 4};
  uint32_t img[16] = {0};
  DrawStretched(reinterpret_cast<uint8_t*>(pic), 2, 2, 8, MakeRect(0, 0, 4, 4),
                MakeRect(0, 0, 4, 4), img, 4);
  CHECK(img[0] == 1 && img[3] == 2 && img[12] == 3 && img[15] == 4);
  DrawStretched(nullptr, 2, 2, 8, MakeRect(0, 0, 4, 4), MakeRect(0, 0, 4, 4),
                img, 4);
  DrawStretched(reinterpret_cast<uint8_t*>(pic), 2, 2, 4, MakeRect(0, 0, 4, 4),
                MakeRect(0, 0, 4, 4), img, 4);  // pitch too small: ignored
  Ok("blend");
}

std::vector<uint8_t> Ev(int type, int code, int value) {
  std::vector<uint8_t> b(kInputEventSize, 0);
  uint16_t t = static_cast<uint16_t>(type), c = static_cast<uint16_t>(code);
  int32_t v = value;
  memcpy(&b[16], &t, 2);
  memcpy(&b[18], &c, 2);
  memcpy(&b[20], &v, 4);
  return b;
}

std::vector<uint8_t> Cat(std::initializer_list<std::vector<uint8_t>> parts) {
  std::vector<uint8_t> all;
  for (const auto& p : parts) all.insert(all.end(), p.begin(), p.end());
  return all;
}

// A device that disappears lets go of everything it held.
void TestRelease() {
  EvdevDecoder d(Keymap::kUs);
  DeviceState k, k2, ms, pd;
  k.cls = k2.cls = DevClass::kKeyboard;
  ms.cls = DevClass::kMouse;
  pd.cls = DevClass::kGamepad;
  std::vector<InEvent> o;
  auto b = Cat({Ev(EV_KEY, KEY_LEFTCTRL, 1), Ev(EV_KEY, KEY_A, 1), Ev(EV_KEY, KEY_A, 2)});
  d.Feed(&k, b.data(), 10, &o);  // and a half record left over
  d.Feed(&k, b.data() + 10, b.size() - 10, &o);
  b = Cat({Ev(EV_KEY, BTN_LEFT, 1), Ev(EV_SYN, 0, 0)});
  d.Feed(&ms, b.data(), b.size(), &o);
  b = Cat({Ev(EV_ABS, ABS_HAT0X, -1), Ev(EV_ABS, ABS_Y, 32000), Ev(EV_KEY, BTN_SOUTH, 1)});
  d.Feed(&pd, b.data(), b.size(), &o);
  b = Cat({Ev(EV_KEY, KEY_B, 1)});
  d.Feed(&k, b.data(), 7, &o);  // half a record pending on the dying device
  o.clear();
  d.ReleaseAll(&k, &o);
  CHECK(o.size() == 2);
  CHECK(o[0].type == InEvent::kUnpress && o[0].key == 0x11);  // Ctrl
  CHECK(o[1].type == InEvent::kUnpress && o[1].key == 0x41);  // A
  CHECK(!(o[1].modifiers & kModCtrl) && k.held.empty() && k.partial_len == 0);
  o.clear();
  b = Cat({Ev(EV_KEY, KEY_B, 1)});
  d.Feed(&k2, b.data(), b.size(), &o);  // another keyboard: Ctrl is not stuck
  CHECK(o.size() == 1 && !(o[0].modifiers & kModCtrl));
  o.clear();
  d.ReleaseAll(&ms, &o);
  CHECK(o.size() == 1 && o[0].type == InEvent::kUnpress && o[0].key == 0x7000 &&
        !(o[0].modifiers & kModButtonLeft));
  o.clear();
  d.ReleaseAll(&pd, &o);
  CHECK(o.size() == 3);
  for (const InEvent& e : o) CHECK(e.type == InEvent::kUnpress && e.source == InEvent::kRemote);
  CHECK(o[0].key == 0x0D && o[1].key == 0x25 && o[2].key == 0x28);  // Enter, hat left, stick down
  o.clear();
  d.ReleaseAll(&pd, &o);
  CHECK(o.empty());
  // A release never seen pressed (a key held when Cobalt started) is
  // dropped, and so is a second press without a release.
  b = Cat({Ev(EV_KEY, KEY_ENTER, 0), Ev(EV_KEY, KEY_C, 1), Ev(EV_KEY, KEY_C, 1),
           Ev(EV_KEY, BTN_RIGHT, 0)});
  d.Feed(&k2, b.data(), b.size(), &o);
  d.Feed(&ms, b.data() + 72, 24, &o);
  CHECK(o.size() == 1 && o[0].key == 0x43 && o[0].type == InEvent::kPress);
}

// A tablet (QEMU usb-tablet) or touch screen points at screen positions.
void TestAbsPointer() {
  EvdevDecoder d(Keymap::kUs);
  d.SetGeometry(1920, 1080, FitWindow(1920, 1080, 2560, 1440), 2560, 1440);
  DeviceState tab;
  tab.cls = DevClass::kAbsPointer;
  tab.abs_x.min = tab.abs_y.min = 0;
  tab.abs_x.max = tab.abs_y.max = 32767;
  std::vector<InEvent> o;
  auto b = Cat({Ev(EV_ABS, ABS_X, 32767), Ev(EV_ABS, ABS_Y, 0), Ev(EV_SYN, 0, 0),
                Ev(EV_KEY, BTN_LEFT, 1), Ev(EV_SYN, 0, 0), Ev(EV_KEY, BTN_LEFT, 0),
                Ev(EV_ABS, ABS_X, 16384), Ev(EV_ABS, ABS_Y, 16384), Ev(EV_SYN, 0, 0),
                Ev(EV_KEY, KEY_A, 1), Ev(EV_ABS, ABS_X, 999999), Ev(EV_ABS, ABS_Y, -5),
                Ev(EV_REL, REL_WHEEL, -1), Ev(EV_SYN, 0, 0)});
  d.Feed(&tab, b.data(), b.size(), &o);
  CHECK(o.size() == 6);  // move, press, unpress, move, move, wheel
  CHECK(o[0].type == InEvent::kMove && o[0].x == 1919 && o[0].y == 0);
  CHECK(o[1].type == InEvent::kPress && o[1].key == 0x7000 && o[1].x == 1919);
  CHECK(o[2].type == InEvent::kUnpress);
  CHECK(o[3].type == InEvent::kMove && std::abs(o[3].x - 960) < 1.5 &&
        std::abs(o[3].y - 540) < 1.5);
  CHECK(o[4].x == 1919 && o[4].y == 0);  // clamped, no key from a tablet
  CHECK(o[5].type == InEvent::kWheel && o[5].dy == 1);
  CHECK(d.pointer_seen() && d.pointer_screen_x() == 2559);
  // A touch screen: touching is the left button.
  DeviceState touch = tab;
  o.clear();
  b = Cat({Ev(EV_ABS, ABS_X, 0), Ev(EV_ABS, ABS_Y, 32767), Ev(EV_KEY, BTN_TOUCH, 1),
           Ev(EV_SYN, 0, 0), Ev(EV_KEY, BTN_TOUCH, 0)});
  d.Feed(&touch, b.data(), b.size(), &o);
  CHECK(o.size() == 3 && o[0].type == InEvent::kMove && o[0].y == 1079);
  CHECK(o[1].key == 0x7000 && o[1].type == InEvent::kPress && o[2].type == InEvent::kUnpress);
  // A broken range is ignored, not divided by.
  DeviceState broken = tab;
  broken.abs_x.max = broken.abs_x.min;
  o.clear();
  b = Cat({Ev(EV_ABS, ABS_X, 5), Ev(EV_SYN, 0, 0)});
  d.Feed(&broken, b.data(), b.size(), &o);
  CHECK(o.empty());
}

void TestDecoder() {
  EvdevDecoder dec(Keymap::kDe);
  dec.SetGeometry(1920, 1080, FitWindow(1920, 1080, 2560, 1440));
  DeviceState kbd;
  kbd.cls = DevClass::kKeyboard;
  std::vector<InEvent> out;
  auto bytes = Cat({Ev(EV_KEY, KEY_UP, 1), Ev(EV_SYN, 0, 0), Ev(EV_KEY, KEY_UP, 2),
                    Ev(EV_KEY, KEY_UP, 0), Ev(EV_KEY, KEY_ENTER, 1),
                    Ev(EV_KEY, KEY_ESC, 1), Ev(EV_KEY, KEY_ESC, 0)});
  // One byte at a time gives the same as all at once.
  for (uint8_t b : bytes) dec.Feed(&kbd, &b, 1, &out);
  CHECK(out.size() == 6);
  CHECK(out[0].type == InEvent::kPress && out[0].key == 0x26 && !out[0].repeat);
  CHECK(out[1].type == InEvent::kPress && out[1].repeat);
  CHECK(out[2].type == InEvent::kUnpress);
  CHECK(out[3].key == 0x0D && out[4].key == 0x1B && out[5].type == InEvent::kUnpress);
  CHECK(out[0].source == InEvent::kKeyboard);
  // Shift + German Y/Z swap + characters.
  out.clear();
  bytes = Cat({Ev(EV_KEY, KEY_LEFTSHIFT, 1), Ev(EV_KEY, KEY_Y, 1),
               Ev(EV_KEY, KEY_LEFTSHIFT, 0), Ev(EV_KEY, KEY_Z, 1),
               Ev(EV_KEY, KEY_5, 1)});
  dec.Feed(&kbd, bytes.data(), bytes.size(), &out);
  CHECK(out.size() == 5);
  CHECK(out[1].key == 'Z' && out[1].character == 'Z' && (out[1].modifiers & kModShift));
  CHECK(out[3].key == 'Y' && out[3].character == 'y' && !(out[3].modifiers & kModShift));
  CHECK(out[4].key == '5' && out[4].character == '5');
  CHECK(LinuxKeyToSbKey(KEY_Y, Keymap::kUs) == 'Y');
  // Mouse: relative motion is in screen pixels (4/3), reported in window
  // pixels, one move per report; buttons carry the position.
  EvdevDecoder m(Keymap::kUs);
  m.SetGeometry(1920, 1080, FitWindow(1920, 1080, 2560, 1440));
  DeviceState mouse;
  mouse.cls = DevClass::kMouse;
  out.clear();
  bytes = Cat({Ev(EV_REL, REL_X, 400), Ev(EV_REL, REL_Y, -100), Ev(EV_SYN, 0, 0),
               Ev(EV_KEY, BTN_LEFT, 1), Ev(EV_SYN, 0, 0), Ev(EV_KEY, BTN_LEFT, 0),
               Ev(EV_REL, REL_WHEEL, 2), Ev(EV_SYN, 0, 0), Ev(EV_KEY, KEY_A, 1)});
  m.Feed(&mouse, bytes.data(), bytes.size(), &out);
  CHECK(m.pointer_seen() && m.pointer_screen_x() == 1280 + 400);
  CHECK(out.size() == 4);
  CHECK(out[0].type == InEvent::kMove && out[0].source == InEvent::kMouse);
  CHECK(std::abs(out[0].x - 1680 * 0.75) < 0.01 && std::abs(out[0].y - 620 * 0.75) < 0.01);
  CHECK(out[1].type == InEvent::kPress && out[1].key == 0x7000 && (out[1].modifiers & kModButtonLeft));
  CHECK(out[2].type == InEvent::kUnpress && !(out[2].modifiers & kModButtonLeft));
  CHECK(out[3].type == InEvent::kWheel && out[3].dy == -2);
  // Clamped at the screen edge.
  out.clear();
  bytes = Cat({Ev(EV_REL, REL_X, 2000000000), Ev(EV_REL, REL_Y, -2000000000), Ev(EV_SYN, 0, 0)});
  m.Feed(&mouse, bytes.data(), bytes.size(), &out);
  CHECK(m.pointer_screen_x() == 2559 && m.pointer_screen_y() == 0);
  CHECK(out.size() == 1 && out[0].x <= 1919 && out[0].y == 0);
  // SYN_DROPPED forgets the half report.
  out.clear();
  bytes = Cat({Ev(EV_REL, REL_WHEEL, 1), Ev(EV_SYN, SYN_DROPPED, 0), Ev(EV_SYN, 0, 0)});
  m.Feed(&mouse, bytes.data(), bytes.size(), &out);
  CHECK(out.empty());
  // Gamepad: buttons, hat and stick become remote keys.
  DeviceState pad;
  pad.cls = DevClass::kGamepad;
  out.clear();
  bytes = Cat({Ev(EV_KEY, BTN_SOUTH, 1), Ev(EV_KEY, BTN_SOUTH, 0), Ev(EV_KEY, BTN_EAST, 1),
               Ev(EV_ABS, ABS_HAT0X, 1), Ev(EV_ABS, ABS_HAT0X, 0),
               Ev(EV_ABS, ABS_Y, -32000), Ev(EV_ABS, ABS_Y, -20000), Ev(EV_ABS, ABS_Y, 0),
               Ev(EV_KEY, BTN_DPAD_DOWN, 1)});
  m.Feed(&pad, bytes.data(), bytes.size(), &out);
  CHECK(out.size() == 8);
  CHECK(out[0].key == 0x0D && out[0].source == InEvent::kRemote);
  CHECK(out[2].key == 0x1B);
  CHECK(out[3].key == 0x27 && out[3].type == InEvent::kPress);
  CHECK(out[4].key == 0x27 && out[4].type == InEvent::kUnpress);
  CHECK(out[5].key == 0x26 && out[5].type == InEvent::kPress);  // stick up
  CHECK(out[6].key == 0x26 && out[6].type == InEvent::kUnpress);  // back to 0
  CHECK(out[7].key == 0x28);
  // Class filters: a keyboard sends no pointer, a mouse no keys.
  out.clear();
  bytes = Cat({Ev(EV_REL, REL_X, 5), Ev(EV_SYN, 0, 0), Ev(EV_KEY, BTN_SOUTH, 1)});
  m.Feed(&kbd, bytes.data(), bytes.size(), &out);
  CHECK(out.empty());
  // Garbage never crashes and only yields well-formed events.
  std::mt19937 rng(1);
  DeviceState any;
  for (int round = 0; round < 200; ++round) {
    std::vector<uint8_t> junk(1 + rng() % 5000);
    for (auto& b : junk) b = static_cast<uint8_t>(rng());
    // Make some of it look like real records.
    for (size_t i = 16; i + 8 <= junk.size(); i += 24) {
      junk[i] = static_cast<uint8_t>(rng() % 5);
      junk[i + 1] = 0;
    }
    out.clear();
    m.Feed(&any, junk.data(), junk.size(), &out);
    for (const InEvent& e : out) {
      CHECK(e.x >= 0 && e.x <= 1919 && e.y >= 0 && e.y <= 1079);
      CHECK(std::abs(e.dx) <= 1000 && std::abs(e.dy) <= 1000);
    }
  }
  TestRelease();
  TestAbsPointer();
  Ok("decoder");
}

fb_var_screeninfo Var(unsigned w, unsigned h, unsigned bpp) {
  fb_var_screeninfo v;
  memset(&v, 0, sizeof(v));
  v.xres = v.xres_virtual = w;
  v.yres = v.yres_virtual = h;
  v.bits_per_pixel = bpp;
  if (bpp == 16) {
    v.red.offset = 11; v.red.length = 5;
    v.green.offset = 5; v.green.length = 6;
    v.blue.offset = 0; v.blue.length = 5;
  } else {
    v.red.offset = 16; v.red.length = 8;
    v.green.offset = 8; v.green.length = 8;
    v.blue.offset = 0; v.blue.length = 8;
  }
  return v;
}

fb_fix_screeninfo Fix(unsigned line, unsigned mem) {
  fb_fix_screeninfo f;
  memset(&f, 0, sizeof(f));
  f.type = FB_TYPE_PACKED_PIXELS;
  f.visual = FB_VISUAL_TRUECOLOR;
  f.line_length = line;
  f.smem_len = mem;
  return f;
}

// The mode checks of a real fbdev, with made-up kernel answers.
void TestGeometry() {
  FbGeometry g;
  std::string why;
  // 1440p with padded rows and two pages.
  fb_var_screeninfo v = Var(2560, 1440, 32);
  v.yres_virtual = 2880;
  fb_fix_screeninfo f = Fix(2560 * 4 + 256, (2560 * 4 + 256) * 2880);
  CHECK(ValidateGeometry(v, f, f.smem_len, &g, &why));
  CHECK(g.width == 2560 && g.height == 1440 && g.pitch == 10496 && g.offset == 0);
  v.yoffset = 1440;  // second page visible
  CHECK(ValidateGeometry(v, f, f.smem_len, &g, &why) && g.offset == 1440u * 10496);
  v.yoffset = 1441;  // page reaches past the virtual screen
  CHECK(!ValidateGeometry(v, f, f.smem_len, &g, &why) && !why.empty());
  v.yoffset = 0;
  v.xoffset = 8;
  CHECK(!ValidateGeometry(v, f, f.smem_len, &g, &why));  // xres_virtual too small
  v.xres_virtual = 2568;
  CHECK(ValidateGeometry(v, f, f.smem_len, &g, &why) && g.offset == 32);
  // Rows shorter than the width; memory one byte short; exactly enough.
  v = Var(2560, 1440, 32);
  f = Fix(2560 * 4 - 4, 2560 * 4 * 1440);
  CHECK(!ValidateGeometry(v, f, f.smem_len, &g, &why));
  f = Fix(10496, 10496 * 1439 + 10240);
  CHECK(ValidateGeometry(v, f, f.smem_len, &g, &why));  // last row needs no padding
  CHECK(!ValidateGeometry(v, f, f.smem_len - 1, &g, &why));
  // 16 bpp (565) works, 24 bpp and odd visuals do not.
  v = Var(1280, 720, 16);
  f = Fix(1280 * 2, 1280 * 2 * 720);
  CHECK(ValidateGeometry(v, f, f.smem_len, &g, &why) && g.format.BytesPerPixel() == 2);
  v = Var(1280, 720, 24);
  f = Fix(1280 * 3, 1280 * 3 * 720);
  CHECK(!ValidateGeometry(v, f, f.smem_len, &g, &why));
  v = Var(1280, 720, 32);
  f = Fix(1280 * 4, 1280 * 4 * 720);
  f.visual = FB_VISUAL_PSEUDOCOLOR;
  CHECK(!ValidateGeometry(v, f, f.smem_len, &g, &why));
  f.visual = FB_VISUAL_TRUECOLOR;
  f.type = FB_TYPE_PLANES;
  CHECK(!ValidateGeometry(v, f, f.smem_len, &g, &why));
  // Hostile numbers: no overflow, no pass.
  fb_var_screeninfo z;
  memset(&z, 0, sizeof(z));
  CHECK(!ValidateGeometry(z, Fix(0, 0), 0, &g, &why));
  v = Var(0xFFFFFFFFu, 720, 32);
  CHECK(!ValidateGeometry(v, Fix(0xFFFFFFFFu, 0xFFFFFFFFu), 0xFFFFFFFFu, &g, &why));
  v = Var(1280, 720, 32);
  v.yoffset = v.yres_virtual = 0xFFFFFFFFu;
  CHECK(!ValidateGeometry(v, Fix(5120, 0xFFFFFFFFu), 0xFFFFFFFFu, &g, &why));
  v = Var(1280, 720, 32);
  CHECK(!ValidateGeometry(v, Fix(0xFFFFFFFFu, 0xFFFFFFFFu), 0xFFFFFFFFu, &g, &why));
  v.bits_per_pixel = 0xFFFFFFFFu;
  CHECK(!ValidateGeometry(v, Fix(5120, 5120 * 720), 5120 * 720, &g, &why));
  // Random modes: whatever passes stays inside the memory.
  std::mt19937 rng(7);
  const unsigned picks[] = {0, 1, 2, 7, 640, 720, 1080, 1440, 2560, 4096, 16384, 16385,
                            0x7FFFFFFFu, 0xFFFFFFFFu};
  auto pick = [&] { return picks[rng() % (sizeof(picks) / sizeof(picks[0]))]; };
  int passed = 0;
  for (int i = 0; i < 20000; ++i) {
    v = Var(pick(), pick(), rng() % 2 ? 32 : 16);
    v.xres_virtual = rng() % 2 ? v.xres : pick();
    v.yres_virtual = rng() % 2 ? v.yres : pick();
    v.xoffset = rng() % 3 ? 0 : pick();
    v.yoffset = rng() % 3 ? 0 : pick();
    const unsigned bpp = v.bits_per_pixel / 8;
    f = Fix(rng() % 2 ? v.xres * bpp : pick(), pick());
    const size_t map = rng() % 2 ? f.smem_len : pick();
    if (ValidateGeometry(v, f, map, &g, &why)) {
      ++passed;
      CHECK(g.width >= 1 && g.height >= 1 && g.pitch >= g.width * static_cast<int>(bpp));
      CHECK(g.offset + static_cast<uint64_t>(g.height - 1) * g.pitch +
                static_cast<uint64_t>(g.width) * bpp <= map);
    }
  }
  CHECK(passed > 0);
  Ok("geometry");
}

void TestFbDevice() {
  Config c;
  c.fb_fake = true;
  c.fake_w = 64;
  c.fake_h = 32;
  c.fb_path = g_tmp + "/fb-fake";
  FbDevice fb;
  std::string err;
  CHECK(fb.Open(c, &err));
  CHECK(fb.fake() && fb.width() == 64 && fb.pitch() == 256);
  struct stat st;
  CHECK(stat(c.fb_path.c_str(), &st) == 0 && st.st_size == 64 * 32 * 4);
  fb.Close();
  c.fb_path = "/dev/fb-should-not-be-touched";
  CHECK(!fb.Open(c, &err) && err.find("/dev") != std::string::npos);
  c.fb_path = g_tmp;  // a folder
  CHECK(!fb.Open(c, &err));
  // Real mode refuses a plain file, a missing node, and a character device
  // that is not a framebuffer (looked at with stat, never opened).
  Config r;
  r.fb_path = g_tmp + "/fb-fake";
  CHECK(!fb.Open(r, &err) && err.find("BITSOS_FB_FAKE") != std::string::npos);
  r.fb_path = g_tmp + "/missing";
  CHECK(!fb.Open(r, &err));
  r.fb_path = "/dev/null";
  CHECK(!fb.Open(r, &err) && err.find("not a framebuffer") != std::string::npos);
  CHECK(!fb.is_open());
  // A fake is never put into /dev, /proc or /sys, however the path is spelt.
  c.fb_path = "/tmp/../dev/fb-should-not-be-touched";
  CHECK(!fb.Open(c, &err) && err.find("/dev") != std::string::npos);
  CHECK(access("/dev/fb-should-not-be-touched", F_OK) != 0);
  const std::string devlink = g_tmp + "/devlink";
  CHECK(symlink("/dev", devlink.c_str()) == 0);
  c.fb_path = devlink + "/null";
  CHECK(!fb.Open(c, &err) && err.find("/dev") != std::string::npos);
  c.fb_path = "/proc/self/fake-fb";
  CHECK(!fb.Open(c, &err));
  CHECK(IsSystemPath("/dev") && IsSystemPath("/sys/x") && !IsSystemPath("/device/x"));
  // A link as the last part, and a FIFO, are refused (no hang on the FIFO).
  const std::string target = g_tmp + "/fb-target";
  close(open(target.c_str(), O_WRONLY | O_CREAT, 0644));
  const std::string link = g_tmp + "/fb-link";
  CHECK(symlink(target.c_str(), link.c_str()) == 0);
  c.fb_path = link;
  CHECK(!fb.Open(c, &err));
  struct stat tst;
  CHECK(stat(target.c_str(), &tst) == 0 && tst.st_size == 0);  // not resized
  const std::string fifo = g_tmp + "/fb-fifo";
  CHECK(mkfifo(fifo.c_str(), 0600) == 0);
  c.fb_path = fifo;
  CHECK(!fb.Open(c, &err) && err.find("plain file") != std::string::npos);
  // Padded rows and a second page; a pan moves pixels(), a mode change or a
  // page past the memory is "lost".
  c.fb_path = g_tmp + "/fb-pad";
  c.fake_pitch = 64 * 4 + 40;
  c.fake_virtual_h = 64;
  CHECK(fb.Open(c, &err));
  CHECK(fb.pitch() == 296 && stat(c.fb_path.c_str(), &st) == 0 && st.st_size == 296 * 64);
  uint8_t* first = fb.pixels();
  CHECK(fb.Recheck() == FbDevice::Change::kSame);  // a fake never pans itself
  FbGeometry g;
  g.width = 64;
  g.height = 32;
  g.pitch = 296;
  g.offset = 32 * 296;
  CHECK(fb.ApplyGeometry(g) == FbDevice::Change::kMoved && fb.pixels() == first + 32 * 296);
  CHECK(fb.Recheck() == FbDevice::Change::kMoved && fb.Recheck() == FbDevice::Change::kSame);
  CHECK(fb.ApplyGeometry(g) == FbDevice::Change::kSame);
  g.offset = 33 * 296;  // the last row would end past the memory
  CHECK(fb.ApplyGeometry(g) == FbDevice::Change::kLost && fb.pixels() == first + 32 * 296);
  g.offset = 0;
  CHECK(fb.ApplyGeometry(g) == FbDevice::Change::kMoved);
  CHECK(fb.Recheck() == FbDevice::Change::kLost);  // lost is not overwritten
  g.width = 65;
  CHECK(fb.ApplyGeometry(g) == FbDevice::Change::kLost);
  fb.Close();
  c.fake_pitch = 64 * 4 - 1;  // shorter than a row
  CHECK(!fb.Open(c, &err));
  Ok("fbdevice");
}

class SolidVideo : public VideoImage {
 public:
  SolidVideo(int w, int h, uint32_t color) : w_(w), h_(h), px_(w * h, color) {}
  bool Get(const uint8_t** d, int* w, int* h, int* p) override {
    *d = reinterpret_cast<const uint8_t*>(px_.data());
    *w = w_;
    *h = h_;
    *p = w_ * 4;
    return true;
  }

 private:
  int w_, h_;
  std::vector<uint32_t> px_;
};

class YuvVideo : public VideoImage {
 public:
  YuvVideo(int w, int h, uint8_t y, uint8_t u, uint8_t v)
      : w_(w), h_(h), y_(w * h, y), u_(((w + 1) / 2) * ((h + 1) / 2), u),
        v_(u_.size(), v) {}
  bool GetYuv(YuvPlanes* p) override {
    p->y = y_.data();
    p->u = u_.data();
    p->v = v_.data();
    p->y_pitch = w_;
    p->u_pitch = p->v_pitch = (w_ + 1) / 2;
    p->width = w_;
    p->height = h_;
    p->y_rows = h_;
    p->u_rows = p->v_rows = (h_ + 1) / 2;
    return true;
  }
  bool Get(const uint8_t**, int*, int*, int*) override { return false; }

 private:
  int w_, h_;
  std::vector<uint8_t> y_, u_, v_;
};

void TestYuv() {
  // The formula against floating point.
  for (int y = 0; y < 256; y += 5)
    for (int u = 0; u < 256; u += 17)
      for (int v = 0; v < 256; v += 13) {
        const uint32_t p = YuvToBgrx(y, u, v);
        auto clamp = [](double x) { return x < 0 ? 0.0 : (x > 255 ? 255.0 : x); };
        const double r = clamp(1.164 * (y - 16) + 1.793 * (v - 128));
        const double g = clamp(1.164 * (y - 16) - 0.213 * (u - 128) - 0.533 * (v - 128));
        const double b = clamp(1.164 * (y - 16) + 2.112 * (u - 128));
        CHECK(std::abs(static_cast<int>((p >> 16) & 0xFF) - r) <= 2);
        CHECK(std::abs(static_cast<int>((p >> 8) & 0xFF) - g) <= 2);
        CHECK(std::abs(static_cast<int>(p & 0xFF) - b) <= 2);
      }
  CHECK(YuvToBgrx(16, 128, 128) == 0 && YuvToBgrx(235, 128, 128) == 0xFEFEFE);
  // SSE2 rows == the scalar formula, for odd sizes and odd rectangles.
  std::mt19937 rng(3);
  for (int round = 0; round < 50; ++round) {
    const int w = 1 + rng() % 70, h = 1 + rng() % 30;
    std::vector<uint8_t> Y(w * h + 8), U(((w + 1) / 2) * ((h + 1) / 2) + 8), V(U.size());
    for (auto& b : Y) b = rng();
    for (auto& b : U) b = rng();
    for (auto& b : V) b = rng();
    YuvPlanes p;
    p.y = Y.data(); p.u = U.data(); p.v = V.data();
    p.y_pitch = w; p.u_pitch = p.v_pitch = (w + 1) / 2;
    p.width = w; p.height = h;
    p.y_rows = h; p.u_rows = p.v_rows = (h + 1) / 2;
    CHECK(p.Valid());
    Rect r = MakeRect(rng() % w, rng() % h, 1 + rng() % w, 1 + rng() % h);
    std::vector<uint32_t> out(static_cast<size_t>(w + 4) * (h + 4), 0xDEADBEEF);
    ConvertYuvRect(p, r, out.data(), w + 4, 2, 3);
    Rect c = Intersect(r, MakeRect(0, 0, w, h));
    for (int yy = 0; yy < h + 4; ++yy)
      for (int xx = 0; xx < w + 4; ++xx) {
        const int sx = xx - 2 + c.x, sy = yy - 3 + c.y;
        const bool inside = sx >= c.x && sx < c.x + c.w && sy >= c.y && sy < c.y + c.h;
        const uint32_t got = out[yy * (w + 4) + xx];
        if (inside) {
          CHECK(got == YuvToBgrx(Y[sy * w + sx], U[(sy / 2) * p.u_pitch + sx / 2],
                                 V[(sy / 2) * p.v_pitch + sx / 2]));
        } else {
          CHECK(got == 0xDEADBEEF);
        }
      }
  }
  YuvPlanes bad;
  ConvertYuvRect(bad, MakeRect(0, 0, 10, 10), nullptr, 0, 0, 0);  // ignored
  // Planes shorter than the frame are refused (no read past their end):
  // a 9x7 frame needs 7 luma rows and 4 chroma rows of at least 5 bytes.
  std::vector<uint8_t> y9(9 * 7), c9(5 * 4);
  YuvPlanes s;
  s.y = y9.data(); s.u = c9.data(); s.v = c9.data();
  s.y_pitch = 9; s.u_pitch = s.v_pitch = 5;
  s.width = 9; s.height = 7;
  s.y_rows = 7; s.u_rows = s.v_rows = 4;
  CHECK(s.Valid());
  YuvPlanes t = s; t.u_rows = 3;  // chroma one row short (7/2 rounded down)
  CHECK(!t.Valid());
  t = s; t.y_rows = 6;
  CHECK(!t.Valid());
  t = s; t.v_pitch = 4;  // 9/2 rounded down
  CHECK(!t.Valid());
  t = s; t.height = -1;
  CHECK(!t.Valid());
  std::vector<uint32_t> sink(9 * 7, 0xDEADBEEF);
  t = s; t.u_rows = 3;
  ConvertYuvRect(t, MakeRect(0, 0, 9, 7), sink.data(), 9, 0, 0);
  CHECK(sink[62] == 0xDEADBEEF);  // nothing written for an invalid frame
  Ok("yuv");
}

bool WaitFor(const std::function<bool()>& done) {
  for (int i = 0; i < 300 && !done(); ++i) {
    usleep(10000);
  }
  return done();
}

// A padded screen (pitch > width*4) whose second page becomes visible: rows
// land at the pitch, the padding is never written, the new page is drawn
// again in full, and after a mode change nothing is drawn any more.
void TestCompositorPan() {
  Config c;
  c.fb_fake = true;
  c.fake_w = 640;
  c.fake_h = 360;
  c.fake_pitch = 640 * 4 + 64;
  c.fake_virtual_h = 720;
  c.fb_path = g_tmp + "/fb-pan";
  FbDevice fb;
  std::string err;
  CHECK(fb.Open(c, &err));
  memset(fb.pixels(), 0x77, static_cast<size_t>(fb.pitch()) * 720);
  FbCompositor comp(&fb, 640, 360, CursorMode::kOn);
  comp.Start();
  std::vector<uint32_t> green(640 * 360, 0xFF00FF00u);
  comp.Present(green.data(), 640 * 4, 640, 360, MakeRect(0, 0, 640, 360));
  comp.SetPointer(600, 300, true);
  comp.WaitIdle();
  CHECK(At(fb, 0, 0) == 0x00FF00 && At(fb, 639, 359) == 0x00FF00);
  const uint8_t* page1 = fb.pixels();
  for (int y = 0; y < 360; ++y)
    for (int i = 640 * 4; i < fb.pitch(); ++i) CHECK(page1[y * fb.pitch() + i] == 0x77);
  FbGeometry g;
  g.width = 640;
  g.height = 360;
  g.pitch = fb.pitch();
  g.offset = static_cast<size_t>(360) * fb.pitch();
  CHECK(fb.ApplyGeometry(g) == FbDevice::Change::kMoved);
  CHECK(WaitFor([&] { return comp.stats().pans == 1; }));
  comp.WaitIdle();
  CHECK(At(fb, 0, 0) == 0x00FF00 && At(fb, 639, 359) == 0x00FF00);  // new page
  CHECK(At(fb, 600, 300) == 0x000000);                               // pointer too
  const uint8_t* page2 = fb.pixels();
  CHECK(page2 == page1 + 360 * fb.pitch() && page2[640 * 4] == 0x77);
  g.width = 641;
  fb.ApplyGeometry(g);
  CHECK(WaitFor([&] { return comp.stats().lost; }));
  std::vector<uint32_t> blue(640 * 360, 0xFF0000FFu);
  comp.Present(blue.data(), 640 * 4, 640, 360, MakeRect(0, 0, 640, 360));
  comp.WaitIdle();
  CHECK(At(fb, 0, 0) == 0x00FF00);  // not drawn any more
  comp.Stop();
}

// YT-5: a frame with a timestamp is noted once when it reaches the screen;
// a UI pass over the same frame is not a new frame.
class StampedVideo : public SolidVideo {
 public:
  StampedVideo(int64_t pts) : SolidVideo(64, 36, 0xFF00FF00u), pts_(pts) {}
  int64_t timestamp() const override { return pts_; }

 private:
  const int64_t pts_;
};

void TestCompositorAv() {
  Config c;
  c.fb_fake = true;
  c.fake_w = 1920;
  c.fake_h = 1080;
  c.fb_path = g_tmp + "/fb-av";
  FbDevice fb;
  std::string err;
  CHECK(fb.Open(c, &err));
  FbCompositor comp(&fb, 1920, 1080, CursorMode::kOff);
  comp.Start();
  const std::string before = AvStats::Get()->Text(0);
  CHECK(before.find("frames_shown=0 ") != std::string::npos);
  comp.SetVideoBounds(1, 0, MakeRect(0, 0, 1920, 1080));
  comp.SetVideoFrame(1, std::make_shared<StampedVideo>(1000));
  comp.WaitIdle();
  std::vector<uint32_t> ui(1920 * 1080, 0);
  comp.Present(ui.data(), 1920 * 4, 1920, 1080, MakeRect(0, 0, 100, 100));
  comp.WaitIdle();
  CHECK(AvStats::Get()->Text(0).find("frames_shown=1 ") != std::string::npos);
  comp.SetVideoFrame(1, std::make_shared<StampedVideo>(2000));
  comp.WaitIdle();
  const std::string t = AvStats::Get()->Text(0);
  CHECK(t.find("frames_shown=2 ") != std::string::npos);
  CHECK(t.find("last_pts_us=2000 ") != std::string::npos);
  comp.Stop();
  Ok("compositor.av_notes");
}

void TestCompositor() {
  Config c;
  c.fb_fake = true;
  c.fake_w = 2560;
  c.fake_h = 1440;
  c.fb_path = g_tmp + "/fb-comp";
  FbDevice fb;
  std::string err;
  CHECK(fb.Open(c, &err));
  memset(fb.pixels(), 0x55, static_cast<size_t>(fb.pitch()) * fb.height());
  FbCompositor comp(&fb, 1920, 1080, CursorMode::kAuto);
  comp.Start();
  comp.WaitIdle();
  CHECK(At(fb, 0, 0) == 0 && At(fb, 2559, 1439) == 0);  // black at start
  // A red UI everywhere, with a transparent hole for the video.
  std::vector<uint32_t> ui(1920 * 1080, 0xFFFF0000u);
  for (int y = 300; y < 600; ++y)
    for (int x = 600; x < 1200; ++x) ui[y * 1920 + x] = 0;
  comp.Present(ui.data(), 1920 * 4, 1920, 1080, MakeRect(0, 0, 1920, 1080));
  comp.WaitIdle();
  CHECK(At(fb, 10, 10) == 0xFF0000 && At(fb, 2550, 1430) == 0xFF0000);
  CHECK(At(fb, 1200, 600) == 0);  // hole, no video yet: black
  comp.SetVideoBounds(1, 0, MakeRect(0, 0, 1920, 1080));
  comp.SetVideoFrame(1, std::make_shared<SolidVideo>(1920, 1080, 0xFF00FF00u));
  comp.WaitIdle();
  CHECK(At(fb, 1200, 600) == 0x00FF00);  // hole shows the video
  CHECK(At(fb, 10, 10) == 0xFF0000);     // UI stays on top
  // Damage only: a blue square at window (0,0)-(30,30) -> screen (0,0)-(40,40)
  for (int y = 0; y < 30; ++y)
    for (int x = 0; x < 30; ++x) ui[y * 1920 + x] = 0xFF0000FFu;
  comp.Present(ui.data(), 1920 * 4, 1920, 1080, MakeRect(0, 0, 30, 30));
  comp.WaitIdle();
  CHECK(At(fb, 20, 20) == 0x0000FF && At(fb, 60, 60) == 0xFF0000);
  // Pointer appears after the first mouse use and moves without trails.
  // (1440p: the sprite is drawn twice as large; row 3 "X..X" -> white at 2,6)
  comp.SetPointer(1000, 1000, true);
  comp.WaitIdle();
  CHECK(At(fb, 1000, 1000) == 0x000000 && At(fb, 1002, 1006) == 0xFFFFFF);
  comp.SetPointer(1500, 1000, true);
  comp.WaitIdle();
  CHECK(At(fb, 1002, 1006) == 0xFF0000 && At(fb, 1502, 1006) == 0xFFFFFF);
  // A YV12 frame (white) instead, 1:1, then a scaled preview in a corner.
  comp.SetVideoFrame(1, std::make_shared<YuvVideo>(1920, 1080, 235, 128, 128));
  comp.WaitIdle();
  CHECK(At(fb, 1200, 600) == 0xFEFEFE);
  comp.SetVideoBounds(3, 1, MakeRect(600, 300, 300, 150));
  comp.SetVideoFrame(3, std::make_shared<YuvVideo>(640, 360, 16, 128, 128));
  comp.WaitIdle();
  CHECK(At(fb, 900, 450) == 0 && At(fb, 1500, 700) == 0xFEFEFE);
  comp.RemoveVideo(3);
  // The video goes away: the hole is black again.
  comp.RemoveVideo(1);
  comp.WaitIdle();
  CHECK(At(fb, 1200, 600) == 0);
  // Hostile calls are ignored.
  comp.Present(nullptr, 0, 10, 10, MakeRect(0, 0, 10, 10));
  comp.Present(ui.data(), 4, 1920, 1080, MakeRect(0, 0, 10, 10));
  comp.Present(ui.data(), 1920 * 4, 1920, 1080, MakeRect(-100, -100, 1, 1));
  comp.SetVideoBounds(2, 0, MakeRect(INT_MAX - 1, 5, INT_MAX, 5));
  comp.SetVideoFrame(2, std::make_shared<SolidVideo>(1, 1, 0));
  comp.WaitIdle();
  FbCompositor::Stats s = comp.stats();
  CHECK(s.presents >= 3 && s.passes >= 4);
  comp.Stop();
  // A letterboxed screen keeps black bars.
  Config c2 = c;
  c2.fake_w = 1280;
  c2.fake_h = 1024;
  c2.fb_path = g_tmp + "/fb-box";
  FbDevice fb2;
  CHECK(fb2.Open(c2, &err));
  FbCompositor comp2(&fb2, 1920, 1080, CursorMode::kOff);
  comp2.Start();
  comp2.Present(ui.data(), 1920 * 4, 1920, 1080, MakeRect(0, 0, 1920, 1080));
  comp2.WaitIdle();
  CHECK(At(fb2, 640, 10) == 0 && At(fb2, 640, 1020) == 0);
  CHECK(At(fb2, 640, 800) == 0xFF0000 && At(fb2, 640, 500) == 0);  // hole
  comp2.Stop();
  TestCompositorPan();
  Ok("compositor");
}

int g_cannot_open_lines = 0;

void Log(const std::string& line) {
  printf("   log: %s\n", line.c_str());
  if (line.find("cannot open") != std::string::npos) {
    ++g_cannot_open_lines;
  }
}

void WriteEvents(const std::string& path, const std::vector<uint8_t>& b) {
  FILE* f = fopen(path.c_str(), "wb");
  CHECK(f);
  fwrite(b.data(), 1, b.size(), f);
  fclose(f);
}

// Reads all devices until |devs| has |left| devices or 30 rounds passed.
void Drain(EvdevDevices* devs, EvdevDecoder* dec, std::vector<InEvent>* out,
           size_t left) {
  for (int i = 0; i < 30 && devs->size() != left; ++i) {
    std::vector<pollfd> fds;
    devs->AddPollFds(&fds);
    poll(fds.data(), fds.size(), 20);
    devs->ReadReady(fds, dec, out);
  }
}

// Hotplug: retry after a failed open, a new look after a re-plug, and keys
// let go when a device ends.
void TestDevicesHotplug() {
  EvdevDecoder dec;
  std::vector<InEvent> out;
  if (getuid() != 0) {  // root opens anything; the rights test needs a user
    const std::string locked = g_tmp + "/locked.ev";
    WriteEvents(locked, Cat({Ev(EV_KEY, KEY_UP, 1), Ev(EV_KEY, KEY_UP, 0)}));
    CHECK(chmod(locked.c_str(), 0) == 0);
    Config c;
    c.evdev_patterns = {locked};
    EvdevDevices devs(c, Log);
    g_cannot_open_lines = 0;
    devs.Rescan();
    devs.Rescan();
    devs.Rescan();
    CHECK(devs.size() == 0 && g_cannot_open_lines == 1);  // logged once
    CHECK(chmod(locked.c_str(), 0600) == 0);  // udev has set the rights now
    devs.Rescan();
    CHECK(devs.size() == 1);
    Drain(&devs, &dec, &out, 0);
    CHECK(out.size() == 2 && out[0].key == 0x26);
  }
  const std::string held = g_tmp + "/held.ev";
  WriteEvents(held, Cat({Ev(EV_KEY, KEY_LEFTCTRL, 1), Ev(EV_KEY, KEY_A, 1)}));
  Config c;
  c.evdev_patterns = {held};
  EvdevDevices devs(c, Log);
  devs.Rescan();
  CHECK(devs.size() == 1);
  out.clear();
  Drain(&devs, &dec, &out, 0);
  // Ctrl and A were pressed and never let go: the end of the device lets go.
  CHECK(out.size() == 4 && out[2].type == InEvent::kUnpress && out[3].type == InEvent::kUnpress);
  CHECK(!(out[3].modifiers & kModCtrl));
  devs.Rescan();
  CHECK(devs.size() == 0);  // read to its end: not again while it is there
  CHECK(unlink(held.c_str()) == 0);
  devs.Rescan();  // gone: forgotten
  WriteEvents(held, Cat({Ev(EV_KEY, KEY_B, 1), Ev(EV_KEY, KEY_B, 0)}));
  devs.Rescan();  // back (re-plugged): read again, whatever its inode
  CHECK(devs.size() == 1);
  out.clear();
  Drain(&devs, &dec, &out, 0);
  CHECK(out.size() == 2 && out[0].key == 0x42);
}

// Runs |body| in a child process; returns what it wrote to the pipe it got
// and its wait status.
std::string InChild(void (*body)(int fd), int* status) {
  int p[2];
  CHECK(pipe(p) == 0);
  fflush(stdout);
  fflush(stderr);
  const pid_t pid = fork();
  CHECK(pid >= 0);
  if (pid == 0) {
    close(p[0]);
    body(p[1]);
    _exit(0);
  }
  close(p[1]);
  std::string got;
  char buf[16];
  ssize_t n;
  while ((n = read(p[0], buf, sizeof(buf))) > 0) {
    got.append(buf, static_cast<size_t>(n));
  }
  close(p[0]);
  CHECK(waitpid(pid, status, 0) == pid);
  return got;
}

int g_guard_pipe = -1;

void WriteMark(int fd, char c) {
  (void)!write(fd, &c, 1);
}

// The console guard, with a pipe in place of the console: 'R' = restored.
void TestConsoleGuard() {
  int st = 0;
  // Killed by SIGTERM that nobody handles: restored, then the default end.
  std::string got = InChild(
      [](int fd) {
        ArmConsoleGuard(fd, [](int f) { WriteMark(f, 'R'); });
        raise(SIGTERM);
      },
      &st);
  CHECK(got == "R" && WIFSIGNALED(st) && WTERMSIG(st) == SIGTERM);
  // exit() without teardown: the atexit hook restores.
  got = InChild(
      [](int fd) {
        ArmConsoleGuard(fd, [](int f) { WriteMark(f, 'R'); });
        exit(3);
      },
      &st);
  CHECK(got == "R" && WIFEXITED(st) && WEXITSTATUS(st) == 3);
  // A crash (abort) where an older handler exists: restored first, then the
  // older handler gets the signal.
  got = InChild(
      [](int fd) {
        g_guard_pipe = fd;
        signal(SIGABRT, [](int) {
          WriteMark(g_guard_pipe, 'O');
          _exit(7);
        });
        ArmConsoleGuard(fd, [](int f) { WriteMark(f, 'R'); });
        abort();
      },
      &st);
  CHECK(got == "RO" && WIFEXITED(st) && WEXITSTATUS(st) == 7);
  // SIGTERM with a handler (Starboard's orderly stop): not restored early.
  got = InChild(
      [](int fd) {
        g_guard_pipe = fd;
        signal(SIGTERM, [](int) {
          WriteMark(g_guard_pipe, 'T');
          _exit(5);
        });
        ArmConsoleGuard(fd, [](int f) { WriteMark(f, 'R'); });
        raise(SIGTERM);
      },
      &st);
  CHECK(got == "T" && WIFEXITED(st) && WEXITSTATUS(st) == 5);
  // Normal end: the caller restored itself and disarmed; nothing more, and
  // the old handler is back.
  got = InChild(
      [](int fd) {
        struct sigaction before, now;
        sigaction(SIGSEGV, nullptr, &before);  // ASan has one in its builds
        ArmConsoleGuard(fd, [](int f) { WriteMark(f, 'R'); });
        DisarmConsoleGuard();
        sigaction(SIGSEGV, nullptr, &now);
        WriteMark(fd, now.sa_handler == before.sa_handler ? 'D' : 'X');
        exit(0);
      },
      &st);
  CHECK(got == "D" && WIFEXITED(st) && WEXITSTATUS(st) == 0);
  Ok("consoleguard");
}

void TestDevices() {
  const std::string fifo = g_tmp + "/kbd-fifo";
  CHECK(mkfifo(fifo.c_str(), 0600) == 0);
  const std::string file = g_tmp + "/replay.ev";
  {
    auto b = Cat({Ev(EV_KEY, KEY_DOWN, 1), Ev(EV_KEY, KEY_DOWN, 0)});
    FILE* f = fopen(file.c_str(), "wb");
    fwrite(b.data(), 1, b.size(), f);
    fclose(f);
  }
  Config c;
  c.evdev_patterns = {fifo, g_tmp + "/*.ev", g_tmp + "/nothing*", "/dev/null"};
  EvdevDevices devs(c, Log);
  devs.Rescan();
  CHECK(devs.size() == 2);  // the FIFO and the file; /dev/null is no evdev
  int w = open(fifo.c_str(), O_WRONLY | O_NONBLOCK);
  CHECK(w >= 0);
  auto b = Cat({Ev(EV_KEY, KEY_LEFT, 1), Ev(EV_KEY, KEY_LEFT, 0)});
  // Half a record first, the rest later.
  CHECK(write(w, b.data(), 10) == 10);
  EvdevDecoder dec;
  std::vector<InEvent> out;
  for (int i = 0; i < 3; ++i) {
    std::vector<pollfd> fds;
    devs.AddPollFds(&fds);
    CHECK(poll(fds.data(), fds.size(), 100) >= 0);
    devs.ReadReady(fds, &dec, &out);
    if (i == 0) {
      CHECK(write(w, b.data() + 10, b.size() - 10) == static_cast<ssize_t>(b.size() - 10));
    }
  }
  close(w);  // a writer leaving is no end for us
  std::vector<pollfd> fds;
  devs.AddPollFds(&fds);
  poll(fds.data(), fds.size(), 50);
  devs.ReadReady(fds, &dec, &out);
  CHECK(devs.size() == 1);  // the file was read to its end and closed
  int down = 0, left = 0;
  for (const InEvent& e : out) {
    down += e.key == 0x28;
    left += e.key == 0x25;
  }
  CHECK(down == 2 && left == 2);
  devs.Rescan();  // the read file is not opened again
  CHECK(devs.size() == 1);
  TestDevicesHotplug();
  Ok("devices");
}

// Times one full 1080p frame (video + UI) composed and scaled onto 1440p.
void Bench() {
  Config c;
  c.fb_fake = true;
  c.fake_w = 2560;
  c.fake_h = 1440;
  c.fb_path = g_tmp + "/fb-bench";
  FbDevice fb;
  std::string err;
  CHECK(fb.Open(c, &err));
  FbCompositor comp(&fb, 1920, 1080, CursorMode::kOff);
  comp.Start();
  std::vector<uint32_t> ui(1920 * 1080, 0);  // all hole: the playing case
  for (int y = 900; y < 1080; ++y)
    for (int x = 0; x < 1920; ++x) ui[y * 1920 + x] = 0x80202020u;  // controls
  comp.Present(ui.data(), 1920 * 4, 1920, 1080, MakeRect(0, 0, 1920, 1080));
  comp.SetVideoBounds(1, 0, MakeRect(0, 0, 1920, 1080));
  comp.WaitIdle();
  const int frames = 240;
  const auto t0 = std::chrono::steady_clock::now();
  for (int i = 0; i < frames; ++i) {
    comp.SetVideoFrame(1, std::make_shared<YuvVideo>(1920, 1080, 16 + i % 200, 128, 128));
    comp.WaitIdle();
  }
  const double ms = std::chrono::duration<double, std::milli>(std::chrono::steady_clock::now() - t0).count();
  FbCompositor::Stats s = comp.stats();
  printf("bench: %.2f ms per full frame (compose %.2f, scale %.2f) = %.2f cores at 60 fps\n",
         ms / frames, s.compose_us / 1e3 / s.passes, s.scale_us / 1e3 / s.passes, ms / frames * 60 / 1000);
}

}  // namespace

int main(int argc, char** argv) {
  char tmpl[] = "/tmp/bitsos-fb-test-XXXXXX";
  const char* base = getenv("YT_TEST_TMP");
  std::string t = base ? std::string(base) + "/fbtest-XXXXXX" : tmpl;
  std::vector<char> buf(t.begin(), t.end());
  buf.push_back(0);
  CHECK(mkdtemp(buf.data()));
  g_tmp = buf.data();
  if (argc > 1 && std::string(argv[1]) == "--bench") {
    Bench();
    return 0;
  }
  TestConsoleGuard();  // first: its children exit while little is allocated
  TestRects();
  TestConfig();
  TestFit();
  TestFormats();
  TestScaler();
  TestBlend();
  TestYuv();
  TestDecoder();
  TestGeometry();
  TestFbDevice();
  TestCompositor();
  TestCompositorAv();
  TestDevices();
  printf("ALL PASSED\n");
  return 0;
}
