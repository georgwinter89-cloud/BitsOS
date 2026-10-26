//! Unsafe primitives of the KS driver lane (ring barriers, MMIO polls with timeouts, ...). Owner: KS.
//! Every unsafe block needs a `// SAFETY:` comment; the hal checker reviews each item touching this file.
