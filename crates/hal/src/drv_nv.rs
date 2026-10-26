//! Unsafe primitives of the NV driver lane (ring barriers, MMIO polls with timeouts, ...). Owner: NV.
//! Every unsafe block needs a `// SAFETY:` comment; the hal checker reviews each item touching this file.
