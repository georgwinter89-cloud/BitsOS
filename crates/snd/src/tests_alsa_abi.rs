//! abi.rs against the real <sound/asound.h>: host/tests/sn/abi-layout.c prints sizes, offsets and ioctl
//! numbers on the build host (it opens no device); every line must equal the hand-written constant.

extern crate std;
use crate::alsa::abi::*;
use std::process::Command;
use std::string::String;
use std::vec::Vec;

fn table() -> Vec<(String, u64)> {
    let sizes: &[(&str, u64)] = &[
        ("sizeof snd_pcm_info", PCM_INFO_SIZE as u64),
        ("sizeof snd_pcm_hw_params", HW_PARAMS_SIZE as u64),
        ("sizeof snd_pcm_sw_params", SW_PARAMS_SIZE as u64),
        ("sizeof snd_pcm_status", STATUS_SIZE as u64),
        ("sizeof snd_pcm_sync_ptr", SYNC_PTR_SIZE as u64),
        ("sizeof snd_xferi", XFERI_SIZE as u64),
        ("sizeof snd_pcm_channel_info", CHANNEL_INFO_SIZE as u64),
        ("sizeof snd_ctl_card_info", CARD_INFO_SIZE as u64),
        ("sizeof snd_ctl_elem_id", ELEM_ID_SIZE as u64),
        ("sizeof snd_ctl_elem_list", ELEM_LIST_SIZE as u64),
        ("sizeof snd_ctl_elem_info", ELEM_INFO_SIZE as u64),
        ("sizeof snd_ctl_elem_value", ELEM_VALUE_SIZE as u64),
        ("sizeof snd_ctl_event", CTL_EVENT_SIZE as u64),
    ];
    let mut t: Vec<(String, u64)> = sizes.iter().map(|(n, v)| ((*n).into(), *v)).collect();
    let off: &[(&str, usize)] = &[
        ("snd_pcm_info.device", pcm_info::DEVICE),
        ("snd_pcm_info.subdevice", pcm_info::SUBDEVICE),
        ("snd_pcm_info.stream", pcm_info::STREAM),
        ("snd_pcm_info.card", pcm_info::CARD),
        ("snd_pcm_info.id", pcm_info::ID),
        ("snd_pcm_info.name", pcm_info::NAME),
        ("snd_pcm_info.subname", pcm_info::SUBNAME),
        ("snd_pcm_info.subdevices_count", pcm_info::COUNT),
        ("snd_pcm_info.subdevices_avail", pcm_info::AVAIL),
        ("snd_pcm_sw_params.tstamp_mode", sw::TSTAMP_MODE),
        ("snd_pcm_sw_params.period_step", sw::PERIOD_STEP),
        ("snd_pcm_sw_params.avail_min", sw::AVAIL_MIN),
        ("snd_pcm_sw_params.start_threshold", sw::START_THRESHOLD),
        ("snd_pcm_sw_params.stop_threshold", sw::STOP_THRESHOLD),
        ("snd_pcm_sw_params.silence_threshold", sw::SILENCE_THRESHOLD),
        ("snd_pcm_sw_params.silence_size", sw::SILENCE_SIZE),
        ("snd_pcm_sw_params.boundary", sw::BOUNDARY),
        ("snd_pcm_sw_params.proto", sw::PROTO),
        ("snd_pcm_sw_params.tstamp_type", sw::TSTAMP_TYPE),
        ("snd_pcm_status.state", status::STATE),
        ("snd_pcm_status.trigger_tstamp", status::TRIGGER_TSTAMP),
        ("snd_pcm_status.tstamp", status::TSTAMP),
        ("snd_pcm_status.appl_ptr", status::APPL_PTR),
        ("snd_pcm_status.hw_ptr", status::HW_PTR),
        ("snd_pcm_status.delay", status::DELAY),
        ("snd_pcm_status.avail", status::AVAIL),
        ("snd_pcm_status.avail_max", status::AVAIL_MAX),
        ("snd_pcm_status.overrange", status::OVERRANGE),
        ("snd_pcm_status.suspended_state", status::SUSPENDED_STATE),
        ("snd_pcm_status.audio_tstamp_data", status::AUDIO_TSTAMP_DATA),
        ("snd_pcm_status.audio_tstamp", status::AUDIO_TSTAMP),
        ("snd_pcm_status.driver_tstamp", status::DRIVER_TSTAMP),
        ("snd_pcm_sync_ptr.flags", sync::FLAGS),
        ("snd_pcm_sync_ptr.s.status.state", sync::STATE),
        ("snd_pcm_sync_ptr.s.status.hw_ptr", sync::HW_PTR),
        ("snd_pcm_sync_ptr.s.status.tstamp", sync::TSTAMP),
        ("snd_pcm_sync_ptr.s.status.suspended_state", sync::SUSPENDED_STATE),
        ("snd_pcm_sync_ptr.s.status.audio_tstamp", sync::AUDIO_TSTAMP),
        ("snd_pcm_sync_ptr.c.control.appl_ptr", sync::APPL_PTR),
        ("snd_pcm_sync_ptr.c.control.avail_min", sync::AVAIL_MIN),
        ("snd_ctl_card_info.card", card::CARD),
        ("snd_ctl_card_info.id", card::ID),
        ("snd_ctl_card_info.driver", card::DRIVER),
        ("snd_ctl_card_info.name", card::NAME),
        ("snd_ctl_card_info.longname", card::LONGNAME),
        ("snd_ctl_card_info.mixername", card::MIXERNAME),
        ("snd_ctl_card_info.components", card::COMPONENTS),
        ("snd_ctl_elem_id.numid", elem::NUMID),
        ("snd_ctl_elem_id.iface", elem::IFACE),
        ("snd_ctl_elem_id.device", elem::DEVICE),
        ("snd_ctl_elem_id.subdevice", elem::SUBDEVICE),
        ("snd_ctl_elem_id.name", elem::NAME),
        ("snd_ctl_elem_id.index", elem::INDEX),
        ("snd_ctl_elem_list.offset", elem::LIST_OFFSET),
        ("snd_ctl_elem_list.space", elem::LIST_SPACE),
        ("snd_ctl_elem_list.used", elem::LIST_USED),
        ("snd_ctl_elem_list.count", elem::LIST_COUNT),
        ("snd_ctl_elem_list.pids", elem::LIST_PIDS),
        ("snd_ctl_elem_info.type", elem::INFO_TYPE),
        ("snd_ctl_elem_info.access", elem::INFO_ACCESS),
        ("snd_ctl_elem_info.count", elem::INFO_COUNT),
        ("snd_ctl_elem_info.owner", elem::INFO_OWNER),
        ("snd_ctl_elem_info.value.integer.min", elem::INFO_MIN),
        ("snd_ctl_elem_info.value.integer.max", elem::INFO_MAX),
        ("snd_ctl_elem_info.value.integer.step", elem::INFO_STEP),
        ("snd_ctl_elem_value.value.integer.value", elem::VALUE),
    ];
    t.extend(off.iter().map(|(n, v)| (alloc::format!("offsetof {n}"), *v as u64)));
    let ioctls: &[(&str, u64)] = &[
        ("ioctl SNDRV_PCM_IOCTL_INFO", PCM_INFO),
        ("ioctl SNDRV_PCM_IOCTL_HW_REFINE", PCM_HW_REFINE),
        ("ioctl SNDRV_PCM_IOCTL_HW_PARAMS", PCM_HW_PARAMS),
        ("ioctl SNDRV_PCM_IOCTL_SW_PARAMS", PCM_SW_PARAMS),
        ("ioctl SNDRV_PCM_IOCTL_STATUS", PCM_STATUS),
        ("ioctl SNDRV_PCM_IOCTL_STATUS_EXT", PCM_STATUS_EXT),
        ("ioctl SNDRV_PCM_IOCTL_SYNC_PTR", PCM_SYNC_PTR),
        ("ioctl SNDRV_PCM_IOCTL_CHANNEL_INFO", PCM_CHANNEL_INFO),
        ("ioctl SNDRV_PCM_IOCTL_WRITEI_FRAMES", PCM_WRITEI_FRAMES),
        // SN-9: the capture side.
        ("ioctl SNDRV_PCM_IOCTL_READI_FRAMES", PCM_READI_FRAMES),
        ("ioctl SNDRV_PCM_IOCTL_READN_FRAMES", PCM_READN_FRAMES),
        ("ioctl SNDRV_PCM_IOCTL_FORWARD", PCM_FORWARD),
        ("ioctl SNDRV_PCM_IOCTL_DRAIN", PCM_DRAIN),
        ("ioctl SNDRV_PCM_IOCTL_DROP", PCM_DROP),
        ("ioctl SNDRV_CTL_IOCTL_CARD_INFO", CTL_CARD_INFO),
        ("ioctl SNDRV_CTL_IOCTL_ELEM_LIST", CTL_ELEM_LIST),
        ("ioctl SNDRV_CTL_IOCTL_ELEM_INFO", CTL_ELEM_INFO),
        ("ioctl SNDRV_CTL_IOCTL_ELEM_READ", CTL_ELEM_READ),
        ("ioctl SNDRV_CTL_IOCTL_ELEM_WRITE", CTL_ELEM_WRITE),
        ("ioctl SNDRV_CTL_IOCTL_PCM_INFO", CTL_PCM_INFO),
    ];
    t.extend(ioctls.iter().map(|(n, v)| ((*n).into(), *v)));
    t
}

