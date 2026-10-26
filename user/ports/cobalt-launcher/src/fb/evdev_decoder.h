// BitsOS V2 Cobalt backend: Linux evdev records -> input events for Cobalt.
//
// Plain C++17 (no Starboard, only Linux's event numbers), so the host tests
// feed it any bytes. Every record is 24 bytes (struct input_event on x86-64:
// time 16, type 2, code 2, value 4); records may arrive in pieces.
//
// What comes out is already in Cobalt's terms: key values are SbKey values
// (the Windows virtual key codes, e.g. 0x26 = Up), modifier bits are
// SbKeyModifiers. Cobalt 27's Ozone layer drops gamepad-type events, so a
// pad is turned into remote-control keys: d-pad, hat and left stick =
// arrows, A = Enter, B = Escape (back), X = play/pause, shoulders = rewind /
// fast forward, Start = Enter, Select/Mode = Escape. An absolute pointer
// (a tablet such as QEMU's usb-tablet, or a touch screen) moves the pointer
// to where it points; touchpads are not supported (they need libinput-like
// handling and are not classified as pointers).

#ifndef STARBOARD_SHARED_BITSOS_FB_EVDEV_DECODER_H_
#define STARBOARD_SHARED_BITSOS_FB_EVDEV_DECODER_H_

#include <stddef.h>
#include <stdint.h>

#include <vector>

#include "starboard/shared/bitsos/fb/fb_config.h"
#include "starboard/shared/bitsos/fb/fb_pixels.h"

namespace bitsos_fb {

constexpr size_t kInputEventSize = 24;

// What a device was found to be. kAny = unknown (a FIFO or file from a test):
// every event type is taken (EV_ABS as a gamepad).
enum class DevClass { kAny, kKeyboard, kMouse, kGamepad, kAbsPointer };

// SbKeyModifiers bits.
enum : int {
  kModAlt = 1 << 0,
  kModCtrl = 1 << 1,
  kModMeta = 1 << 2,
  kModShift = 1 << 3,
  kModButtonLeft = 1 << 4,
  kModButtonRight = 1 << 5,
  kModButtonMiddle = 1 << 6,
};

struct AxisRange {
  int min = -32768;
  int max = 32767;
};

// Per-device state; owned by the device list, handed to Feed.
struct DeviceState {
  DevClass cls = DevClass::kAny;
  uint8_t partial[kInputEventSize];
  size_t partial_len = 0;
  AxisRange abs_x;
  AxisRange abs_y;
  // -1, 0, 1 per axis: what is held down now from the hat and the stick.
  int hat_x = 0;
  int hat_y = 0;
  int stick_x = 0;
  int stick_y = 0;
  // Linux codes of the keys and buttons held down now, so that they can be
  // let go when the device disappears.
  std::vector<int> held;
};

struct InEvent {
  enum Type { kPress, kUnpress, kMove, kWheel };
  enum Source { kKeyboard, kMouse, kRemote };
  Type type = kPress;
  Source source = kKeyboard;
  int key = 0;            // SbKey value (0 = unknown)
  int character = 0;      // Unicode code point for text keys, else 0
  bool repeat = false;    // key held down (evdev value 2)
  bool right_side = false;  // right-hand modifier key
  int modifiers = 0;      // kMod* bits
  double x = 0;           // pointer position in window pixels
  double y = 0;
  double dx = 0;          // wheel steps (+y = down, +x = right)
  double dy = 0;
};

class EvdevDecoder {
 public:
  explicit EvdevDecoder(Keymap keymap = Keymap::kDe);

  // Window size and where it sits on the screen; the pointer moves in screen
  // pixels and is reported in window pixels. Puts the pointer in the middle.
  // |screen_w|/|screen_h| (0 = the placement's corner) is what an absolute
  // pointer's range covers.
  void SetGeometry(int win_w, int win_h, const Placement& placement,
                   int screen_w = 0, int screen_h = 0);

  // Decodes |n| bytes from |dev| and appends the resulting events.
  void Feed(DeviceState* dev, const uint8_t* data, size_t n,
            std::vector<InEvent>* out);

  // The device is gone: lets go of everything it still holds (keys,
  // modifiers, buttons, pad directions) and forgets a half record.
  void ReleaseAll(DeviceState* dev, std::vector<InEvent>* out);

  // Pointer in screen pixels, and whether any mouse has moved or clicked.
  int pointer_screen_x() const { return static_cast<int>(px_); }
  int pointer_screen_y() const { return static_cast<int>(py_); }
  bool pointer_seen() const { return pointer_seen_; }

 private:
  void Record(DeviceState* dev, int type, int code, int value,
              std::vector<InEvent>* out);
  void Key(DeviceState* dev, int code, int value, std::vector<InEvent>* out);
  void Abs(DeviceState* dev, int code, int value, std::vector<InEvent>* out);
  void AbsPointer(DeviceState* dev, int code, int value);
  void Flush(std::vector<InEvent>* out);
  void Direction(int* held, int now, int key_neg, int key_pos,
                 std::vector<InEvent>* out);
  void Emit(InEvent::Type type, InEvent::Source source, int key,
            std::vector<InEvent>* out);
  void ToWindow(double* x, double* y) const;

  Keymap keymap_;
  int modifiers_ = 0;
  int win_w_ = 1920;
  int win_h_ = 1080;
  Placement placement_;
  int screen_w_ = 1920;
  int screen_h_ = 1080;
  double px_ = 0;
  double py_ = 0;
  bool pointer_seen_ = false;
  bool moved_ = false;
  long long wheel_x_ = 0;
  long long wheel_y_ = 0;
};

// Linux key code -> SbKey value (0 if not used), and the text it types.
int LinuxKeyToSbKey(int code, Keymap keymap);
int LinuxKeyToChar(int code, bool shift, Keymap keymap);

}  // namespace bitsos_fb

#endif  // STARBOARD_SHARED_BITSOS_FB_EVDEV_DECODER_H_
