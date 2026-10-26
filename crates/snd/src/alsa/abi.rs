//! The ALSA user ABI for x86-64 (`linux-6.6/include/uapi/sound/asound.h`): ioctl numbers, struct sizes and field
//! offsets, little-endian accessors. Structs are handled as byte arrays (no unsafe, no layout assumptions); the
//! offsets below are the 64-bit layout (`snd_pcm_uframes_t` = 8 bytes, 64-bit `time_t`), checked by host tests.

pub const SNDRV_PCM_VERSION: u32 = (2 << 16) | 15; // asound.h:145, 2.0.15
pub const SNDRV_CTL_VERSION: u32 = (2 << 16) | 9; // asound.h:1038, 2.0.9

/// `_IOC(dir, type, nr, size)` (asm-generic/ioctl.h): dir 1 = write (to the kernel), 2 = read.
const fn ioc(dir: u64, ty: u8, nr: u8, size: usize) -> u64 {
    (dir << 30) | ((size as u64) << 16) | ((ty as u64) << 8) | nr as u64
}
const fn io(ty: u8, nr: u8) -> u64 {
    ioc(0, ty, nr, 0)
}
const fn ior(ty: u8, nr: u8, size: usize) -> u64 {
    ioc(2, ty, nr, size)
}
const fn iow(ty: u8, nr: u8, size: usize) -> u64 {
    ioc(1, ty, nr, size)
}
const fn iowr(ty: u8, nr: u8, size: usize) -> u64 {
    ioc(3, ty, nr, size)
}

pub const PCM_INFO_SIZE: usize = 288;
pub const HW_PARAMS_SIZE: usize = 608;
pub const SW_PARAMS_SIZE: usize = 136;
pub const STATUS_SIZE: usize = 152;
pub const SYNC_PTR_SIZE: usize = 136;
pub const XFERI_SIZE: usize = 24;
pub const CHANNEL_INFO_SIZE: usize = 24;

pub const PCM_PVERSION: u64 = ior(b'A', 0x00, 4);
pub const PCM_INFO: u64 = ior(b'A', 0x01, PCM_INFO_SIZE);
pub const PCM_TSTAMP: u64 = iow(b'A', 0x02, 4);
pub const PCM_TTSTAMP: u64 = iow(b'A', 0x03, 4);
pub const PCM_USER_PVERSION: u64 = iow(b'A', 0x04, 4);
pub const PCM_HW_REFINE: u64 = iowr(b'A', 0x10, HW_PARAMS_SIZE);
pub const PCM_HW_PARAMS: u64 = iowr(b'A', 0x11, HW_PARAMS_SIZE);
pub const PCM_HW_FREE: u64 = io(b'A', 0x12);
pub const PCM_SW_PARAMS: u64 = iowr(b'A', 0x13, SW_PARAMS_SIZE);
pub const PCM_STATUS: u64 = ior(b'A', 0x20, STATUS_SIZE);
pub const PCM_DELAY: u64 = ior(b'A', 0x21, 8);
pub const PCM_HWSYNC: u64 = io(b'A', 0x22);
pub const PCM_SYNC_PTR: u64 = iowr(b'A', 0x23, SYNC_PTR_SIZE);
pub const PCM_STATUS_EXT: u64 = iowr(b'A', 0x24, STATUS_SIZE);
pub const PCM_CHANNEL_INFO: u64 = ior(b'A', 0x32, CHANNEL_INFO_SIZE);
pub const PCM_PREPARE: u64 = io(b'A', 0x40);
pub const PCM_RESET: u64 = io(b'A', 0x41);
pub const PCM_START: u64 = io(b'A', 0x42);
pub const PCM_DROP: u64 = io(b'A', 0x43);
pub const PCM_DRAIN: u64 = io(b'A', 0x44);
pub const PCM_PAUSE: u64 = iow(b'A', 0x45, 4);
pub const PCM_REWIND: u64 = iow(b'A', 0x46, 8);
pub const PCM_RESUME: u64 = io(b'A', 0x47);
pub const PCM_XRUN: u64 = io(b'A', 0x48);
pub const PCM_FORWARD: u64 = iow(b'A', 0x49, 8);
pub const PCM_WRITEI_FRAMES: u64 = iow(b'A', 0x50, XFERI_SIZE);
pub const PCM_READI_FRAMES: u64 = ior(b'A', 0x51, XFERI_SIZE);
pub const PCM_WRITEN_FRAMES: u64 = iow(b'A', 0x52, XFERI_SIZE);
pub const PCM_READN_FRAMES: u64 = ior(b'A', 0x53, XFERI_SIZE);
pub const PCM_LINK: u64 = iow(b'A', 0x60, 4);
pub const PCM_UNLINK: u64 = io(b'A', 0x61);