#[test]
fn layout_matches_asound_h() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let src = root.join("host/tests/sn/abi-layout.c");
    if !std::path::Path::new("/usr/include/sound/asound.h").exists() || !src.exists() {
        std::eprintln!("skip: no <sound/asound.h> or abi-layout.c on this host");
        return;
    }
    let exe = std::env::temp_dir().join(alloc::format!("snd-abi-layout-{}", std::process::id()));
    match Command::new("cc").arg("-O0").arg("-o").arg(&exe).arg(&src).status() {
        Ok(s) if s.success() => {}
        r => {
            std::eprintln!("skip: cc unavailable or failed: {r:?}");
            return;
        }
    }
    let out = Command::new(&exe).output().expect("run abi-layout");
    let _ = std::fs::remove_file(&exe);
    let text = String::from_utf8(out.stdout).unwrap();
    let got: Vec<(String, u64)> = text.lines().filter_map(|l| l.rsplit_once(' ').map(|(k, v)| (k.into(), v.parse().unwrap()))).collect();
    let want = table();
    assert_eq!(got.len(), want.len(), "lines: C printed {}, abi.rs has {}", got.len(), want.len());
    for (k, v) in &want {
        let c = got.iter().find(|(n, _)| n == k).unwrap_or_else(|| panic!("{k} not printed by abi-layout.c"));
        assert_eq!(c.1, *v, "{k}: <sound/asound.h> {} != abi.rs {}", c.1, v);
    }
}
