//! `struct termios` as the x86-64 Linux kernel keeps it (include/uapi/asm-generic/termbits.h, termbits-common.h,
//! ioctls.h of linux-6.6): the flag bits, the control-character indices, the three user layouts (`termios` 36
//! bytes for TCGETS/TCSETS, `termios2` 44 bytes with the speeds for TCGETS2/TCSETS2, the old `termio` 18 bytes
//! for TCGETA/TCSETA) and the terminal ioctl numbers. Pure data, host-tested.

/// Flag words (octal as in termbits.h).
pub mod flag {
    // c_iflag
    pub const IGNBRK: u32 = 0o1;
    pub const BRKINT: u32 = 0o2;
    pub const IGNPAR: u32 = 0o4;
    pub const PARMRK: u32 = 0o10;
    pub const INPCK: u32 = 0o20;
    pub const ISTRIP: u32 = 0o40;
    pub const INLCR: u32 = 0o100;
    pub const IGNCR: u32 = 0o200;
    pub const ICRNL: u32 = 0o400;
    pub const IUCLC: u32 = 0o1000;
    pub const IXON: u32 = 0o2000;
    pub const IXANY: u32 = 0o4000;
    pub const IXOFF: u32 = 0o10000;
    pub const IMAXBEL: u32 = 0o20000;
    pub const IUTF8: u32 = 0o40000;
    // c_oflag
    pub const OPOST: u32 = 0o1;
    pub const OLCUC: u32 = 0o2;
    pub const ONLCR: u32 = 0o4;
    pub const OCRNL: u32 = 0o10;
    pub const ONOCR: u32 = 0o20;
    pub const ONLRET: u32 = 0o40;
    pub const TABDLY: u32 = 0o14000;
    pub const XTABS: u32 = 0o14000;
    // c_cflag
    pub const CBAUD: u32 = 0o10017;
    pub const B38400: u32 = 0o17;
    pub const CSIZE: u32 = 0o60;
    pub const CS8: u32 = 0o60;
    pub const CREAD: u32 = 0o200;
    pub const PARENB: u32 = 0o400;
    pub const HUPCL: u32 = 0o2000;
    pub const CLOCAL: u32 = 0o4000;
    pub const CBAUDEX: u32 = 0o10000;
    pub const BOTHER: u32 = 0o10000;
    pub const CIBAUD: u32 = 0o2003600000;
    pub const IBSHIFT: u32 = 16;
    // c_lflag
    pub const ISIG: u32 = 0o1;
    pub const ICANON: u32 = 0o2;
    pub const ECHO: u32 = 0o10;
    pub const ECHOE: u32 = 0o20;
    pub const ECHOK: u32 = 0o40;
    pub const ECHONL: u32 = 0o100;
    pub const NOFLSH: u32 = 0o200;
    pub const TOSTOP: u32 = 0o400;
    pub const ECHOCTL: u32 = 0o1000;
    pub const ECHOPRT: u32 = 0o2000;
    pub const ECHOKE: u32 = 0o4000;
    pub const FLUSHO: u32 = 0o10000;
    pub const PENDIN: u32 = 0o40000;
    pub const IEXTEN: u32 = 0o100000;
    pub const EXTPROC: u32 = 0o200000;
}

/// Indices into `c_cc`.
pub mod cc {
    pub const VINTR: usize = 0;
    pub const VQUIT: usize = 1;
    pub const VERASE: usize = 2;
    pub const VKILL: usize = 3;
    pub const VEOF: usize = 4;
    pub const VTIME: usize = 5;
    pub const VMIN: usize = 6;
    pub const VSWTC: usize = 7;
    pub const VSTART: usize = 8;
    pub const VSTOP: usize = 9;
    pub const VSUSP: usize = 10;
    pub const VEOL: usize = 11;
    pub const VREPRINT: usize = 12;
    pub const VDISCARD: usize = 13;
    pub const VWERASE: usize = 14;
    pub const VLNEXT: usize = 15;
    pub const VEOL2: usize = 16;
    /// `NCCS` of the kernel's `struct termios`.
    pub const NCCS: usize = 19;
    /// `NCC` of the old `struct termio`.
    pub const NCC: usize = 8;
}

