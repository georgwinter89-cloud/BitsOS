// BitsOS V2 Cobalt backend: the set of open input devices.
//
// Devices come from BITSOS_EVDEV patterns and are looked for again every few
// seconds (a keyboard plugged in later just works; one pulled out is closed
// on its first read error, and whatever it still held is let go). A node
// that could not be opened (e.g. udev has not set its rights yet) is tried
// again at every rescan. A real evdev node is sorted by what it can send
// (keyboard, mouse, pad, tablet; others are closed again and not tried
// again while the same node is there) and grabbed if asked. A
// FIFO or plain file (tests) cannot answer ioctls and is read as kAny; a FIFO
// is opened read-write so it never reports end-of-file when a writer leaves.

#ifndef STARBOARD_SHARED_BITSOS_FB_EVDEV_DEVICES_H_
#define STARBOARD_SHARED_BITSOS_FB_EVDEV_DEVICES_H_

#include <poll.h>

#include <memory>
#include <string>
#include <vector>

#include "starboard/shared/bitsos/fb/evdev_decoder.h"
#include "starboard/shared/bitsos/fb/fb_config.h"

namespace bitsos_fb {

class EvdevDevices {
 public:
  // |log| receives one line per device change ("opened ... as mouse").
  typedef void (*LogFn)(const std::string& line);

  EvdevDevices(const Config& config, LogFn log);
  EvdevDevices(const EvdevDevices&) = delete;
  EvdevDevices& operator=(const EvdevDevices&) = delete;
  ~EvdevDevices();

  // Opens devices matching the patterns that are not open yet.
  void Rescan();

  // Appends one pollfd per open device (events = POLLIN).
  void AddPollFds(std::vector<pollfd>* fds) const;

  // Reads whatever the devices whose pollfd shows activity have, decodes it
  // and appends the events. Devices that fail are closed.
  void ReadReady(const std::vector<pollfd>& fds, EvdevDecoder* decoder,
                 std::vector<InEvent>* out);

  size_t size() const { return devices_.size(); }

 private:
  struct Device {
    std::string path;
    int fd = -1;
    bool regular_file = false;
    unsigned long long inode = 0;
    DeviceState state;
  };

  // A node looked at and not taken. |final|: not wanted (or a test file
  // already read), never retried while the same node is there; otherwise
  // it failed to open and is retried (logged once).
  struct Skipped {
    std::string path;
    unsigned long long inode;
    bool final;
  };

  bool IsOpen(const std::string& path) const;
  Skipped* FindSkipped(const std::string& path);
  // Records |path|; true if this is news (worth a log line).
  bool Skip(const std::string& path, unsigned long long inode, bool final);
  void OpenOne(const std::string& path);
  // Closes a device; with |decoder|, first lets go of what it holds.
  void CloseAt(size_t index, const char* why, EvdevDecoder* decoder,
               std::vector<InEvent>* out);

  Config config_;
  LogFn log_;
  std::vector<std::unique_ptr<Device>> devices_;
  // Entries go away once their path is no longer found, so a node that
  // comes back (re-plugged) is looked at again.
  std::vector<Skipped> skipped_;
};

}  // namespace bitsos_fb

#endif  // STARBOARD_SHARED_BITSOS_FB_EVDEV_DEVICES_H_
