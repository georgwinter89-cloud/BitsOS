//! Field decoding and verb encoding.

use crate::caps::*;
use crate::verb::*;

#[test]
fn widget_caps() {
    let pin = WidgetCaps(0x0040_058D);
    assert_eq!(pin.kind(), WidgetType::Pin);
    assert!(
        pin.stereo()
            && pin.out_amp()
            && pin.amp_override()
            && pin.unsol()
            && pin.conn_list()
            && pin.power_control()
    );
    assert!(!pin.in_amp() && !pin.digital());
    assert_eq!(WidgetCaps(0x0000_62B1).channels(), 8);
    assert_eq!(WidgetCaps(0x00F0_0000).kind(), WidgetType::Vendor);
    assert_eq!(WidgetCaps(0x00A0_0000).kind(), WidgetType::Reserved(0xA));
}

#[test]
fn pin_config() {
    let hp = PinConfig(0x0221_101F);
    assert_eq!(
        (hp.connectivity(), hp.device(), hp.location()),
        (Connectivity::Jack, Device::Headphone, 0x02)
    );
    assert_eq!(
        (hp.conn_type(), hp.color(), hp.association(), hp.sequence()),
        (1, 1, 1, 0xF)
    );
    let spk = PinConfig(0x9017_0110);
    assert_eq!(
        (spk.connectivity(), spk.device()),
        (Connectivity::Fixed, Device::Speaker)
    );
    assert!(spk.no_presence_detect());
    let hdmi = PinConfig(0x1856_00F0);
    assert_eq!(
        (hdmi.connectivity(), hdmi.location(), hdmi.device()),
        (Connectivity::Jack, 0x18, Device::DigitalOtherOut)
    );
    assert_eq!(PinConfig(0x4111_11F0).connectivity(), Connectivity::None);
}

#[test]
fn amp_and_pcm_caps() {
    assert_eq!(AmpCaps(0x0002_5757).zero_db_gain(), 0x57);
    assert_eq!(AmpCaps(0x0000_1F7F).zero_db_gain(), 0x1F); // offset above the top step
    assert!(AmpCaps(0x8000_0000).mute());
    assert!(PcmCaps(0x000E_0560).takes_48k_16());
    assert!(!PcmCaps(0x000C_0560).takes_48k_16());
}

#[test]
fn verbs() {
    assert_eq!(
        command(0, 0x14, set(SET_PIN_WIDGET_CONTROL, 0x40)),
        Some(0x0147_0740)
    );
    assert_eq!(command(15, 0x7F, 0xFFFFF), Some(0xF7FF_FFFF));
    assert_eq!(command(16, 0, 0), None);
    assert_eq!(command(0, 0x80, 0), None);
    assert_eq!(get_amp(true, true, 0), 0xBA000);
    assert_eq!(get_amp(false, false, 3), 0xB0003);
    assert_eq!(set_in_amp(2, 0x17), 0x37217);
    assert!(is_read_only(0xF1C00) && is_read_only(0xA0000) && is_read_only(0xBA000));
    assert!(
        !is_read_only(0x70500)
            && !is_read_only(0x3B000)
            && !is_read_only(0x20011)
            && !is_read_only(0x1_00000)
    );
    assert_eq!(crate::ati::slot_verb(0), 0x777);
    assert_eq!(crate::ati::slot_verb(1), 0x785);
    assert_eq!(crate::ati::slot_verb(7), 0x788);
}