pub const CARD_INFO_SIZE: usize = 376;
pub const ELEM_ID_SIZE: usize = 64;
pub const ELEM_LIST_SIZE: usize = 80;
pub const ELEM_INFO_SIZE: usize = 272;
/// id 64, indirect 4 (+4), value union 1024 (`long value[128]`), reserved 128.
pub const ELEM_VALUE_SIZE: usize = 1224;
pub const CTL_EVENT_SIZE: usize = 72;

pub const CTL_PVERSION: u64 = ior(b'U', 0x00, 4);
pub const CTL_CARD_INFO: u64 = ior(b'U', 0x01, CARD_INFO_SIZE);
pub const CTL_ELEM_LIST: u64 = iowr(b'U', 0x10, ELEM_LIST_SIZE);
pub const CTL_ELEM_INFO: u64 = iowr(b'U', 0x11, ELEM_INFO_SIZE);
pub const CTL_ELEM_READ: u64 = iowr(b'U', 0x12, ELEM_VALUE_SIZE);
pub const CTL_ELEM_WRITE: u64 = iowr(b'U', 0x13, ELEM_VALUE_SIZE);
pub const CTL_SUBSCRIBE_EVENTS: u64 = iowr(b'U', 0x16, 4);
pub const CTL_TLV_READ: u64 = iowr(b'U', 0x1a, 8);
pub const CTL_HWDEP_NEXT_DEVICE: u64 = iowr(b'U', 0x20, 4);
pub const CTL_PCM_NEXT_DEVICE: u64 = ior(b'U', 0x30, 4);
pub const CTL_PCM_INFO: u64 = iowr(b'U', 0x31, PCM_INFO_SIZE);
pub const CTL_PCM_PREFER_SUBDEVICE: u64 = iow(b'U', 0x32, 4);
pub const CTL_RAWMIDI_NEXT_DEVICE: u64 = iowr(b'U', 0x40, 4);
pub const CTL_POWER: u64 = iowr(b'U', 0xd0, 4);
pub const CTL_POWER_STATE: u64 = ior(b'U', 0xd1, 4);

// PCM states (asound.h:304).
pub const STATE_OPEN: u32 = 0;
pub const STATE_SETUP: u32 = 1;
pub const STATE_PREPARED: u32 = 2;
pub const STATE_RUNNING: u32 = 3;
pub const STATE_XRUN: u32 = 4;
pub const STATE_DRAINING: u32 = 5;
pub const STATE_PAUSED: u32 = 6;
pub const STATE_DISCONNECTED: u32 = 8;

// hw_params info bits (asound.h:272).
pub const INFO_INTERLEAVED: u32 = 0x100;
pub const INFO_BATCH: u32 = 0x10;
pub const INFO_BLOCK_TRANSFER: u32 = 0x1_0000;
pub const INFO_PAUSE: u32 = 0x8_0000;
pub const INFO_RESUME: u32 = 0x4_0000;
pub const INFO_NO_REWINDS: u32 = 0x2000_0000;
pub const INFO_MMAP: u32 = 0x1;
pub const INFO_MMAP_VALID: u32 = 0x2;
pub const INFO_DRAIN_TRIGGER: u32 = 0x4000_0000;
pub const INFO_FIFO_IN_FRAMES: u32 = 0x8000_0000;

pub const ACCESS_RW_INTERLEAVED: u32 = 3;
pub const FORMAT_S16_LE: u32 = 2;
pub const FORMAT_S32_LE: u32 = 10;
pub const FORMAT_FLOAT_LE: u32 = 14;
pub const SUBFORMAT_STD: u32 = 0;

pub const SYNC_PTR_HWSYNC: u32 = 1;
pub const SYNC_PTR_APPL: u32 = 2;
pub const SYNC_PTR_AVAIL_MIN: u32 = 4;

pub const TSTAMP_ENABLE: i32 = 1;
pub const TSTAMP_TYPE_GETTIMEOFDAY: u32 = 0;
pub const TSTAMP_TYPE_LAST: u32 = 2;

/// asm-generic/errno.h: not in `uapi::errno`.
pub const EBADFD: i64 = 77;
pub const ESTRPIPE: i64 = 86;

pub fn get_u32(b: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]])
}
pub fn get_i32(b: &[u8], off: usize) -> i32 {
    get_u32(b, off) as i32
}
pub fn get_u64(b: &[u8], off: usize) -> u64 {
    let mut a = [0u8; 8];
    a.copy_from_slice(&b[off..off + 8]);
    u64::from_le_bytes(a)
}
pub fn put_u32(b: &mut [u8], off: usize, v: u32) {
    b[off..off + 4].copy_from_slice(&v.to_le_bytes());
}
pub fn put_u64(b: &mut [u8], off: usize, v: u64) {
    b[off..off + 8].copy_from_slice(&v.to_le_bytes());
}
/// A timespec (two 64-bit words) from nanoseconds.
pub fn put_ts(b: &mut [u8], off: usize, ns: u64) {
    put_u64(b, off, ns / 1_000_000_000);
    put_u64(b, off + 8, ns % 1_000_000_000);
}
/// `strscpy` into a fixed field: at most len-1 bytes and a NUL; the rest stays as it is (zeroed by callers).
pub fn put_str(b: &mut [u8], off: usize, len: usize, s: &str) {
    let n = s.len().min(len - 1);
    b[off..off + n].copy_from_slice(&s.as_bytes()[..n]);
    b[off + n] = 0;
}

