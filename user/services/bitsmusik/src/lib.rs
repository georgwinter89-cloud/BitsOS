//! bitsmusik (GM-6m, spec/sofa-mode.md 3.5): the music player of user 1000. Scans `/data/home/1000/Musik` and
//! `\BITSOS2\MUSIK\`, reads tags and `*.m3u` lists, plays through the kernel's ALSA PCM (hw:0) and answers JSON
//! lines on `/run/user/1000/bitsmusik.sock`. No volume of its own: that is the system's (spec section 7).

pub mod decode;
pub mod events;
pub mod last;
pub mod m3u;
pub mod pcm;
pub mod player;
pub mod proto;
pub mod scan;
pub mod server;
pub mod sink;
pub mod tags;
