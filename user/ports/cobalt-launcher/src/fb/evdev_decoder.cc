// BitsOS V2 Cobalt backend: Linux evdev records -> input events for Cobalt.

#include "starboard/shared/bitsos/fb/evdev_decoder.h"

#include <linux/input-event-codes.h>
#include <string.h>

#include <algorithm>

namespace bitsos_fb {
namespace {

// SbKey values used here (starboard/key.h; the glue checks them).
enum : int {
  kBackspace = 0x08,
  kTab = 0x09,
  kReturn = 0x0D,
  kShift = 0x10,
  kControl = 0x11,
  kMenu = 0x12,
  kPause = 0x13,
  kCapital = 0x14,
  kEscape = 0x1B,
  kSpace = 0x20,
  kPrior = 0x21,
  kNext = 0x22,
  kEnd = 0x23,
  kHome = 0x24,
  kLeft = 0x25,
  kUp = 0x26,
  kRight = 0x27,
  kDown = 0x28,
  kInsert = 0x2D,
  kDelete = 0x2E,
  kKey0 = 0x30,
  kKeyA = 0x41,
  kLwin = 0x5B,
  kRwin = 0x5C,
  kApps = 0x5D,
  kNumpad0 = 0x60,
  kMultiply = 0x6A,
  kAdd = 0x6B,
  kSubtract = 0x6D,
  kDecimal = 0x6E,
  kDivide = 0x6F,
  kF1 = 0x70,
  kBrowserBack = 0xA6,
  kVolumeMute = 0xAD,
  kVolumeDown = 0xAE,
  kVolumeUp = 0xAF,
  kMediaNextTrack = 0xB0,
  kMediaPrevTrack = 0xB1,
  kMediaStop = 0xB2,
  kMediaPlayPause = 0xB3,
  kOem1 = 0xBA,
  kOemPlus = 0xBB,
  kOemComma = 0xBC,
  kOemMinus = 0xBD,
  kOemPeriod = 0xBE,
  kOem2 = 0xBF,
  kOem3 = 0xC0,
  kOem4 = 0xDB,
  kOem5 = 0xDC,
  kOem6 = 0xDD,
  kOem7 = 0xDE,
  kOem102 = 0xE2,
  kMediaRewind = 0xE3,
  kMediaFastForward = 0xE4,
  kMouse1 = 0x7000,
  kMouse2 = 0x7001,
  kMouse3 = 0x7002,
  kMouse4 = 0x7003,
  kMouse5 = 0x7004,
};

// Letter keys in Linux code order (QWERTY positions).
const char kRowQ[] = "QWERTYUIOP";  // KEY_Q .. KEY_P
const char kRowA[] = "ASDFGHJKL";   // KEY_A .. KEY_L
const char kRowZ[] = "ZXCVBNM";     // KEY_Z .. KEY_M

int Letter(int code) {
  if (code >= KEY_Q && code <= KEY_P) {
    return kRowQ[code - KEY_Q];
  }
  if (code >= KEY_A && code <= KEY_L) {
    return kRowA[code - KEY_A];
  }
  if (code >= KEY_Z && code <= KEY_M) {
    return kRowZ[code - KEY_Z];
  }
  return 0;
}

int ModifierBit(int code) {
  switch (code) {
    case KEY_LEFTSHIFT:
    case KEY_RIGHTSHIFT:
      return kModShift;
    case KEY_LEFTCTRL:
    case KEY_RIGHTCTRL:
      return kModCtrl;
    case KEY_LEFTALT:
    case KEY_RIGHTALT:
      return kModAlt;
    case KEY_LEFTMETA:
    case KEY_RIGHTMETA:
      return kModMeta;
  }
  return 0;
}

int MouseButtonKey(int code, int* bit) {
  switch (code) {
    case BTN_LEFT:
      *bit = kModButtonLeft;
      return kMouse1;
    case BTN_MIDDLE:
      *bit = kModButtonMiddle;
      return kMouse2;
    case BTN_RIGHT:
      *bit = kModButtonRight;
      return kMouse3;
    case BTN_SIDE:
      *bit = 0;
      return kMouse4;
    case BTN_EXTRA:
      *bit = 0;
      return kMouse5;
  }
  *bit = 0;
  return 0;
}

int GamepadKey(int code) {
  switch (code) {
    case BTN_SOUTH:
    case BTN_START:
    case BTN_TRIGGER:
      return kReturn;
    case BTN_EAST:
    case BTN_SELECT:
    case BTN_MODE:
    case BTN_THUMB:
      return kEscape;
    case BTN_NORTH:
    case BTN_WEST:
      return kMediaPlayPause;
    case BTN_TL:
    case BTN_TL2:
      return kMediaRewind;
    case BTN_TR:
    case BTN_TR2:
      return kMediaFastForward;
    case BTN_DPAD_UP:
      return kUp;
    case BTN_DPAD_DOWN:
      return kDown;
    case BTN_DPAD_LEFT:
      return kLeft;
    case BTN_DPAD_RIGHT:
      return kRight;
  }
  return 0;
}

bool IsMouseButton(int code) {
  return code >= BTN_MOUSE && code < BTN_JOYSTICK;
}

bool IsGamepadButton(int code) {
  return (code >= BTN_JOYSTICK && code < BTN_DIGI) ||
         (code >= BTN_DPAD_UP && code <= BTN_DPAD_RIGHT);
}

bool IsPointerClass(DevClass c) {
  return c == DevClass::kAny || c == DevClass::kMouse ||
         c == DevClass::kAbsPointer;
}

// Remembers which keys a device holds; true if |code| was held before.
bool Hold(DeviceState* dev, int code, bool down) {
  auto it = std::find(dev->held.begin(), dev->held.end(), code);
  const bool was = it != dev->held.end();
  if (down && !was) {
    dev->held.push_back(code);
  } else if (!down && was) {
    dev->held.erase(it);
  }
  return was;
}

int Clamp(double v, int lo, int hi) {
  if (v < lo) {
    return lo;
  }
  if (v > hi) {
    return hi;
  }
  return static_cast<int>(v);
}

}  // namespace

int LinuxKeyToSbKey(int code, Keymap keymap) {
  if (int l = Letter(code)) {
    if (keymap == Keymap::kDe && (l == 'Y' || l == 'Z')) {
      l = l == 'Y' ? 'Z' : 'Y';  // QWERTZ
    }
    return kKeyA + (l - 'A');
  }
  if (code >= KEY_1 && code <= KEY_9) {
    return kKey0 + 1 + (code - KEY_1);
  }
  if (code >= KEY_F1 && code <= KEY_F10) {
    return kF1 + (code - KEY_F1);
  }
  switch (code) {
    case KEY_0: return kKey0;
    case KEY_ESC: return kEscape;
    case KEY_MINUS: return kOemMinus;
    case KEY_EQUAL: return kOemPlus;
    case KEY_BACKSPACE: return kBackspace;
    case KEY_TAB: return kTab;
    case KEY_LEFTBRACE: return kOem4;
    case KEY_RIGHTBRACE: return kOem6;
    case KEY_ENTER:
    case KEY_KPENTER:
    case KEY_OK:
    case KEY_SELECT: return kReturn;
    case KEY_LEFTCTRL:
    case KEY_RIGHTCTRL: return kControl;
    case KEY_SEMICOLON: return kOem1;
    case KEY_APOSTROPHE: return kOem7;
    case KEY_GRAVE: return kOem3;
    case KEY_LEFTSHIFT:
    case KEY_RIGHTSHIFT: return kShift;
    case KEY_BACKSLASH: return kOem5;
    case KEY_COMMA: return kOemComma;
    case KEY_DOT: return kOemPeriod;
    case KEY_SLASH: return kOem2;
    case KEY_KPASTERISK: return kMultiply;
    case KEY_LEFTALT:
    case KEY_RIGHTALT: return kMenu;
    case KEY_SPACE: return kSpace;
    case KEY_CAPSLOCK: return kCapital;
    case KEY_KP0: return kNumpad0;
    case KEY_KP1: return kNumpad0 + 1;
    case KEY_KP2: return kNumpad0 + 2;
    case KEY_KP3: return kNumpad0 + 3;
    case KEY_KP4: return kNumpad0 + 4;
    case KEY_KP5: return kNumpad0 + 5;
    case KEY_KP6: return kNumpad0 + 6;
    case KEY_KP7: return kNumpad0 + 7;
    case KEY_KP8: return kNumpad0 + 8;
    case KEY_KP9: return kNumpad0 + 9;
    case KEY_KPMINUS: return kSubtract;
    case KEY_KPPLUS: return kAdd;
    case KEY_KPDOT: return kDecimal;
    case KEY_KPSLASH: return kDivide;
    case KEY_102ND: return kOem102;
    case KEY_F11: return kF1 + 10;
    case KEY_F12: return kF1 + 11;
    case KEY_HOME: return kHome;
    case KEY_UP: return kUp;
    case KEY_PAGEUP: return kPrior;
    case KEY_LEFT: return kLeft;
    case KEY_RIGHT: return kRight;
    case KEY_END: return kEnd;
    case KEY_DOWN: return kDown;
    case KEY_PAGEDOWN: return kNext;
    case KEY_INSERT: return kInsert;
    case KEY_DELETE: return kDelete;
    case KEY_MUTE: return kVolumeMute;
    case KEY_VOLUMEDOWN: return kVolumeDown;
    case KEY_VOLUMEUP: return kVolumeUp;
    case KEY_PAUSE: return kPause;
    case KEY_LEFTMETA: return kLwin;
    case KEY_RIGHTMETA: return kRwin;
    case KEY_COMPOSE: return kApps;
    case KEY_BACK: return kBrowserBack;
    case KEY_EXIT: return kEscape;
    case KEY_NEXTSONG: return kMediaNextTrack;
    case KEY_PREVIOUSSONG: return kMediaPrevTrack;
    case KEY_STOPCD:
    case KEY_STOP: return kMediaStop;
    case KEY_PLAYPAUSE:
    case KEY_PLAY:
    case KEY_PAUSECD: return kMediaPlayPause;
    case KEY_REWIND: return kMediaRewind;
    case KEY_FASTFORWARD: return kMediaFastForward;
  }
  return 0;
}

int LinuxKeyToChar(int code, bool shift, Keymap keymap) {
  const int key = LinuxKeyToSbKey(code, keymap);
  if (key >= kKeyA && key <= kKeyA + 25) {
    return shift ? key : key + ('a' - 'A');
  }
  if (!shift && key >= kKey0 && key <= kKey0 + 9) {
    return key;
  }
  if (key == kSpace) {
    return ' ';
  }
  return 0;
}

EvdevDecoder::EvdevDecoder(Keymap keymap) : keymap_(keymap) {
  SetGeometry(1920, 1080, FitWindow(1920, 1080, 1920, 1080));
}

void EvdevDecoder::SetGeometry(int win_w, int win_h,
                               const Placement& placement, int screen_w,
                               int screen_h) {
  win_w_ = std::max(1, win_w);
  win_h_ = std::max(1, win_h);
  placement_ = placement;
  if (placement_.w <= 0 || placement_.h <= 0) {
    placement_.x = placement_.y = 0;
    placement_.w = win_w_;
    placement_.h = win_h_;
  }
  screen_w_ = screen_w > 0 ? screen_w : placement_.x + placement_.w;
  screen_h_ = screen_h > 0 ? screen_h : placement_.y + placement_.h;
  px_ = placement_.x + placement_.w / 2;
  py_ = placement_.y + placement_.h / 2;
}

void EvdevDecoder::ToWindow(double* x, double* y) const {
  double wx = (px_ - placement_.x) * win_w_ / placement_.w;
  double wy = (py_ - placement_.y) * win_h_ / placement_.h;
  *x = std::min(std::max(wx, 0.0), static_cast<double>(win_w_ - 1));
  *y = std::min(std::max(wy, 0.0), static_cast<double>(win_h_ - 1));
}

void EvdevDecoder::Emit(InEvent::Type type, InEvent::Source source, int key,
                        std::vector<InEvent>* out) {
  InEvent e;
  e.type = type;
  e.source = source;
  e.key = key;
  e.modifiers = modifiers_;
  if (source == InEvent::kMouse) {
    ToWindow(&e.x, &e.y);
  }
  out->push_back(e);
}

void EvdevDecoder::Feed(DeviceState* dev, const uint8_t* data, size_t n,
                        std::vector<InEvent>* out) {
  while (n > 0) {
    const size_t take = std::min(n, kInputEventSize - dev->partial_len);
    memcpy(dev->partial + dev->partial_len, data, take);
    dev->partial_len += take;
    data += take;
    n -= take;
    if (dev->partial_len < kInputEventSize) {
      break;
    }
    dev->partial_len = 0;
    uint16_t type, code;
    int32_t value;
    memcpy(&type, dev->partial + 16, 2);
    memcpy(&code, dev->partial + 18, 2);
    memcpy(&value, dev->partial + 20, 4);
    Record(dev, type, code, value, out);
  }
}

void EvdevDecoder::Record(DeviceState* dev, int type, int code, int value,
                          std::vector<InEvent>* out) {
  switch (type) {
    case EV_SYN:
      if (code == SYN_REPORT) {
        Flush(out);
      } else if (code == SYN_DROPPED) {
        // The kernel lost events: drop the half-seen report.
        moved_ = false;
        wheel_x_ = wheel_y_ = 0;
      }
      return;
    case EV_KEY:
      Key(dev, code, value, out);
      return;
    case EV_REL:
      if (!IsPointerClass(dev->cls)) {
        return;
      }
      // One report never moves more than the screen; hostile values are cut.
      value = std::max(-100000, std::min(100000, value));
      if (code == REL_X || code == REL_Y) {
        const Placement& p = placement_;
        if (code == REL_X) {
          px_ = Clamp(px_ + value, p.x, p.x + p.w - 1);
        } else {
          py_ = Clamp(py_ + value, p.y, p.y + p.h - 1);
        }
        moved_ = true;
        pointer_seen_ = true;
      } else if (code == REL_WHEEL) {
        wheel_y_ -= value;  // evdev: positive = away from the user = up
      } else if (code == REL_HWHEEL) {
        wheel_x_ += value;
      }
      return;
    case EV_ABS:
      if (dev->cls == DevClass::kAbsPointer) {
        AbsPointer(dev, code, value);
      } else if (dev->cls == DevClass::kAny ||
                 dev->cls == DevClass::kGamepad) {
        Abs(dev, code, value, out);
      }
      return;
  }
}

void EvdevDecoder::Key(DeviceState* dev, int code, int value,
                       std::vector<InEvent>* out) {
  if (value < 0 || value > 2) {
    return;
  }
  const bool down = value != 0;
  if (code == BTN_TOUCH && dev->cls == DevClass::kAbsPointer) {
    code = BTN_LEFT;  // a touch screen: touching is the click
  }
  if (IsMouseButton(code)) {
    if (!IsPointerClass(dev->cls)) {
      return;
    }
    if (value == 2) {
      return;
    }
    int bit = 0;
    const int key = MouseButtonKey(code, &bit);
    if (!key || Hold(dev, code, down) == down) {
      return;  // unknown, or no change (a release we never saw pressed)
    }
    if (moved_) {
      Flush(out);
    }
    if (down) {
      modifiers_ |= bit;
    } else {
      modifiers_ &= ~bit;
    }
    pointer_seen_ = true;
    Emit(down ? InEvent::kPress : InEvent::kUnpress, InEvent::kMouse, key,
         out);
    return;
  }
  if (IsGamepadButton(code)) {
    if (dev->cls != DevClass::kAny && dev->cls != DevClass::kGamepad) {
      return;
    }
    const int key = GamepadKey(code);
    if (key && (value == 2 || Hold(dev, code, down) != down)) {
      Emit(down ? InEvent::kPress : InEvent::kUnpress, InEvent::kRemote, key,
           out);
      out->back().repeat = value == 2;
    }
    return;
  }
  if (code >= BTN_MISC && code < KEY_OK) {
    return;  // touch, tool and other button ranges: not used
  }
  if (dev->cls == DevClass::kMouse || dev->cls == DevClass::kAbsPointer) {
    return;
  }
  const int key = LinuxKeyToSbKey(code, keymap_);
  const int bit = ModifierBit(code);
  if (!key && !bit) {
    return;
  }
  if (value != 2 && Hold(dev, code, down) == down) {
    return;  // no change: a release we never saw pressed, or a double press
  }
  if (bit) {
    if (down) {
      modifiers_ |= bit;
    } else {
      modifiers_ &= ~bit;
    }
  }
  if (!key) {
    return;
  }
  Emit(down ? InEvent::kPress : InEvent::kUnpress, InEvent::kKeyboard, key,
       out);
  InEvent& e = out->back();
  e.repeat = value == 2;
  e.character =
      down ? LinuxKeyToChar(code, (modifiers_ & kModShift) != 0, keymap_) : 0;
  e.right_side = code == KEY_RIGHTSHIFT || code == KEY_RIGHTCTRL ||
                 code == KEY_RIGHTALT || code == KEY_RIGHTMETA;
}

void EvdevDecoder::Direction(int* held, int now, int key_neg, int key_pos,
                             std::vector<InEvent>* out) {
  if (*held == now) {
    return;
  }
  if (*held != 0) {
    Emit(InEvent::kUnpress, InEvent::kRemote, *held < 0 ? key_neg : key_pos,
         out);
  }
  if (now != 0) {
    Emit(InEvent::kPress, InEvent::kRemote, now < 0 ? key_neg : key_pos, out);
  }
  *held = now;
}

void EvdevDecoder::Abs(DeviceState* dev, int code, int value,
                       std::vector<InEvent>* out) {
  switch (code) {
    case ABS_HAT0X:
      Direction(&dev->hat_x, (value > 0) - (value < 0), kLeft, kRight, out);
      return;
    case ABS_HAT0Y:
      Direction(&dev->hat_y, (value > 0) - (value < 0), kUp, kDown, out);
      return;
    case ABS_X:
    case ABS_Y: {
      const AxisRange& r = code == ABS_X ? dev->abs_x : dev->abs_y;
      if (r.max <= r.min) {
        return;
      }
      // -1 .. 1; pressed beyond 0.5, released below 0.3 (no flicker).
      const double v =
          2.0 * (static_cast<double>(value) - r.min) / (r.max - r.min) - 1.0;
      int* held = code == ABS_X ? &dev->stick_x : &dev->stick_y;
      int now = *held;
      if (v > 0.5) {
        now = 1;
      } else if (v < -0.5) {
        now = -1;
      } else if (v > -0.3 && v < 0.3) {
        now = 0;
      }
      if (code == ABS_X) {
        Direction(held, now, kLeft, kRight, out);
      } else {
        Direction(held, now, kUp, kDown, out);
      }
      return;
    }
  }
}

void EvdevDecoder::AbsPointer(DeviceState* dev, int code, int value) {
  if (code != ABS_X && code != ABS_Y) {
    return;
  }
  const AxisRange& r = code == ABS_X ? dev->abs_x : dev->abs_y;
  if (r.max <= r.min) {
    return;
  }
  // The device's range covers the whole screen; the pointer stays on the
  // window like a mouse's.
  const double f =
      (static_cast<double>(std::max(r.min, std::min(r.max, value))) - r.min) /
      (static_cast<double>(r.max) - r.min);
  const Placement& p = placement_;
  if (code == ABS_X) {
    px_ = Clamp(f * (screen_w_ - 1), p.x, p.x + p.w - 1);
  } else {
    py_ = Clamp(f * (screen_h_ - 1), p.y, p.y + p.h - 1);
  }
  moved_ = true;
  pointer_seen_ = true;
}

void EvdevDecoder::ReleaseAll(DeviceState* dev, std::vector<InEvent>* out) {
  dev->partial_len = 0;
  const std::vector<int> held = dev->held;
  for (int code : held) {
    Key(dev, code, 0, out);
  }
  dev->held.clear();
  Direction(&dev->hat_x, 0, kLeft, kRight, out);
  Direction(&dev->hat_y, 0, kUp, kDown, out);
  Direction(&dev->stick_x, 0, kLeft, kRight, out);
  Direction(&dev->stick_y, 0, kUp, kDown, out);
}

void EvdevDecoder::Flush(std::vector<InEvent>* out) {
  if (moved_) {
    moved_ = false;
    Emit(InEvent::kMove, InEvent::kMouse, 0, out);
  }
  if (wheel_x_ || wheel_y_) {
    Emit(InEvent::kWheel, InEvent::kMouse, 0, out);
    InEvent& e = out->back();
    e.dx = static_cast<double>(std::max(-1000LL, std::min(1000LL, wheel_x_)));
    e.dy = static_cast<double>(std::max(-1000LL, std::min(1000LL, wheel_y_)));
    wheel_x_ = wheel_y_ = 0;
  }
}

}  // namespace bitsos_fb
