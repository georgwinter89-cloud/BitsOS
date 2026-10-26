// BitsOS V2 Cobalt backend: the Linux framebuffer (fbdev) or a fake one.

#include "starboard/shared/bitsos/fb/fb_device.h"

#include <errno.h>
#include <fcntl.h>
#include <limits.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <sys/sysmacros.h>
#include <unistd.h>

#include <sstream>

namespace bitsos_fb {
namespace {

std::string Errno(const char* what, const std::string& path) {
  return std::string(what) + " " + path + ": " + strerror(errno);
}

bool SameFormat(const PixelFormat& a, const PixelFormat& b) {
  return a.bits_per_pixel == b.bits_per_pixel &&
         a.red_offset == b.red_offset && a.red_length == b.red_length &&
         a.green_offset == b.green_offset &&
         a.green_length == b.green_length &&
         a.blue_offset == b.blue_offset && a.blue_length == b.blue_length;
}

bool InFolder(const std::string& path, const char* folder) {
  const size_t n = strlen(folder);
  return path.compare(0, n, folder) == 0 &&
         (path.size() == n || path[n] == '/');
}

bool InSystemFolder(const std::string& path) {
  return InFolder(path, "/dev") || InFolder(path, "/proc") ||
         InFolder(path, "/sys");
}

}  // namespace

bool SameMode(const FbGeometry& a, const FbGeometry& b) {
  return a.width == b.width && a.height == b.height && a.pitch == b.pitch &&
         SameFormat(a.format, b.format);
}

bool ValidateGeometry(const fb_var_screeninfo& var,
                      const fb_fix_screeninfo& fix,
                      size_t map_size,
                      FbGeometry* out,
                      std::string* why) {
  if (fix.type != FB_TYPE_PACKED_PIXELS ||
      (fix.visual != FB_VISUAL_TRUECOLOR &&
       fix.visual != FB_VISUAL_DIRECTCOLOR)) {
    *why = "not a packed true-colour framebuffer";
    return false;
  }
  FbGeometry g;
  g.format.bits_per_pixel = static_cast<int>(var.bits_per_pixel);
  g.format.red_offset = static_cast<int>(var.red.offset);
  g.format.red_length = static_cast<int>(var.red.length);
  g.format.green_offset = static_cast<int>(var.green.offset);
  g.format.green_length = static_cast<int>(var.green.length);
  g.format.blue_offset = static_cast<int>(var.blue.offset);
  g.format.blue_length = static_cast<int>(var.blue.length);
  const char* format_why = nullptr;
  if (var.bits_per_pixel > 32 || !g.format.Validate(&format_why)) {
    *why = format_why ? format_why : "unsupported pixel size";
    return false;
  }
  if (var.xres < 1 || var.yres < 1 || var.xres > kMaxSide ||
      var.yres > kMaxSide) {
    *why = "odd resolution";
    return false;
  }
  // All sums in 64 bits: every field is a 32-bit value from the kernel.
  const uint64_t bpp = static_cast<uint64_t>(g.format.BytesPerPixel());
  const uint64_t xres = var.xres, yres = var.yres;
  const uint64_t pitch = fix.line_length;
  if (static_cast<uint64_t>(var.xoffset) + xres > var.xres_virtual ||
      static_cast<uint64_t>(var.yoffset) + yres > var.yres_virtual) {
    *why = "visible page lies outside the virtual screen";
    return false;
  }
  if (pitch < xres * bpp || pitch > static_cast<uint64_t>(INT_MAX)) {
    *why = "line length does not fit the width";
    return false;
  }
  const uint64_t offset = var.yoffset * pitch + var.xoffset * bpp;
  // The last row needs only its pixels, not the padding after them.
  const uint64_t end = offset + (yres - 1) * pitch + xres * bpp;
  if (end > map_size) {
    *why = "framebuffer memory too small for the mode";
    return false;
  }
  g.width = static_cast<int>(xres);
  g.height = static_cast<int>(yres);
  g.pitch = static_cast<int>(pitch);
  g.offset = static_cast<size_t>(offset);
  *out = g;
  return true;
}

bool IsSystemPath(const std::string& path) {
  if (InSystemFolder(path)) {
    return true;
  }
  // The folder the file would be in, with links and ".." resolved.
  const size_t slash = path.rfind('/');
  const std::string dir = slash == std::string::npos ? "."
                          : slash == 0              ? "/"
                                                    : path.substr(0, slash);
  char resolved[PATH_MAX];
  if (!realpath(dir.c_str(), resolved)) {
    return false;  // no such folder: opening fails anyway
  }
  return InSystemFolder(resolved);
}

FbDevice::~FbDevice() {
  Close();
}

void FbDevice::Close() {
  if (map_) {
    munmap(map_, map_size_);
  }
  if (fd_ >= 0) {
    close(fd_);
  }
  fd_ = -1;
  map_ = nullptr;
  map_size_ = 0;
  pixels_.store(nullptr, std::memory_order_release);
  pending_.store(0);
  geo_ = FbGeometry();
}

bool FbDevice::Open(const Config& config, std::string* error) {
  Close();
  const bool ok = config.fb_fake ? OpenFake(config, error)
                                 : OpenReal(config.fb_path, error);
  if (!ok) {
    Close();
  }
  return ok;
}

bool FbDevice::OpenReal(const std::string& path, std::string* error) {
  fake_ = false;
  const std::string not_fb =
      path + " is not a framebuffer device (set BITSOS_FB_FAKE=WxH for a "
             "plain file)";
  // Look before opening: only a framebuffer node is ever opened read-write.
  struct stat st;
  if (stat(path.c_str(), &st) != 0) {
    *error = Errno("cannot find", path);
    return false;
  }
  if (!S_ISCHR(st.st_mode) || major(st.st_rdev) != kFbMajor) {
    *error = not_fb;
    return false;
  }
  fd_ = open(path.c_str(), O_RDWR | O_CLOEXEC);
  if (fd_ < 0) {
    *error = Errno("cannot open", path);
    return false;
  }
  struct stat st2;
  if (fstat(fd_, &st2) != 0 || st2.st_rdev != st.st_rdev) {
    *error = not_fb;
    return false;
  }
  fb_var_screeninfo var;
  fb_fix_screeninfo fix;
  memset(&var, 0, sizeof(var));
  memset(&fix, 0, sizeof(fix));
  if (ioctl(fd_, FBIOGET_VSCREENINFO, &var) != 0) {
    *error = Errno("FBIOGET_VSCREENINFO on", path);
    return false;
  }
  if (ioctl(fd_, FBIOGET_FSCREENINFO, &fix) != 0) {
    *error = Errno("FBIOGET_FSCREENINFO on", path);
    return false;
  }
  std::string why;
  if (!ValidateGeometry(var, fix, fix.smem_len, &geo_, &why)) {
    *error = path + ": " + why;
    return false;
  }
  map_size_ = fix.smem_len;
  map_ = mmap(nullptr, map_size_, PROT_READ | PROT_WRITE, MAP_SHARED, fd_, 0);
  if (map_ == MAP_FAILED) {
    map_ = nullptr;
    *error = Errno("cannot map", path);
    return false;
  }
  pixels_.store(static_cast<uint8_t*>(map_) + geo_.offset,
                std::memory_order_release);
  return true;
}

bool FbDevice::OpenFake(const Config& config, std::string* error) {
  fake_ = true;
  const std::string& path = config.fb_path;
  // A fake is a plain file the user owns; a device node is never opened.
  if (IsSystemPath(path)) {
    *error = "BITSOS_FB_FAKE refuses a path in /dev, /proc or /sys: " + path;
    return false;
  }
  // First look read-only (no create, no links followed): only a plain file
  // is then opened for writing.
  const int ro = open(path.c_str(), O_RDONLY | O_NONBLOCK | O_NOFOLLOW |
                                        O_CLOEXEC);
  struct stat st;
  memset(&st, 0, sizeof(st));
  if (ro >= 0) {
    const bool regular = fstat(ro, &st) == 0 && S_ISREG(st.st_mode);
    close(ro);
    if (!regular) {
      *error = path + " is not a plain file";
      return false;
    }
    fd_ = open(path.c_str(), O_RDWR | O_NONBLOCK | O_NOFOLLOW | O_CLOEXEC);
  } else if (errno == ENOENT) {
    // O_EXCL: if something appeared meanwhile, this fails instead.
    fd_ = open(path.c_str(),
               O_RDWR | O_CREAT | O_EXCL | O_NOFOLLOW | O_CLOEXEC, 0644);
  } else {
    *error = Errno("cannot open fake framebuffer", path);
    return false;
  }
  if (fd_ < 0) {
    *error = Errno("cannot open fake framebuffer", path);
    return false;
  }
  struct stat st2;
  if (fstat(fd_, &st2) != 0 || !S_ISREG(st2.st_mode) ||
      (ro >= 0 && (st2.st_dev != st.st_dev || st2.st_ino != st.st_ino))) {
    *error = path + " is not a plain file";
    return false;
  }
  const int w = config.fake_w;
  const int h = config.fake_h;
  const int vh = config.fake_virtual_h > h ? config.fake_virtual_h : h;
  const long long pitch =
      config.fake_pitch > 0 ? config.fake_pitch : static_cast<long long>(w) * 4;
  if (w < 1 || h < 1 || pitch < static_cast<long long>(w) * 4 ||
      pitch > INT_MAX || vh > kMaxSide * 2) {
    *error = "odd fake framebuffer size";
    return false;
  }
  map_size_ = static_cast<size_t>(pitch) * vh;
  if (static_cast<size_t>(st2.st_size) != map_size_ &&
      ftruncate(fd_, static_cast<off_t>(map_size_)) != 0) {
    *error = Errno("cannot size fake framebuffer", path);
    return false;
  }
  map_ = mmap(nullptr, map_size_, PROT_READ | PROT_WRITE, MAP_SHARED, fd_, 0);
  if (map_ == MAP_FAILED) {
    map_ = nullptr;
    *error = Errno("cannot map", path);
    return false;
  }
  geo_.width = w;
  geo_.height = h;
  geo_.pitch = static_cast<int>(pitch);
  geo_.offset = 0;
  geo_.format = PixelFormat();
  pixels_.store(static_cast<uint8_t*>(map_), std::memory_order_release);
  return true;
}

FbDevice::Change FbDevice::ApplyGeometry(const FbGeometry& g) {
  if (!map_) {
    return Change::kLost;
  }
  Change c = Change::kSame;
  if (!SameMode(g, geo_) || g.offset > map_size_ ||
      map_size_ - g.offset <
          static_cast<size_t>(g.height - 1) * g.pitch +
              static_cast<size_t>(g.width) * g.format.BytesPerPixel()) {
    c = Change::kLost;
  } else if (g.offset != geo_.offset) {
    geo_.offset = g.offset;
    pixels_.store(static_cast<uint8_t*>(map_) + g.offset,
                  std::memory_order_release);
    c = Change::kMoved;
  }
  if (c != Change::kSame) {
    // kLost wins over a pan that was not reported yet.
    int old = pending_.load();
    while (old != static_cast<int>(Change::kLost) &&
           !pending_.compare_exchange_weak(old, static_cast<int>(c))) {
    }
  }
  return c;
}

FbDevice::Change FbDevice::Recheck() {
  if (map_ && !fake_) {
    fb_var_screeninfo var;
    fb_fix_screeninfo fix;
    memset(&var, 0, sizeof(var));
    memset(&fix, 0, sizeof(fix));
    FbGeometry g;
    std::string why;
    if (ioctl(fd_, FBIOGET_VSCREENINFO, &var) != 0 ||
        ioctl(fd_, FBIOGET_FSCREENINFO, &fix) != 0 ||
        !ValidateGeometry(var, fix, map_size_, &g, &why)) {
      pending_.store(static_cast<int>(Change::kLost));
    } else {
      ApplyGeometry(g);
    }
  }
  return static_cast<Change>(pending_.exchange(0));
}

std::string FbDevice::Describe() const {
  std::ostringstream s;
  s << (fake_ ? "fake" : "fbdev") << " " << geo_.width << "x" << geo_.height
    << " bpp=" << geo_.format.bits_per_pixel << " pitch=" << geo_.pitch
    << " page_offset=" << geo_.offset
    << " rgb_offsets=" << geo_.format.red_offset << "/"
    << geo_.format.green_offset << "/" << geo_.format.blue_offset;
  return s.str();
}

}  // namespace bitsos_fb
