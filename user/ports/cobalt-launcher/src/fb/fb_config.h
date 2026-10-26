// BitsOS V2 Cobalt backend: settings, read once from the environment.
//
//   BITSOS_BACKEND      "fb" selects this backend; anything else keeps X11.
//   BITSOS_FB           framebuffer device or fake file      (/dev/fb0)
//   BITSOS_FB_FAKE      WxH: treat BITSOS_FB as a plain file of that size,
//                       32-bit BGRX, no ioctls (agents' tests)   (unset)
//   BITSOS_WINDOW       WxH of the Cobalt window, at most 1920x1080
//                                                              (1920x1080)
//   BITSOS_EVDEV        input devices: paths or glob patterns separated by
//                       ':'; FIFOs and plain files are read as raw
//                       struct input_event records    (/dev/input/event*;
//                       with BITSOS_FB_FAKE: none, so a test run never
//                       reads or grabs the real keyboard and mouse)
//   BITSOS_EVDEV_GRAB   1: take real devices exclusively (EVIOCGRAB)
//                       (then the console's Ctrl+Alt+F1.. are dead while
//                       Cobalt runs; Ctrl+Alt+Backspace ends Cobalt)
//                                            (1; with BITSOS_FB_FAKE: 0)
//   BITSOS_KEYMAP       "de" (QWERTZ letters) or "us"                  (de)
//   BITSOS_CURSOR       "auto" (after the first mouse move), "on", "off"
//   BITSOS_KD_GRAPHICS  1: switch the controlling VT to graphics mode while
//                       running (a real console only; restored at exit) (0)

#ifndef STARBOARD_SHARED_BITSOS_FB_FB_CONFIG_H_
#define STARBOARD_SHARED_BITSOS_FB_FB_CONFIG_H_

#include <string>
#include <vector>

namespace bitsos_fb {

enum class CursorMode { kAuto, kOn, kOff };
enum class Keymap { kDe, kUs };

struct Config {
  bool fb_backend = false;
  std::string fb_path = "/dev/fb0";
  bool fb_fake = false;
  int fake_w = 0;
  int fake_h = 0;
  // Tests only (no environment setting): bytes per fake row (0 = width*4)
  // and fake virtual height (0 = height), to mimic padded or paged fbdevs.
  int fake_pitch = 0;
  int fake_virtual_h = 0;
  int window_w = 1920;
  int window_h = 1080;
  std::vector<std::string> evdev_patterns{"/dev/input/event*"};
  bool evdev_grab = true;
  Keymap keymap = Keymap::kDe;
  CursorMode cursor = CursorMode::kAuto;
  bool kd_graphics = false;
  // Settings that were present but not understood (kept default), for the
  // log: "BITSOS_WINDOW=abc".
  std::vector<std::string> ignored;
};

// |getenv_fn| is getenv in the program and a fake in the tests.
typedef const char* (*GetEnvFn)(const char* name);
Config ReadConfig(GetEnvFn getenv_fn);

// "1920x1080" -> 1920, 1080. Each side 1..16384; false otherwise.
bool ParseSize(const std::string& s, int* w, int* h);

// The largest window Cobalt is given (Y-NUMBERS: 1080p cap).
constexpr int kMaxWindowW = 1920;
constexpr int kMaxWindowH = 1080;
constexpr int kMaxSide = 16384;

}  // namespace bitsos_fb

#endif  // STARBOARD_SHARED_BITSOS_FB_FB_CONFIG_H_
