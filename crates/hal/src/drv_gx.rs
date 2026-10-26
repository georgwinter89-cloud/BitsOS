//! Unsafe primitives of the GX driver lane (ring barriers, MMIO polls with timeouts, ...). Owner: GX.
//! Every unsafe block needs a `// SAFETY:` comment; the hal checker reviews each item touching this file.