/// Terminal ioctl numbers (include/uapi/asm-generic/ioctls.h, include/uapi/linux/kd.h).
pub mod ioc {
    pub const TCGETS: u64 = 0x5401;
    pub const TCSETS: u64 = 0x5402;
    pub const TCSETSW: u64 = 0x5403;
    pub const TCSETSF: u64 = 0x5404;
    pub const TCGETA: u64 = 0x5405;
    pub const TCSETA: u64 = 0x5406;
    pub const TCSETAW: u64 = 0x5407;
    pub const TCSETAF: u64 = 0x5408;
    pub const TCSBRK: u64 = 0x5409;
    pub const TCXONC: u64 = 0x540A;
    pub const TCFLSH: u64 = 0x540B;
    pub const TIOCEXCL: u64 = 0x540C;
    pub const TIOCNXCL: u64 = 0x540D;
    pub const TIOCSCTTY: u64 = 0x540E;
    pub const TIOCGPGRP: u64 = 0x540F;
    pub const TIOCSPGRP: u64 = 0x5410;
    pub const TIOCOUTQ: u64 = 0x5411;
    pub const TIOCSTI: u64 = 0x5412;
    pub const TIOCGWINSZ: u64 = 0x5413;
    pub const TIOCSWINSZ: u64 = 0x5414;
    pub const TIOCGSOFTCAR: u64 = 0x5419;
    pub const TIOCSSOFTCAR: u64 = 0x541A;
    pub const TIOCINQ: u64 = 0x541B;
    pub const TIOCNOTTY: u64 = 0x5422;
    pub const TIOCSETD: u64 = 0x5423;
    pub const TIOCGETD: u64 = 0x5424;
    pub const TCSBRKP: u64 = 0x5425;
    pub const TIOCSBRK: u64 = 0x5427;
    pub const TIOCCBRK: u64 = 0x5428;
    pub const TIOCGSID: u64 = 0x5429;
    pub const TCGETS2: u64 = 0x802C_542A;
    pub const TCSETS2: u64 = 0x402C_542B;
    pub const TCSETSW2: u64 = 0x402C_542C;
    pub const TCSETSF2: u64 = 0x402C_542D;
    pub const TIOCGDEV: u64 = 0x8004_5432;
    pub const TIOCGEXCL: u64 = 0x8004_5440;
    pub const TIOCVHANGUP: u64 = 0x5437;
    pub const KDSETMODE: u64 = 0x4B3A;
    pub const KDGETMODE: u64 = 0x4B3B;
    pub const KDGKBTYPE: u64 = 0x4B33;
    pub const KDGKBMODE: u64 = 0x4B44;
    pub const KDSKBMODE: u64 = 0x4B45;
    /// TCFLSH / TCXONC arguments.
    pub const TCIFLUSH: u64 = 0;
    pub const TCOFLUSH: u64 = 1;
    pub const TCIOFLUSH: u64 = 2;
    pub const TCOOFF: u64 = 0;
    pub const TCOON: u64 = 1;
    pub const TCIOFF: u64 = 2;
    pub const TCION: u64 = 3;
    /// KDSETMODE / KDSKBMODE values.
    pub const KD_TEXT: u64 = 0;
    pub const KD_GRAPHICS: u64 = 1;
    pub const KD_TEXT0: u64 = 2;
    pub const KD_TEXT1: u64 = 3;
    pub const K_RAW: u64 = 0;
    pub const K_XLATE: u64 = 1;
    pub const K_MEDIUMRAW: u64 = 2;
    pub const K_UNICODE: u64 = 3;
    pub const K_OFF: u64 = 4;
    /// `KDGKBTYPE`'s answer (Linux says KB_101 for every keyboard).
    pub const KB_101: u8 = 2;
    /// The pty master's own calls (IN-7; include/uapi/asm-generic/ioctls.h).
    pub const TIOCPKT: u64 = 0x5420;
    pub const TIOCGPTN: u64 = 0x8004_5430;
    pub const TIOCSPTLCK: u64 = 0x4004_5431;
    pub const TIOCSIG: u64 = 0x4004_5436;
    pub const TIOCGPKT: u64 = 0x8004_5438;
    pub const TIOCGPTLCK: u64 = 0x8004_5439;
    pub const TIOCGPTPEER: u64 = 0x5441;
}

use flag::*;

