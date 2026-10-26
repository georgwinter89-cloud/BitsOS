// BitsOS V2 Cobalt backend: choosing it, and its dev.bitsos.Canvas.
//
// The Linux loader_app keeps its X11 window (the debug backend on CachyOS);
// BITSOS_BACKEND=fb switches the same binary to the framebuffer + evdev
// backend that V2 uses. ALSA sound is the same in both.

#ifndef STARBOARD_SHARED_BITSOS_FB_BACKEND_FB_H_
#define STARBOARD_SHARED_BITSOS_FB_BACKEND_FB_H_

#include "starboard/shared/bitsos/fb/fb_config.h"

namespace starboard {
namespace bitsos {

// Settings from the environment, read once.
const bitsos_fb::Config& FbConfig();

// True when BITSOS_BACKEND=fb.
bool FbBackendSelected();

// The "dev.bitsos.Canvas" extension of the framebuffer backend.
const void* GetBitsosCanvasApiFb();

}  // namespace bitsos
}  // namespace starboard

#endif  // STARBOARD_SHARED_BITSOS_FB_BACKEND_FB_H_
