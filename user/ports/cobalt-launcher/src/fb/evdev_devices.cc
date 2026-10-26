// BitsOS V2 Cobalt backend: the set of open input devices.

#include "starboard/shared/bitsos/fb/evdev_devices.h"

#include <errno.h>
#include <fcntl.h>
#include <glob.h>
#include <linux/input.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/stat.h>
#include <unistd.h>

#include <algorithm>
#include <set>

namespace bitsos_fb {
namespace {

static_assert(sizeof(struct input_event) == kInputEventSize,
              "the decoder assumes the x86-64 input_event layout");

bool Bit(const std::vector<unsigned long>& bits, int n) {
  const size_t per = sizeof(unsigned long) * 8;
  const size_t i = static_cast<size_t>(n) / per;
  return i < bits.size() && ((bits[i] >> (n % per)) & 1);
}

bool GetBits(int fd, int type, int max, std::vector<unsigned long>* bits) {
  const size_t per = sizeof(unsigned long) * 8;
  bits->assign(max / per + 1, 0);
  const int len = static_cast<int>(bits->size() * sizeof(unsigned long));
  return ioctl(fd, EVIOCGBIT(type, len), bits->data()) >= 0;
}

const char* ClassName(DevClass c) {
  switch (c) {
    case DevClass::kAny:
      return "raw event stream";
    case DevClass::kKeyboard:
      return "keyboard";
    case DevClass::kMouse:
      return "mouse";
    case DevClass::kGamepad:
      return "gamepad";
    case DevClass::kAbsPointer:
      return "absolute pointer";
  }
  return "?";
}

enum class Kind { kTaken, kUnwanted, kError };

// An ioctl that a non-evdev node rejects means "not an input device"; any
// other failure may pass (the device is still being set up).
Kind Failed() {
  return errno == ENOTTY || errno == EINVAL ? Kind::kUnwanted : Kind::kError;
}

// What a real evdev node can send. kError: the node did not answer (tried
// again later); kUnwanted: nothing useful for Cobalt.
Kind Classify(int fd, DevClass* cls) {
  std::vector<unsigned long> ev, keys, rel, abs, props;
  if (!GetBits(fd, 0, EV_MAX, &ev)) {
    return Failed();
  }
  if (!Bit(ev, EV_KEY)) {
    return Kind::kUnwanted;
  }
  if (!GetBits(fd, EV_KEY, KEY_MAX, &keys)) {
    return Failed();
  }
  const bool has_rel = Bit(ev, EV_REL) && GetBits(fd, EV_REL, REL_MAX, &rel) &&
                       Bit(rel, REL_X) && Bit(rel, REL_Y);
  const bool has_abs = Bit(ev, EV_ABS) && GetBits(fd, EV_ABS, ABS_MAX, &abs) &&
                       Bit(abs, ABS_X) && Bit(abs, ABS_Y);
  // Touchpads say INPUT_PROP_POINTER; their absolute finger position is not
  // a screen position.
  props.assign(INPUT_PROP_MAX / (sizeof(unsigned long) * 8) + 1, 0);
  const bool touchpad =
      ioctl(fd, EVIOCGPROP(static_cast<int>(props.size() *
                                            sizeof(unsigned long))),
            props.data()) >= 0 &&
      Bit(props, INPUT_PROP_POINTER);
  const bool gamepad = Bit(keys, BTN_GAMEPAD) || Bit(keys, BTN_JOYSTICK);
  const bool mouse = has_rel && Bit(keys, BTN_LEFT);
  const bool tablet = has_abs && !has_rel && !touchpad &&
                      (Bit(keys, BTN_LEFT) || Bit(keys, BTN_TOUCH));
  const bool keyboard = Bit(keys, KEY_SPACE) || Bit(keys, KEY_ENTER) ||
                        Bit(keys, KEY_UP) || Bit(keys, KEY_OK);
  if (gamepad) {
    *cls = DevClass::kGamepad;
  } else if (mouse && keyboard) {
    *cls = DevClass::kAny;
  } else if (mouse) {
    *cls = DevClass::kMouse;
  } else if (tablet) {
    *cls = DevClass::kAbsPointer;
  } else if (keyboard) {
    *cls = DevClass::kKeyboard;
  } else {
    return Kind::kUnwanted;
  }
  return Kind::kTaken;
}

void ReadAxis(int fd, int axis, AxisRange* range) {
  struct input_absinfo info;
  memset(&info, 0, sizeof(info));
  if (ioctl(fd, EVIOCGABS(axis), &info) == 0 && info.maximum > info.minimum) {
    range->min = info.minimum;
    range->max = info.maximum;
  }
}

}  // namespace

EvdevDevices::EvdevDevices(const Config& config, LogFn log)
    : config_(config), log_(log) {}

EvdevDevices::~EvdevDevices() {
  while (!devices_.empty()) {
    CloseAt(devices_.size() - 1, nullptr, nullptr, nullptr);
  }
}

bool EvdevDevices::IsOpen(const std::string& path) const {
  for (const auto& d : devices_) {
    if (d->path == path) {
      return true;
    }
  }
  return false;
}

EvdevDevices::Skipped* EvdevDevices::FindSkipped(const std::string& path) {
  for (Skipped& s : skipped_) {
    if (s.path == path) {
      return &s;
    }
  }
  return nullptr;
}

bool EvdevDevices::Skip(const std::string& path, unsigned long long inode,
                        bool final) {
  if (Skipped* s = FindSkipped(path)) {
    const bool news = s->inode != inode || s->final != final;
    s->inode = inode;
    s->final = final;
    return news;
  }
  skipped_.push_back(Skipped{path, inode, final});
  return true;
}

void EvdevDevices::Rescan() {
  std::set<std::string> seen;
  for (const std::string& pattern : config_.evdev_patterns) {
    glob_t g;
    memset(&g, 0, sizeof(g));
    if (glob(pattern.c_str(), GLOB_NOSORT, nullptr, &g) == 0) {
      for (size_t i = 0; i < g.gl_pathc; ++i) {
        const std::string path(g.gl_pathv[i]);
        seen.insert(path);
        struct stat st;
        if (IsOpen(path) || stat(path.c_str(), &st) != 0) {
          continue;
        }
        const Skipped* s = FindSkipped(path);
        if (s && s->final && s->inode == st.st_ino) {
          continue;
        }
        OpenOne(path);
      }
    }
    globfree(&g);
  }
  // Forget nodes that are gone: when they come back, they get a new look.
  skipped_.erase(std::remove_if(skipped_.begin(), skipped_.end(),
                                [&](const Skipped& s) {
                                  return seen.count(s.path) == 0;
                                }),
                 skipped_.end());
}

void EvdevDevices::OpenOne(const std::string& path) {
  struct stat st;
  if (stat(path.c_str(), &st) != 0 || S_ISDIR(st.st_mode)) {
    return;  // gone again, or a folder like /dev/input/by-id
  }
  const bool fifo = S_ISFIFO(st.st_mode);
  const bool regular = S_ISREG(st.st_mode);
  // A FIFO is opened read-write so that it never signals end-of-file.
  const int flags = (fifo ? O_RDWR : O_RDONLY) | O_NONBLOCK | O_CLOEXEC;
  const int fd = open(path.c_str(), flags);
  if (fd < 0) {
    const int err = errno;
    if (Skip(path, st.st_ino, false)) {
      log_("input: cannot open " + path + " (tried again later): " +
           strerror(err));
    }
    return;
  }
  std::unique_ptr<Device> d(new Device);
  d->path = path;
  d->fd = fd;
  d->regular_file = regular;
  d->inode = st.st_ino;
  if (S_ISCHR(st.st_mode)) {
    const Kind kind = Classify(fd, &d->state.cls);
    if (kind != Kind::kTaken) {
      const int err = errno;
      close(fd);
      const bool final = kind == Kind::kUnwanted;
      if (Skip(path, st.st_ino, final) && !final) {
        log_("input: " + path + " does not answer (tried again later): " +
             strerror(err));
      }
      return;
    }
    if (d->state.cls == DevClass::kGamepad ||
        d->state.cls == DevClass::kAbsPointer) {
      ReadAxis(fd, ABS_X, &d->state.abs_x);
      ReadAxis(fd, ABS_Y, &d->state.abs_y);
    }
    if (config_.evdev_grab && ioctl(fd, EVIOCGRAB, 1) != 0) {
      log_("input: could not take " + path + " exclusively: " +
           strerror(errno));
    }
  } else if (!fifo && !regular) {
    close(fd);
    if (Skip(path, st.st_ino, true)) {
      log_("input: " + path + " is neither an evdev node, a FIFO nor a file");
    }
    return;
  }
  // Opened now: a later failure is news again.
  skipped_.erase(std::remove_if(skipped_.begin(), skipped_.end(),
                                [&](const Skipped& s) { return s.path == path; }),
                 skipped_.end());
  log_("input: opened " + path + " as " + ClassName(d->state.cls));
  devices_.push_back(std::move(d));
}

void EvdevDevices::CloseAt(size_t index, const char* why,
                           EvdevDecoder* decoder, std::vector<InEvent>* out) {
  Device* d = devices_[index].get();
  if (decoder && out) {
    decoder->ReleaseAll(&d->state, out);
  }
  if (why) {
    log_("input: closed " + d->path + " (" + why + ")");
  }
  close(d->fd);
  devices_.erase(devices_.begin() + static_cast<long>(index));
}

void EvdevDevices::AddPollFds(std::vector<pollfd>* fds) const {
  for (const auto& d : devices_) {
    pollfd p;
    p.fd = d->fd;
    p.events = POLLIN;
    p.revents = 0;
    fds->push_back(p);
  }
}

void EvdevDevices::ReadReady(const std::vector<pollfd>& fds,
                             EvdevDecoder* decoder,
                             std::vector<InEvent>* out) {
  uint8_t buf[kInputEventSize * 64];
  for (size_t i = 0; i < devices_.size();) {
    Device* d = devices_[i].get();
    short revents = 0;
    for (const pollfd& p : fds) {
      if (p.fd == d->fd) {
        revents = p.revents;
        break;
      }
    }
    if (!revents) {
      ++i;
      continue;
    }
    const char* failed = nullptr;
    // Bounded: a device that never runs dry cannot hold the loop forever.
    for (int round = 0; round < 64; ++round) {
      const ssize_t n = read(d->fd, buf, sizeof(buf));
      if (n > 0) {
        decoder->Feed(&d->state, buf, static_cast<size_t>(n), out);
        continue;
      }
      if (n == 0) {
        failed = d->regular_file ? "end of file" : "end of stream";
      } else if (errno != EAGAIN && errno != EINTR) {
        failed = strerror(errno);
      }
      break;
    }
    if (failed) {
      // A test file is read once; a device that failed is opened again by
      // Rescan only if its node is still (or again) there.
      if (d->regular_file) {
        Skip(d->path, d->inode, true);
      }
      CloseAt(i, failed, decoder, out);
    } else {
      ++i;
    }
  }
}

}  // namespace bitsos_fb
