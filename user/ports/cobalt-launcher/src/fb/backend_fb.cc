// BitsOS V2 Cobalt backend: choosing it, and its dev.bitsos.Canvas.

#include "starboard/shared/bitsos/fb/backend_fb.h"

#include <stdlib.h>

#include "starboard/extension/bitsos_canvas.h"
#include "starboard/shared/bitsos/fb/application_bitsos_fb.h"

namespace starboard {
namespace bitsos {
namespace {

const char* RealGetenv(const char* name) {
  return getenv(name);
}

void Present(void* window_handle,
             const void* pixels,
             int32_t stride,
             int32_t width,
             int32_t height,
             int32_t damage_x,
             int32_t damage_y,
             int32_t damage_width,
             int32_t damage_height) {
  ApplicationBitsosFb* app = ApplicationBitsosFb::Get();
  if (app) {
    app->Present(window_handle, pixels, stride, width, height, damage_x,
                 damage_y, damage_width, damage_height);
  }
}

const StarboardExtensionBitsosCanvasApi kCanvasApi = {
    kStarboardExtensionBitsosCanvasName,
    1,
    &Present,
};

}  // namespace

const bitsos_fb::Config& FbConfig() {
  static const bitsos_fb::Config config = bitsos_fb::ReadConfig(&RealGetenv);
  return config;
}

bool FbBackendSelected() {
  return FbConfig().fb_backend;
}

const void* GetBitsosCanvasApiFb() {
  return &kCanvasApi;
}

}  // namespace bitsos
}  // namespace starboard
