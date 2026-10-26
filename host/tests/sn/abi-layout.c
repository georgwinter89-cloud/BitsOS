/* Sizes, offsets and ioctl numbers of the ALSA structs SN-4 encodes by hand (crates/snd/src/alsa/abi.rs),
 * printed from the real <sound/asound.h> of the build host. Opens no device: the host test
 * tests_alsa::layout_matches_asound_h compiles and runs it and compares every line with abi.rs. */
#include <stddef.h>
#include <stdio.h>
#include <time.h>
#include <sys/ioctl.h>
#include <sound/asound.h>

#define S(t) printf("sizeof %s %zu\n", #t, sizeof(struct t))
#define O(t, f) printf("offsetof %s.%s %zu\n", #t, #f, offsetof(struct t, f))
#define I(n) printf("ioctl %s %lu\n", #n, (unsigned long)(n))

int main(void) {
    S(snd_pcm_info); S(snd_pcm_hw_params); S(snd_pcm_sw_params); S(snd_pcm_status); S(snd_pcm_sync_ptr);
    S(snd_xferi); S(snd_pcm_channel_info); S(snd_ctl_card_info); S(snd_ctl_elem_id); S(snd_ctl_elem_list);
    S(snd_ctl_elem_info); S(snd_ctl_elem_value); S(snd_ctl_event);
    O(snd_pcm_info, device); O(snd_pcm_info, subdevice); O(snd_pcm_info, stream); O(snd_pcm_info, card);
    O(snd_pcm_info, id); O(snd_pcm_info, name); O(snd_pcm_info, subname); O(snd_pcm_info, subdevices_count);
    O(snd_pcm_info, subdevices_avail);
    O(snd_pcm_sw_params, tstamp_mode); O(snd_pcm_sw_params, period_step); O(snd_pcm_sw_params, avail_min);
    O(snd_pcm_sw_params, start_threshold); O(snd_pcm_sw_params, stop_threshold);
    O(snd_pcm_sw_params, silence_threshold); O(snd_pcm_sw_params, silence_size); O(snd_pcm_sw_params, boundary);
    O(snd_pcm_sw_params, proto); O(snd_pcm_sw_params, tstamp_type);
    O(snd_pcm_status, state); O(snd_pcm_status, trigger_tstamp); O(snd_pcm_status, tstamp);
    O(snd_pcm_status, appl_ptr); O(snd_pcm_status, hw_ptr); O(snd_pcm_status, delay); O(snd_pcm_status, avail);
    O(snd_pcm_status, avail_max); O(snd_pcm_status, overrange); O(snd_pcm_status, suspended_state);
    O(snd_pcm_status, audio_tstamp_data); O(snd_pcm_status, audio_tstamp); O(snd_pcm_status, driver_tstamp);
    O(snd_pcm_sync_ptr, flags); O(snd_pcm_sync_ptr, s.status.state); O(snd_pcm_sync_ptr, s.status.hw_ptr);
    O(snd_pcm_sync_ptr, s.status.tstamp); O(snd_pcm_sync_ptr, s.status.suspended_state);
    O(snd_pcm_sync_ptr, s.status.audio_tstamp); O(snd_pcm_sync_ptr, c.control.appl_ptr);
    O(snd_pcm_sync_ptr, c.control.avail_min);
    O(snd_ctl_card_info, card); O(snd_ctl_card_info, id); O(snd_ctl_card_info, driver); O(snd_ctl_card_info, name);
    O(snd_ctl_card_info, longname); O(snd_ctl_card_info, mixername); O(snd_ctl_card_info, components);
    O(snd_ctl_elem_id, numid); O(snd_ctl_elem_id, iface); O(snd_ctl_elem_id, device); O(snd_ctl_elem_id, subdevice);
    O(snd_ctl_elem_id, name); O(snd_ctl_elem_id, index);
    O(snd_ctl_elem_list, offset); O(snd_ctl_elem_list, space); O(snd_ctl_elem_list, used); O(snd_ctl_elem_list, count);
    O(snd_ctl_elem_list, pids);
    O(snd_ctl_elem_info, type); O(snd_ctl_elem_info, access); O(snd_ctl_elem_info, count); O(snd_ctl_elem_info, owner);
    O(snd_ctl_elem_info, value.integer.min); O(snd_ctl_elem_info, value.integer.max);
    O(snd_ctl_elem_info, value.integer.step); O(snd_ctl_elem_value, value.integer.value);
    I(SNDRV_PCM_IOCTL_INFO); I(SNDRV_PCM_IOCTL_HW_REFINE); I(SNDRV_PCM_IOCTL_HW_PARAMS); I(SNDRV_PCM_IOCTL_SW_PARAMS);
    I(SNDRV_PCM_IOCTL_STATUS); I(SNDRV_PCM_IOCTL_STATUS_EXT); I(SNDRV_PCM_IOCTL_SYNC_PTR);
    I(SNDRV_PCM_IOCTL_CHANNEL_INFO); I(SNDRV_PCM_IOCTL_WRITEI_FRAMES); I(SNDRV_CTL_IOCTL_CARD_INFO);
    I(SNDRV_CTL_IOCTL_ELEM_LIST); I(SNDRV_CTL_IOCTL_ELEM_INFO); I(SNDRV_CTL_IOCTL_ELEM_READ);
    I(SNDRV_CTL_IOCTL_ELEM_WRITE); I(SNDRV_CTL_IOCTL_PCM_INFO);
    I(SNDRV_PCM_IOCTL_READI_FRAMES); I(SNDRV_PCM_IOCTL_READN_FRAMES); I(SNDRV_PCM_IOCTL_FORWARD);
    I(SNDRV_PCM_IOCTL_DRAIN); I(SNDRV_PCM_IOCTL_DROP);
    return 0;
}