/// `struct ktermios`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Termios {
    pub iflag: u32,
    pub oflag: u32,
    pub cflag: u32,
    pub lflag: u32,
    pub line: u8,
    pub cc: [u8; cc::NCCS],
    pub ispeed: u32,
    pub ospeed: u32,
}

/// Size of `struct termios` (TCGETS), `struct termios2` (TCGETS2), `struct termio` (TCGETA).
pub const TERMIOS_SIZE: usize = 36;
pub const TERMIOS2_SIZE: usize = 44;
pub const TERMIO_SIZE: usize = 18;

/// `INIT_C_CC` (include/linux/termios_internal.h:21-35): ^C ^\ DEL ^U ^D, TIME 0, MIN 1, ^Q ^S ^Z, ^R ^O ^W ^V.
pub const INIT_C_CC: [u8; cc::NCCS] = [3, 0x1C, 0x7F, 0x15, 4, 0, 1, 0, 0x11, 0x13, 0x1A, 0, 0x12, 0x0F, 0x17, 0x16, 0, 0, 0];

fn u32_at(b: &[u8], i: usize) -> u32 {
    u32::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]])
}
fn u16_at(b: &[u8], i: usize) -> u16 {
    u16::from_le_bytes([b[i], b[i + 1]])
}

/// The speed a `CBAUD` code stands for (`baud_table`, drivers/tty/tty_baudrate.c); BOTHER and unknown codes 0.
pub fn baud_of(code: u32) -> u32 {
    const LOW: [u32; 16] = [0, 50, 75, 110, 134, 150, 200, 300, 600, 1200, 1800, 2400, 4800, 9600, 19200, 38400];
    const HIGH: [u32; 16] =
        [0, 57600, 115200, 230400, 460800, 500000, 576000, 921600, 1000000, 1152000, 1500000, 2000000, 2500000, 3000000, 3500000, 4000000];
    if code & CBAUDEX == 0 {
        LOW[(code & 0xF) as usize]
    } else if code & 0xF == 0 {
        0 // BOTHER: the speed fields hold it
    } else {
        HIGH[(code & 0xF) as usize]
    }
}

