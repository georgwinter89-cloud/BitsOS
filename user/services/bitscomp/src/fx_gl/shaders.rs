//! WY-19: the GLSL of the effect level. Pixel programs get Smithay's prelude (`#version 100`, v_coords 0..1 over the
//! element, the uniforms `size` and `alpha`); texture programs carry the template of Smithay's own texture shader.
//! Everything is premultiplied, every output multiplied by `alpha`.

macro_rules! pixel {
    ($body:expr) => {
        concat!(
            "#ifdef GL_FRAGMENT_PRECISION_HIGH\nprecision highp float;\n#else\nprecision mediump float;\n#endif\n",
            "uniform float alpha;\nuniform vec2 size;\nvarying vec2 v_coords;\n",
            "#if defined(DEBUG_FLAGS)\nuniform float tint;\n#endif\n",
            "float rbox(vec2 p, vec2 c, vec2 half_, float r) {\n",
            "  vec2 q = abs(p - c) - half_ + vec2(r);\n",
            "  return length(max(q, vec2(0.0))) + min(max(q.x, q.y), 0.0) - r;\n}\n",
            $body
        )
    };
}

macro_rules! texture {
    ($body:expr) => {
        concat!(
            "#version 100\n//_DEFINES_\n",
            "#if defined(EXTERNAL)\n#extension GL_OES_EGL_image_external : require\n#endif\n",
            "#ifdef GL_FRAGMENT_PRECISION_HIGH\nprecision highp float;\n#else\nprecision mediump float;\n#endif\n",
            "#if defined(EXTERNAL)\nuniform samplerExternalOES tex;\n#else\nuniform sampler2D tex;\n#endif\n",
            "uniform float alpha;\nvarying vec2 v_coords;\n",
            "#if defined(DEBUG_FLAGS)\nuniform float tint;\n#endif\n",
            "float rbox(vec2 p, vec2 c, vec2 half_, float r) {\n",
            "  vec2 q = abs(p - c) - half_ + vec2(r);\n",
            "  return length(max(q, vec2(0.0))) + min(max(q.x, q.y), 0.0) - r;\n}\n",
            $body
        )
    };
}

/// CSS box-shadow: a rounded box under a Gaussian. The vertical integral in four samples, the horizontal one in closed
/// form (the error function's usual polynomial), outside the panel's own box only.
pub const SHADOW: &str = pixel!(
    r#"
uniform vec4 bx;
uniform vec4 hole;
uniform float radius;
uniform float hole_radius;
uniform float sigma;
uniform vec4 color;
vec2 erf2(vec2 x) {
  vec2 s = sign(x);
  vec2 a = abs(x);
  x = 1.0 + (0.278393 + (0.230389 + 0.078108 * (a * a)) * a) * a;
  x *= x;
  return s - s / (x * x);
}
float gauss(float x, float s) {
  return exp(-(x * x) / (2.0 * s * s)) / (2.5066283 * s);
}
float along(float x, float y, float s, float r, vec2 h) {
  float d = min(h.y - r - abs(y), 0.0);
  float curved = h.x - r + sqrt(max(0.0, r * r - d * d));
  vec2 i = 0.5 + 0.5 * erf2((x + vec2(-curved, curved)) * (0.7071068 / s));
  return i.y - i.x;
}
void main() {
  vec2 p = v_coords * size;
  vec2 h = bx.zw * 0.5;
  vec2 q = p - (bx.xy + h);
  float r = min(radius, min(h.x, h.y));
  float lo = clamp(-3.0 * sigma, q.y - h.y, q.y + h.y);
  float hi = clamp(3.0 * sigma, q.y - h.y, q.y + h.y);
  float st = (hi - lo) / 4.0;
  float y = lo + st * 0.5;
  float v = 0.0;
  for (int i = 0; i < 4; i++) {
    v += along(q.x, q.y - y, sigma, r, h) * gauss(y, sigma) * st;
    y += st;
  }
  float outside = clamp(rbox(p, hole.xy + hole.zw * 0.5, hole.zw * 0.5, hole_radius) + 0.5, 0.0, 1.0);
  gl_FragColor = color * clamp(v, 0.0, 1.0) * outside * alpha;
}
"#
);

/// `.beam`: linear-gradient(to bottom, 0, mid at 70 %, peak at 100 %).
pub const BEAM: &str = pixel!(
    r#"
uniform vec3 color;
uniform float mid;
uniform float peak;
void main() {
  float y = v_coords.y;
  float a = y < 0.7 ? mix(0.0, mid, y / 0.7) : mix(mid, peak, (y - 0.7) / 0.3);
  gl_FragColor = vec4(color * a, a) * alpha;
}
"#
);

/// `.scan`: one row in `period`, on the output's grid.
pub const SCAN: &str = pixel!(
    r#"
uniform float period;
uniform vec4 color;
void main() {
  float row = floor(v_coords.y * size.y);
  gl_FragColor = (mod(row, period) < 0.5 ? color : vec4(0.0)) * alpha;
}
"#
);

