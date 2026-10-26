//! `event!`, `try_event!` and the message helpers (the `event!` syntax is the G0 contract).

/// `klog::event!(Info, "net", "link.up", ifname = name, speed_mbps = 1000);`
/// `klog::event!(Warn, "net", "dhcp.retry", attempt = n; "no offer after {} s", secs);`
#[macro_export]
macro_rules! event {
    ($lvl:ident, $sub:expr, $name:expr $(, $k:ident = $v:expr)* $(,)? $(; $($fmt:tt)+)?) => {{
        if $crate::Level::$lvl <= $crate::min_level() {
            // The match keeps every value's temporaries alive until the event is emitted.
            match ($(&$v,)*) {
                ($($k,)*) => {
                    let fields = [$($crate::Field { key: stringify!($k), value: $crate::IntoValue::into_value($k) }),*];
                    let ev = $crate::Event {
                        level: $crate::Level::$lvl,
                        subsystem: $sub,
                        name: $name,
                        fields: &fields,
                        message: $crate::__msg!($($($fmt)+)?),
                    };
                    $crate::emit(&ev);
                }
            }
        }
    }};
}

#[doc(hidden)]
#[macro_export]
macro_rules! __msg {
    () => {
        None
    };
    ($($fmt:tt)+) => {
        Some(format_args!($($fmt)+))
    };
}

/// Plain message helpers (event name "msg").
#[macro_export]
macro_rules! info { ($sub:expr, $($fmt:tt)+) => { $crate::event!(Info, $sub, "msg"; $($fmt)+) }; }
#[macro_export]
macro_rules! warn { ($sub:expr, $($fmt:tt)+) => { $crate::event!(Warn, $sub, "msg"; $($fmt)+) }; }
#[macro_export]
macro_rules! error { ($sub:expr, $($fmt:tt)+) => { $crate::event!(Error, $sub, "msg"; $($fmt)+) }; }
#[macro_export]
macro_rules! debug { ($sub:expr, $($fmt:tt)+) => { $crate::event!(Debug, $sub, "msg"; $($fmt)+) }; }


/// The panic/NMI form of `event!` (same syntax); evaluates to true if the record reached the ring.
/// `klog::try_event!(Error, "nmi", "cpu.stalled", cpu = n, rip = klog::Hex(rip));`
#[macro_export]
macro_rules! try_event {
    ($lvl:ident, $sub:expr, $name:expr $(, $k:ident = $v:expr)* $(,)? $(; $($fmt:tt)+)?) => {{
        if $crate::Level::$lvl <= $crate::min_level() {
            match ($(&$v,)*) {
                ($($k,)*) => {
                    let fields = [$($crate::Field { key: stringify!($k), value: $crate::IntoValue::into_value($k) }),*];
                    let ev = $crate::Event {
                        level: $crate::Level::$lvl,
                        subsystem: $sub,
                        name: $name,
                        fields: &fields,
                        message: $crate::__msg!($($($fmt)+)?),
                    };
                    $crate::try_emit(&ev)
                }
            }
        } else {
            false
        }
    }};
}