impl Termios {
    /// `tty_std_termios` (drivers/tty/tty_io.c): what a fresh Linux VT reports.
    pub const fn std() -> Termios {
        Termios {
            iflag: ICRNL | IXON,
            oflag: OPOST | ONLCR,
            cflag: B38400 | CS8 | CREAD | HUPCL,
            lflag: ISIG | ICANON | ECHO | ECHOE | ECHOK | ECHOCTL | ECHOKE | IEXTEN,
            line: 0,
            cc: INIT_C_CC,
            ispeed: 38400,
            ospeed: 38400,
        }
    }
    /// The Linux VT's start settings: `tty_std_termios` plus IUTF8 (`default_utf8`, linux-6.6/drivers/tty/vt/
    /// vt.c:3554-3556), what TCGETS answers on a fresh console.
    pub const fn vt() -> Termios {
        let mut t = Termios::std();
        t.iflag |= IUTF8;
        t
    }
    /// A pty slave's start settings (IN-7): `tty_std_termios` with `c_cflag = B38400 | CS8 | CREAD` (no HUPCL,
    /// no IUTF8; linux-6.6/drivers/tty/pty.c:914-917).
    pub const fn pty() -> Termios {
        let mut t = Termios::std();
        t.cflag = B38400 | CS8 | CREAD;
        t
    }
    pub fn to_termios(&self) -> [u8; TERMIOS_SIZE] {
        let mut b = [0u8; TERMIOS_SIZE];
        for (i, f) in [self.iflag, self.oflag, self.cflag, self.lflag].iter().enumerate() {
            b[i * 4..i * 4 + 4].copy_from_slice(&f.to_le_bytes());
        }
        b[16] = self.line;
        b[17..36].copy_from_slice(&self.cc);
        b
    }
    pub fn to_termios2(&self) -> [u8; TERMIOS2_SIZE] {
        let mut b = [0u8; TERMIOS2_SIZE];
        b[..TERMIOS_SIZE].copy_from_slice(&self.to_termios());
        b[36..40].copy_from_slice(&self.ispeed.to_le_bytes());
        b[40..44].copy_from_slice(&self.ospeed.to_le_bytes());
        b
    }
    /// `struct termio`: the low 16 bits of each flag word and the first 8 control characters
    /// (`kernel_termios_to_user_termio`).
    pub fn to_termio(&self) -> [u8; TERMIO_SIZE] {
        let mut b = [0u8; TERMIO_SIZE];
        for (i, f) in [self.iflag, self.oflag, self.cflag, self.lflag].iter().enumerate() {
            b[i * 2..i * 2 + 2].copy_from_slice(&(*f as u16).to_le_bytes());
        }
        b[8] = self.line;
        b[9..17].copy_from_slice(&self.cc[..cc::NCC]);
        b
    }
    /// A `struct termios` from a program onto `self` (`user_termios_to_kernel_termios_1`: the speeds stay).
    pub fn set_from_termios(&mut self, b: &[u8; TERMIOS_SIZE]) {
        self.iflag = u32_at(b, 0);
        self.oflag = u32_at(b, 4);
        self.cflag = u32_at(b, 8);
        self.lflag = u32_at(b, 12);
        self.line = b[16];
        self.cc.copy_from_slice(&b[17..36]);
    }
    pub fn set_from_termios2(&mut self, b: &[u8; TERMIOS2_SIZE]) {
        let mut t = [0u8; TERMIOS_SIZE];
        t.copy_from_slice(&b[..TERMIOS_SIZE]);
        self.set_from_termios(&t);
        self.ispeed = u32_at(b, 36);
        self.ospeed = u32_at(b, 40);
    }
    /// `user_termio_to_kernel_termio` (drivers/tty/tty_ioctl.c): only the low 16 bits of each flag word change.
    pub fn set_from_termio(&mut self, b: &[u8; TERMIO_SIZE]) {
        let low = |old: u32, i: usize| (old & 0xFFFF_0000) | u16_at(b, i) as u32;
        self.iflag = low(self.iflag, 0);
        self.oflag = low(self.oflag, 2);
        self.cflag = low(self.cflag, 4);
        self.lflag = low(self.lflag, 6);
        self.line = b[8];
        self.cc[..cc::NCC].copy_from_slice(&b[9..17]);
    }
    /// `set_termios`'s speed step: the old-style `Bfoo` codes load the speed fields
    /// (`tty_termios_input_baud_rate`/`tty_termios_baud_rate`); BOTHER keeps the given speeds.
    pub fn load_speeds(&mut self) {
        let out = self.cflag & CBAUD;
        if out != BOTHER {
            self.ospeed = baud_of(out);
        }
        let inp = (self.cflag & CIBAUD) >> IBSHIFT;
        if inp == 0 {
            self.ispeed = self.ospeed;
        } else if inp != BOTHER {
            self.ispeed = baud_of(inp);
        }
    }
    /// A VT has no `set_termios` of its own: `tty_termios_copy_hw` keeps the old hardware bits and speeds, only
    /// HUPCL, CREAD and CLOCAL come from the new settings.
    pub fn copy_hw(&mut self, old: &Termios) {
        self.cflag &= HUPCL | CREAD | CLOCAL;
        self.cflag |= old.cflag & !(HUPCL | CREAD | CLOCAL);
        self.ispeed = old.ispeed;
        self.ospeed = old.ospeed;
    }

    pub fn l(&self, f: u32) -> bool {
        self.lflag & f != 0
    }
    pub fn i(&self, f: u32) -> bool {
        self.iflag & f != 0
    }
    pub fn o(&self, f: u32) -> bool {
        self.oflag & f != 0
    }
    pub fn c(&self, i: usize) -> u8 {
        self.cc[i]
    }
}

impl Default for Termios {
    fn default() -> Self {
        Termios::std()
    }
}

/// `struct winsize`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Winsize {
    pub rows: u16,
    pub cols: u16,
    pub xpixel: u16,
    pub ypixel: u16,
}

impl Winsize {
    pub fn to_bytes(&self) -> [u8; 8] {
        let mut b = [0u8; 8];
        for (i, v) in [self.rows, self.cols, self.xpixel, self.ypixel].iter().enumerate() {
            b[i * 2..i * 2 + 2].copy_from_slice(&v.to_le_bytes());
        }
        b
    }
    pub fn from_bytes(b: &[u8; 8]) -> Winsize {
        Winsize { rows: u16_at(b, 0), cols: u16_at(b, 2), xpixel: u16_at(b, 4), ypixel: u16_at(b, 6) }
    }
}
