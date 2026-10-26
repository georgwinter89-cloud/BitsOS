//! Unsafe primitives of the WL driver lane (ring barriers, MMIO polls with timeouts, ...). Owner: WL.
//! Every unsafe block needs a `// SAFETY:` comment; the hal checker reviews each item touching this file.
