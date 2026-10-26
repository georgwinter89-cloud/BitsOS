//! Unsafe primitives of the IN driver lane (ring barriers, MMIO polls with timeouts, ...). Owner: IN.
//! Every unsafe block needs a `// SAFETY:` comment; the hal checker reviews each item touching this file.