/// The map's disc (radial gradient, border, two rings, the cross) with `.radar`'s conic sweep: 280 deg of nothing,
/// then up to 38 % at 360 deg, turned by `angle`.
pub const RADAR: &str = pixel!(
    r#"
uniform float angle;
uniform vec3 accent;
uniform vec4 ground;
void over(inout vec4 d, vec3 c, float a) {
  d = vec4(c * a, a) + d * (1.0 - a);
}
void main() {
  vec2 p = v_coords * size;
  vec2 q = p - size * 0.5;
  float R = size.x * 0.5;
  float k = size.x / 232.0;
  float d = length(q);
  vec4 c = mix(vec4(accent * 0.07, 0.07), ground, clamp(d / R, 0.0, 1.0));
  float a = atan(q.x, -q.y) - angle;
  a = mod(a, 6.2831853) / 6.2831853 * 360.0;
  over(c, accent, a < 280.0 ? 0.0 : (a - 280.0) / 80.0 * 0.38);
  over(c, accent, 0.2 * clamp(1.0 - abs(d - 77.0 * k) / k, 0.0, 1.0));
  over(c, accent, 0.2 * clamp(1.0 - abs(d - 38.0 * k) / k, 0.0, 1.0));
  over(c, accent, 0.15 * clamp(1.0 - abs(q.x) / k, 0.0, 1.0));
  over(c, accent, 0.15 * clamp(1.0 - abs(q.y) / k, 0.0, 1.0));
  over(c, accent, 0.35 * clamp(1.0 - abs(d - R + 0.5 * k) / k, 0.0, 1.0));
  gl_FragColor = c * clamp(R - d + 0.5, 0.0, 1.0) * alpha;
}
"#
);

/// A dashed ring: as many whole dashes as fit (an SVG dash that does not fit leaves a seam at its start), butt ends,
/// with a soft stroke glow where the dash is.
pub const RING: &str = pixel!(
    r#"
uniform float r;
uniform float width;
uniform float on;
uniform float off;
uniform float phase;
uniform float glow;
uniform vec4 color;
void main() {
  vec2 q = v_coords * size - size * 0.5;
  float d = length(q);
  float turn = 6.2831853 * r;
  float n = max(floor(turn / (on + off) + 0.5), 1.0);
  float per = turn / n;
  float len = on * per / (on + off);
  float s = (fract(atan(q.x, -q.y) / 6.2831853 - phase + 1.0)) * turn;
  float along = abs(mod(s, per) - len * 0.5) - len * 0.5;
  float across = abs(d - r) - width * 0.5;
  float sd = length(max(vec2(along, across), vec2(0.0))) + min(max(along, across), 0.0);
  float core = clamp(0.5 - sd, 0.0, 1.0);
  float halo = glow > 0.0 ? 0.35 * exp(-max(sd, 0.0) * max(sd, 0.0) / (2.0 * glow * glow)) : 0.0;
  gl_FragColor = color * max(core, halo) * alpha;
}
"#
);

/// A disc, or a circle line when `stroke` > 0, around the middle, with a glow.
pub const DOT: &str = pixel!(
    r#"
uniform float r;
uniform float stroke;
uniform float glow;
uniform vec4 color;
void main() {
  float d = length(v_coords * size - size * 0.5);
  float sd = stroke > 0.0 ? abs(d - r) - stroke * 0.5 : d - r;
  float core = clamp(0.5 - sd, 0.0, 1.0);
  float halo = glow > 0.0 ? 0.55 * exp(-max(sd, 0.0) * max(sd, 0.0) / (2.0 * glow * glow)) : 0.0;
  gl_FragColor = color * max(core, halo) * alpha;
}
"#
);

/// One straight stretch of a thread: dashes along it from `offset`, with a soft glow.
pub const FLOW: &str = pixel!(
    r#"
uniform vec2 a;
uniform vec2 b;
uniform float width;
uniform float on;
uniform float off;
uniform float offset;
uniform float glow;
uniform vec4 color;
void main() {
  vec2 p = v_coords * size - a;
  vec2 e = b - a;
  float l = max(length(e), 0.001);
  float t = clamp(dot(p, e) / (l * l), 0.0, 1.0);
  float across = length(p - e * t);
  float s = t * l + offset;
  float per = on + off;
  float along = abs(mod(s, per) - on * 0.5) - on * 0.5;
  vec2 v = vec2(along, across - width * 0.5);
  float sd = length(max(v, vec2(0.0))) + min(max(v.x, v.y), 0.0);
  float core = clamp(0.5 - sd, 0.0, 1.0);
  float halo = 0.5 * exp(-max(sd, 0.0) * max(sd, 0.0) / (2.0 * glow * glow));
  gl_FragColor = color * max(core, halo) * alpha;
}
"#
);