/// `struct snd_pcm_info` (asound.h:336): device 0, subdevice 4, stream 8, card 12, id[64] 16, name[80] 80,
/// subname[32] 160, dev_class 192, dev_subclass 196, subdevices_count 200, subdevices_avail 204, sync 208,
/// reserved 224.
pub mod pcm_info {
    pub const DEVICE: usize = 0;
    pub const SUBDEVICE: usize = 4;
    pub const STREAM: usize = 8;
    pub const CARD: usize = 12;
    pub const ID: usize = 16;
    pub const NAME: usize = 80;
    pub const SUBNAME: usize = 160;
    pub const COUNT: usize = 200;
    pub const AVAIL: usize = 204;
}

/// `struct snd_pcm_sw_params` (asound.h:448).
pub mod sw {
    pub const TSTAMP_MODE: usize = 0;
    pub const PERIOD_STEP: usize = 4;
    pub const AVAIL_MIN: usize = 16;
    pub const START_THRESHOLD: usize = 32;
    pub const STOP_THRESHOLD: usize = 40;
    pub const SILENCE_THRESHOLD: usize = 48;
    pub const SILENCE_SIZE: usize = 56;
    pub const BOUNDARY: usize = 64;
    pub const PROTO: usize = 72;
    pub const TSTAMP_TYPE: usize = 76;
}

/// `struct snd_pcm_status` / kernel `snd_pcm_status64` on x86-64 (asound.h:500).
pub mod status {
    pub const STATE: usize = 0;
    pub const TRIGGER_TSTAMP: usize = 8;
    pub const TSTAMP: usize = 24;
    pub const APPL_PTR: usize = 40;
    pub const HW_PTR: usize = 48;
    pub const DELAY: usize = 56;
    pub const AVAIL: usize = 64;
    pub const AVAIL_MAX: usize = 72;
    pub const OVERRANGE: usize = 80;
    pub const SUSPENDED_STATE: usize = 88;
    pub const AUDIO_TSTAMP_DATA: usize = 92;
    pub const AUDIO_TSTAMP: usize = 96;
    pub const DRIVER_TSTAMP: usize = 112;
}

/// `struct snd_pcm_sync_ptr` (64-bit = `__snd_pcm_sync_ptr64`, asound.h:617): flags 0; status union at 8
/// (state 8, hw_ptr 16, tstamp 24, suspended_state 40, audio_tstamp 48); control union at 72 (appl_ptr 72,
/// avail_min 80).
pub mod sync {
    pub const FLAGS: usize = 0;
    pub const STATE: usize = 8;
    pub const HW_PTR: usize = 16;
    pub const TSTAMP: usize = 24;
    pub const SUSPENDED_STATE: usize = 40;
    pub const AUDIO_TSTAMP: usize = 48;
    pub const APPL_PTR: usize = 72;
    pub const AVAIL_MIN: usize = 80;
}

/// `struct snd_ctl_card_info` (asound.h:1040).
pub mod card {
    pub const CARD: usize = 0;
    pub const ID: usize = 8;
    pub const DRIVER: usize = 24;
    pub const NAME: usize = 40;
    pub const LONGNAME: usize = 72;
    pub const MIXERNAME: usize = 168;
    pub const COMPONENTS: usize = 248;
}

/// `struct snd_ctl_elem_id` (64 bytes), `snd_ctl_elem_list`, `snd_ctl_elem_info`, `snd_ctl_elem_value`.
pub mod elem {
    pub const NUMID: usize = 0;
    pub const IFACE: usize = 4;
    pub const DEVICE: usize = 8;
    pub const SUBDEVICE: usize = 12;
    pub const NAME: usize = 16;
    pub const NAME_LEN: usize = 44;
    pub const INDEX: usize = 60;
    // snd_ctl_elem_list
    pub const LIST_OFFSET: usize = 0;
    pub const LIST_SPACE: usize = 4;
    pub const LIST_USED: usize = 8;
    pub const LIST_COUNT: usize = 12;
    pub const LIST_PIDS: usize = 16;
    // snd_ctl_elem_info (after the id)
    pub const INFO_TYPE: usize = 64;
    pub const INFO_ACCESS: usize = 68;
    pub const INFO_COUNT: usize = 72;
    pub const INFO_OWNER: usize = 76;
    pub const INFO_MIN: usize = 80;
    pub const INFO_MAX: usize = 88;
    pub const INFO_STEP: usize = 96;
    // snd_ctl_elem_value: integer.value[] (long) at 72
    pub const VALUE: usize = 72;
}