/// One direction of the blur: 25 taps of a Gaussian whose sigma is 4 steps (`step_uv` is one step, a quarter sigma).
pub const BLUR: &str = texture!(
    r#"
uniform vec2 step_uv;
void main() {
  vec4 c = vec4(0.0);
  float sum = 0.0;
  for (int i = -12; i <= 12; i++) {
    float x = float(i);
    float w = exp(-0.5 * x * x / 16.0);
    c += texture2D(tex, v_coords + step_uv * x) * w;
    sum += w;
  }
  gl_FragColor = c / sum * alpha;
}
"#
);

/// A texture inside a rounded rectangle, saturated as CSS `saturate()`: the glass, and the morph's flying picture.
pub const GLASS: &str = texture!(
    r#"
uniform vec4 uv;
uniform vec2 size_px;
uniform float radius;
uniform float saturate;
void main() {
  vec4 c = texture2D(tex, v_coords);
  float s = saturate;
  vec3 o = vec3(
    dot(c.rgb, vec3(0.213 + 0.787 * s, 0.715 - 0.715 * s, 0.072 - 0.072 * s)),
    dot(c.rgb, vec3(0.213 - 0.213 * s, 0.715 + 0.285 * s, 0.072 - 0.072 * s)),
    dot(c.rgb, vec3(0.213 - 0.213 * s, 0.715 - 0.715 * s, 0.072 + 0.928 * s)));
  vec2 p = (v_coords - uv.xy) / uv.zw * size_px;
  float inside = clamp(0.5 - rbox(p, size_px * 0.5, size_px * 0.5, radius), 0.0, 1.0);
  gl_FragColor = vec4(clamp(o, vec3(0.0), vec3(c.a)), c.a) * inside * alpha;
}
"#
);

/// The circle wipe: the old picture with a soft-edged hole.
pub const WIPE: &str = texture!(
    r#"
uniform vec2 centre;
uniform float r;
uniform vec2 size_px;
void main() {
  float d = length(v_coords * size_px - centre);
  gl_FragColor = texture2D(tex, v_coords) * clamp(d - r + 0.5, 0.0, 1.0) * alpha;
}
"#
);

/// The glitch: the new picture blurred (13 taps), shifted, skewed about the middle and hue-turned.
pub const GLITCH: &str = texture!(
    r#"
uniform vec2 size_px;
uniform float blur;
uniform float dx;
uniform float skew;
uniform float hue;
void main() {
  vec2 p = v_coords * size_px;
  p.x -= dx + skew * (p.y - size_px.y * 0.5);
  vec2 uv = p / size_px;
  vec2 b = vec2(blur) / size_px / 2.0;
  vec4 c = texture2D(tex, uv) * 4.0;
  c += (texture2D(tex, uv + vec2(b.x, 0.0)) + texture2D(tex, uv - vec2(b.x, 0.0)) +
        texture2D(tex, uv + vec2(0.0, b.y)) + texture2D(tex, uv - vec2(0.0, b.y))) * 2.0;
  c += (texture2D(tex, uv + b) + texture2D(tex, uv - b) +
        texture2D(tex, uv + vec2(b.x, -b.y)) + texture2D(tex, uv + vec2(-b.x, b.y))) * 1.5;
  c += (texture2D(tex, uv + 2.0 * vec2(b.x, 0.0)) + texture2D(tex, uv - 2.0 * vec2(b.x, 0.0)) +
        texture2D(tex, uv + 2.0 * vec2(0.0, b.y)) + texture2D(tex, uv - 2.0 * vec2(0.0, b.y)));
  c /= 22.0;
  float co = cos(hue);
  float si = sin(hue);
  mat3 m = mat3(
    0.213 + 0.787 * co - 0.213 * si, 0.213 - 0.213 * co + 0.143 * si, 0.213 - 0.213 * co - 0.787 * si,
    0.715 - 0.715 * co - 0.715 * si, 0.715 + 0.285 * co + 0.140 * si, 0.715 - 0.715 * co + 0.715 * si,
    0.072 - 0.072 * co + 0.928 * si, 0.072 - 0.072 * co - 0.283 * si, 0.072 + 0.928 * co + 0.072 * si);
  vec3 o = clamp(m * c.rgb, vec3(0.0), vec3(c.a));
  float in_x = step(0.0, uv.x) * step(uv.x, 1.0);
  gl_FragColor = vec4(o, c.a) * in_x * alpha;
}
"#
);

/// A picture as a card turned about its upright middle, in perspective (`perspective(P) rotateY(turn) scale(s)`).
/// The projection is solved backwards, from the screen to the card.
pub const CARD: &str = texture!(
    r#"
uniform vec2 size_px;
uniform float turn;
uniform float persp;
uniform float scale;
uniform float shift;
uniform float radius;
void main() {
  vec2 c = size_px * 0.5;
  vec2 q = (v_coords * size_px - c - vec2(shift, 0.0)) / scale;
  float co = cos(turn);
  float si = sin(turn);
  float x = q.x * persp / (persp * co - q.x * si);
  float f = persp / (persp + x * si);
  vec2 p = vec2(x, q.y / f) + c;
  float inside = clamp(0.5 - rbox(p, c, c, radius), 0.0, 1.0);
  gl_FragColor = texture2D(tex, p / size_px) * inside * alpha;
}
"#
);
