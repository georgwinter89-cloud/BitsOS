//! The crypto of the station against the published test values V1's wlan-vectors.mjs typed from
//! the documents (each source named at its table; V1 checked them against node's crypto, so a
//! typing slip there would have shown): FIPS 180-4 / RFC 3174 (SHA-1), RFC 2202 (HMAC-SHA1),
//! RFC 6070 (PBKDF2), IEEE 802.11 Annex J.4 (passphrase to PSK), J.3 and 802.11-2007 H.6.5 (PRF),
//! H.7.1 (PTK), FIPS 197 (AES), RFC 3394 (key wrap), RFC 3610 and J.6.4 (CCM / CCMP, the fake
//! AP's cipher), plus js/test-sta.mjs's crypto checks of wpa.mjs.

use wifi::crypto::{self, PskError};
use wifi::secret::{Passphrase, Secret};

fn hex(s: &str) -> Vec<u8> {
    let s: String = s.chars().filter(|c| !c.is_whitespace()).collect();
    (0..s.len() / 2)
        .map(|i| u8::from_str_radix(&s[2 * i..2 * i + 2], 16).unwrap())
        .collect()
}

fn hx(b: &[u8]) -> String {
    b.iter().map(|v| format!("{v:02x}")).collect()
}

#[test]
fn sha1_fips_180_and_rfc_3174() {
    let abc448 = "abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq";
    let abc896 = "abcdefghbcdefghicdefghijdefghijkefghijklfghijklmghijklmnhijklmnoijklmnopjklmnopqklmnopqrlmnopqrsmnopqrstnopqrstu";
    let million = vec![b'a'; 1_000_000];
    let test4 = "01234567".repeat(80);
    let cases: [(&[u8], &str); 6] = [
        (b"abc", "a9993e36 4706816a ba3e2571 7850c26c 9cd0d89d"),
        (b"", "da39a3ee 5e6b4b0d 3255bfef 95601890 afd80709"),
        (
            abc448.as_bytes(),
            "84983e44 1c3bd26e baae4aa1 f95129e5 e54670f1",
        ),
        (
            abc896.as_bytes(),
            "a49b2446 a02c645b f419f995 b6709125 3a04a259",
        ),
        (&million, "34aa973c d4c4daa4 f61eeb2b dbad2731 6534016f"),
        (
            test4.as_bytes(),
            "dea356a2 cddd90c7 a7ecedc5 ebb56393 4f460452",
        ),
    ];
    for (data, want) in cases {
        assert_eq!(
            crypto::sha1(data).to_vec(),
            hex(want),
            "SHA-1 of {} bytes",
            data.len()
        );
    }
}

#[test]
fn hmac_sha1_rfc_2202() {
    let cases: [(Vec<u8>, Vec<u8>, &str); 7] = [
        (
            vec![0x0b; 20],
            b"Hi There".to_vec(),
            "b617318655057264e28bc0b6fb378c8ef146be00",
        ),
        (
            b"Jefe".to_vec(),
            b"what do ya want for nothing?".to_vec(),
            "effcdf6ae5eb2fa2d27416d5f184df9c259a7c79",
        ),
        (
            vec![0xaa; 20],
            vec![0xdd; 50],
            "125d7342b9ac11cd91a39af48aa17b4f63f175d3",
        ),
        (
            hex("0102030405060708090a0b0c0d0e0f10111213141516171819"),
            vec![0xcd; 50],
            "4c9007f4026250c6bc8414f9bf50c86c2d7235da",
        ),
        (
            vec![0x0c; 20],
            b"Test With Truncation".to_vec(),
            "4c1a03424b55e07fe7f27be1d58bb9324a9a5a04",
        ),
        (
            vec![0xaa; 80],
            b"Test Using Larger Than Block-Size Key - Hash Key First".to_vec(),
            "aa4ae5e15272d00e95705637ce8a3b55ed402112",
        ),
        (
            vec![0xaa; 80],
            b"Test Using Larger Than Block-Size Key and Larger Than One Block-Size Data".to_vec(),
            "e8e99d0f45237d786d6bbaa7965c7808bbff1a91",
        ),
    ];
    for (i, (key, data, want)) in cases.iter().enumerate() {
        assert_eq!(
            crypto::hmac_sha1(key, &[data]).to_vec(),
            hex(want),
            "RFC 2202 case {}",
            i + 1
        );
        // in pieces too
        let (a, b) = data.split_at(data.len() / 2);
        assert_eq!(crypto::hmac_sha1(key, &[a, b]).to_vec(), hex(want));
    }
    // the EAPOL MIC is HMAC-SHA1 cut to 128 bits (js/test-sta.mjs: RFC 2202 case 2)
    let mut kck = [0u8; 16];
    kck[..4].copy_from_slice(b"Jefe");
    let m = crypto::hmac_sha1(b"Jefe", &[b"what do ya want for nothing?"]);
    assert_eq!(hx(&m[..16]), "effcdf6ae5eb2fa2d27416d5f184df9c");
}

#[test]
fn pbkdf2_rfc_6070() {
    // the 16777216-iteration case takes an hour, left out as V1 did
    let cases: [(&[u8], &[u8], u32, &str); 5] = [
        (
            b"password",
            b"salt",
            1,
            "0c60c80f961f0e71f3a9b524af6012062fe037a6",
        ),
        (
            b"password",
            b"salt",
            2,
            "ea6c014dc72d6f8ccd1ed92ace1d41f0d8de8957",
        ),
        (
            b"password",
            b"salt",
            4096,
            "4b007901b765489abead49d926f721d065a429c1",
        ),
        (
            b"passwordPASSWORDpassword",
            b"saltSALTsaltSALTsaltSALTsaltSALTsalt",
            4096,
            "3d2eec4fe41c849b80c8d83662c0e44a8b291a964cf2f07038",
        ),
        (
            b"pass\0word",
            b"sa\0lt",
            4096,
            "56fa6aa75548099dcc37d7f03425e0c3",
        ),
    ];
    for (p, s, n, want) in cases {
        let w = hex(want);
        let mut out = vec![0u8; w.len()];
        crypto::pbkdf2_sha1(p, s, n, &mut out);
        assert_eq!(out, w, "RFC 6070 {} rounds", n);
    }
}

#[test]
fn psk_annex_j4() {
    let cases = [
        (
            "password",
            "IEEE",
            "f42c6fc52df0ebef9ebb4b90b38a5f902e83fe1b135a70e23aed762e9710a12e",
        ),
        (
            "ThisIsAPassword",
            "ThisIsASSID",
            "0dc0d6eb90555ed6419756b9a15ec3e3209b63df707dd508d14581f8982721af",
        ),
        (
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "ZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZ",
            "becb93866bb8c3832cb777c2f559807c8c59afcb6eae734885001300a981cc62",
        ),
    ];
    for (p, s, want) in cases {
        let pmk = crypto::psk(p.as_bytes(), s.as_bytes()).unwrap();
        assert_eq!(pmk.expose().to_vec(), hex(want), "J.4 {s}");
        // CONFIG.TXT's wlan_psk= as a passphrase, and the same PSK as 64 hex digits
        let c = crypto::pmk_from_config(&Passphrase::new(p.as_bytes()), s.as_bytes()).unwrap();
        assert!(c.ct_eq(&pmk));
        let h = crypto::pmk_from_config(&Passphrase::new(want.as_bytes()), s.as_bytes()).unwrap();
        assert!(h.ct_eq(&pmk));
    }
    // the refusals carry no part of the value (R8i)
    assert_eq!(
        crypto::psk(b"short", b"x").err(),
        Some(PskError::Length { len: 5 })
    );
    assert_eq!(
        crypto::psk(&[b'a'; 64], b"x").err(),
        Some(PskError::Length { len: 64 })
    );
    assert_eq!(
        crypto::psk(b"pass\x07word", b"x").err(),
        Some(PskError::NotPrintable)
    );
    assert_eq!(
        crypto::psk(b"password", b"").err(),
        Some(PskError::Ssid { len: 0 })
    );
    assert_eq!(
        crypto::psk(b"password", &[b's'; 33]).err(),
        Some(PskError::Ssid { len: 33 })
    );
    let bad_hex = Passphrase::new(&[b'g'; 64]);
    assert_eq!(
        crypto::pmk_from_config(&bad_hex, b"x").err(),
        Some(PskError::NotHex)
    );
    let shown = format!(
        "{:?} {}",
        crypto::psk(b"short", b"x").err().unwrap(),
        PskError::NotHex
    );
    assert!(!shown.contains("short"));
    assert_eq!(format!("{:?}", bad_hex), "<passphrase>");
    assert_eq!(
        format!("{:?}", crypto::psk(b"password", b"IEEE").unwrap()),
        "<secret 32 bytes>"
    );
}

#[test]
fn prf_annex_j3_and_h65() {
    let cases: [(Vec<u8>, &str, Vec<u8>, &str); 7] = [
        (
            vec![0x0b; 20],
            "prefix",
            b"Hi There".to_vec(),
            "bcd4c650b30b9684951829e0d75f9d54b862175ed9f00606e17d8da35402ffee75df78c3d31e0f889f012120c0862beb67753e7439ae242edb8373698356cf5a",
        ),
        (
            b"Jefe".to_vec(),
            "prefix",
            b"what do ya want for nothing?".to_vec(),
            "51f4de5b33f249adf81aeb713a3c20f4fe631446fabdfa58244759ae58ef9009a99abf4eac2ca5fa87e692c440eb40023e7babb206d61de7b92f41529092b8fc",
        ),
        (
            vec![0xaa; 20],
            "prefix",
            vec![0xdd; 50],
            "e1ac546ec4cb636f9976487be5c86be17a0252ca5d8d8df12cfb0473525249ce9dd8d177ead710bc9b590547239107aef7b4abd43d87f0a68f1cbd9e2b6f7607",
        ),
        // 802.11-2007 H.6.5 Tables H.9 to H.12: PRF-192, -256, -384 (80-byte key), -512
        (
            vec![0x0b; 20],
            "prefix",
            b"Hi There".to_vec(),
            "bcd4c650b30b9684951829e0d75f9d54b862175ed9f00606",
        ),
        (
            b"Jefe".to_vec(),
            "prefix-2",
            b"what do ya want for nothing?".to_vec(),
            "47c4908e30c947521ad20be9053450ecbea23d3aa604b77326d8b3825ff7475c",
        ),
        (
            vec![0xaa; 80],
            "prefix-3",
            b"Test Using Larger Than Block-Size Key - Hash Key First".to_vec(),
            "0ab6c33ccf70d0d736f4b04c8a7373255511abc5073713163bd0b8c9eeb7e1956fa066820a73ddee3f6d3bd407e0682a",
        ),
        (
            vec![0x0b; 20],
            "prefix-4",
            b"Hi There Again".to_vec(),
            "248cfbc532ab38ffa483c8a2e40bf170eb542a2e0916d7bf6d97da2c4c5ca877736c53a65b03fa4b3745ce7613f6ad68e0e4a798b7cf691c96176fd634a59a49",
        ),
    ];
    for (key, label, data, want) in cases {
        let w = hex(want);
        let mut out = vec![0u8; w.len()];
        crypto::prf(&key, label.as_bytes(), &data, &mut out);
        assert_eq!(hx(&out), hx(&w), "PRF {label} {} bytes", w.len());
    }
}

#[test]
fn ptk_h71_and_symmetry() {
    // 802.11-2007 H.7.1 Tables H.13 to H.15
    let pmk = Secret::from_slice(&hex(
        "0dc0d6eb90555ed6419756b9a15ec3e3209b63df707dd508d14581f8982721af",
    ))
    .unwrap();
    let aa: [u8; 6] = hex("a0a1a1a3a4a5").try_into().unwrap();
    let spa: [u8; 6] = hex("b0b1b2b3b4b5").try_into().unwrap();
    let snonce: [u8; 32] = hex("c0c1c2c3c4c5c6c7c8c9d0d1d2d3d4d5d6d7d8d9dadbdcdddedfe0e1e2e3e4e5")
        .try_into()
        .unwrap();
    let anonce: [u8; 32] = hex("e0e1e2e3e4e5e6e7e8e9f0f1f2f3f4f5f6f7f8f9fafbfcfdfeff000102030405")
        .try_into()
        .unwrap();
    let p = crypto::ptk(&pmk, &aa, &spa, &anonce, &snonce);
    let all = [
        p.kck.expose().to_vec(),
        p.kek.expose().to_vec(),
        p.tk.expose().to_vec(),
    ]
    .concat();
    assert_eq!(
        hx(&all),
        hx(&hex(
            "379f9852d0199236b94e407ce4c00ec8 47c9edc01c2c6e5b4910caddfb3e51a7 b2360c79e9710fdd58bea93deaf06599"
        ))
    );
    // js/test-sta.mjs: any argument order gives the same keys; PRF-384 over min/max written out
    let one = Secret::new([1u8; 32]);
    let (a1, a2) = ([2, 0, 0, 0, 0, 1], [2, 0, 0, 0, 0, 2]);
    let p1 = crypto::ptk(&one, &a1, &a2, &[3; 32], &[4; 32]);
    let p2 = crypto::ptk(&one, &a2, &a1, &[4; 32], &[3; 32]);
    assert!(p1.tk.ct_eq(&p2.tk) && p1.kck.ct_eq(&p2.kck) && p1.kek.ct_eq(&p2.kek));
    let mut data = Vec::new();
    data.extend_from_slice(&a1);
    data.extend_from_slice(&a2);
    data.extend_from_slice(&[3; 32]);
    data.extend_from_slice(&[4; 32]);
    let mut want = [0u8; 48];
    crypto::prf(&[1; 32], b"Pairwise key expansion", &data, &mut want);
    assert_eq!(p1.kck.expose()[..], want[..16]);
    assert_eq!(p1.kek.expose()[..], want[16..32]);
    assert_eq!(p1.tk.expose()[..], want[32..]);
    assert_eq!(format!("{p1:?}"), "<PTK>");
}

#[test]
fn aes_fips_197_and_key_wrap_rfc_3394() {
    for (k, p, c) in [
        (
            "000102030405060708090a0b0c0d0e0f",
            "00112233445566778899aabbccddeeff",
            "69c4e0d86a7b0430d8cdb78070b4c55a",
        ),
        (
            "2b7e151628aed2a6abf7158809cf4f3c",
            "3243f6a8885a308d313198a2e0370734",
            "3925841d02dc09fbdc118597196a0b32",
        ),
    ] {
        let key: [u8; 16] = hex(k).try_into().unwrap();
        let mut b: [u8; 16] = hex(p).try_into().unwrap();
        crypto::aes128_encrypt_block(&key, &mut b);
        assert_eq!(hx(&b), c);
        crypto::aes128_decrypt_block(&key, &mut b);
        assert_eq!(hx(&b), p);
    }
    let kek = Secret::from_slice(&hex("000102030405060708090A0B0C0D0E0F")).unwrap();
    let wrapped = hex("1FA68B0A8112B447 AEF34BD8FB5A7B82 9D3E862371D2CFE5");
    let mut out = [0u8; 16];
    assert!(crypto::aes_unwrap(&kek, &wrapped, &mut out));
    assert_eq!(hx(&out), "00112233445566778899aabbccddeeff");
    let mut again = [0u8; 24];
    assert!(crypto::aes_wrap(&kek, &out, &mut again));
    assert_eq!(again.to_vec(), wrapped);
    // a wrong KEK fails the integrity check and leaves nothing behind
    let mut junk = [0x55u8; 16];
    assert!(!crypto::aes_unwrap(
        &Secret::new([0; 16]),
        &wrapped,
        &mut junk
    ));
    assert_eq!(junk, [0; 16]);
    // lengths that do not fit are refused
    assert!(!crypto::aes_unwrap(&kek, &wrapped[..16], &mut [0u8; 8]));
    assert!(!crypto::aes_unwrap(&kek, &wrapped[..23], &mut [0u8; 15]));
    assert!(!crypto::aes_unwrap(&kek, &wrapped, &mut [0u8; 8]));
}

/// AES-CCM with M = 8 (the CCMP of the fake AP), from RustCrypto's ccm
fn ccm8(key: &[u8], nonce: &[u8], ad: &[u8], text: &[u8]) -> Vec<u8> {
    use ccm::aead::{AeadInPlace, NewAead, generic_array::GenericArray};
    use ccm::consts::{U8, U13};
    type Ccm = ccm::Ccm<aes::Aes128, U8, U13>;
    let c = Ccm::new(GenericArray::from_slice(key));
    let mut buf = text.to_vec();
    let tag = c
        .encrypt_in_place_detached(GenericArray::from_slice(nonce), ad, &mut buf)
        .unwrap();
    buf.extend_from_slice(&tag);
    buf
}

#[test]
fn ccm_rfc_3610_and_ccmp_annex_j64() {
    let cases = [
        (
            1,
            "C0C1C2C3C4C5C6C7C8C9CACBCCCDCECF",
            "00000003020100A0A1A2A3A4A5",
            8,
            "000102030405060708090A0B0C0D0E0F101112131415161718191A1B1C1D1E",
            "0001020304050607588C979A61C663D2F066D0C2C0F989806D5F6B61DAC38417E8D12CFDF926E0",
        ),
        (
            2,
            "C0C1C2C3C4C5C6C7C8C9CACBCCCDCECF",
            "00000004030201A0A1A2A3A4A5",
            8,
            "000102030405060708090A0B0C0D0E0F101112131415161718191A1B1C1D1E1F",
            "000102030405060772C91A36E135F8CF291CA894085C87E3CC15C439C9E43A3BA091D56E10400916",
        ),
        (
            3,
            "C0C1C2C3C4C5C6C7C8C9CACBCCCDCECF",
            "00000005040302A0A1A2A3A4A5",
            8,
            "000102030405060708090A0B0C0D0E0F101112131415161718191A1B1C1D1E1F20",
            "000102030405060751B1E5F44A197D1DA46B0F8E2D282AE871E838BB64DA8596574ADAA76FBD9FB0C5",
        ),
        (
            4,
            "C0C1C2C3C4C5C6C7C8C9CACBCCCDCECF",
            "00000006050403A0A1A2A3A4A5",
            12,
            "000102030405060708090A0B0C0D0E0F101112131415161718191A1B1C1D1E",
            "000102030405060708090A0BA28C6865939A9A79FAAA5C4C2A9D4A91CDAC8C96C861B9C9E61EF1",
        ),
        (
            5,
            "C0C1C2C3C4C5C6C7C8C9CACBCCCDCECF",
            "00000007060504A0A1A2A3A4A5",
            12,
            "000102030405060708090A0B0C0D0E0F101112131415161718191A1B1C1D1E1F",
            "000102030405060708090A0BDCF1FB7B5D9E23FB9D4E131253658AD86EBDCA3E51E83F077D9C2D93",
        ),
        (
            6,
            "C0C1C2C3C4C5C6C7C8C9CACBCCCDCECF",
            "00000008070605A0A1A2A3A4A5",
            12,
            "000102030405060708090A0B0C0D0E0F101112131415161718191A1B1C1D1E1F20",
            "000102030405060708090A0B6FC1B011F006568B5171A42D953D469B2570A4BD87405A0443AC91CB94",
        ),
        (
            13,
            "D7828D13B2B0BDC325A76236DF93CC6B",
            "00412B4EA9CDBE3C9696766CFA",
            8,
            "0BE1A88BACE018B108E8CF97D820EA258460E96AD9CF5289054D895CEAC47C",
            "0BE1A88BACE018B14CB97F86A2A4689A877947AB8091EF5386A6FFBDD080F8E78CF7CB0CDDD7B3",
        ),
        (
            14,
            "D7828D13B2B0BDC325A76236DF93CC6B",
            "0033568EF7B2633C9696766CFA",
            8,
            "63018F76DC8A1BCB9020EA6F91BDD85AFA0039BA4BAFF9BFB79C7028949CD0EC",
            "63018F76DC8A1BCB4CCB1E7CA981BEFAA0726C55D378061298C85C92814ABC33C52EE81D7D77C08A",
        ),
        (
            15,
            "D7828D13B2B0BDC325A76236DF93CC6B",
            "00103FE41336713C9696766CFA",
            8,
            "AA6CFA36CAE86B40B916E0EACC1C00D7DCEC68EC0B3BBB1A02DE8A2D1AA346132E",
            "AA6CFA36CAE86B40B1D23A2220DDC0AC900D9AA03C61FCF4A559A4417767089708A776796EDB723506",
        ),
        (
            16,
            "D7828D13B2B0BDC325A76236DF93CC6B",
            "00764C63B8058E3C9696766CFA",
            12,
            "D0D0735C531E1BECF049C24412DAAC5630EFA5396F770CE1A66B21F7B2101C",
            "D0D0735C531E1BECF049C24414D253C3967B70609B7CBB7C499160283245269A6F49975BCADEAF",
        ),
        (
            17,
            "D7828D13B2B0BDC325A76236DF93CC6B",
            "00F8B678094E3B3C9696766CFA",
            12,
            "77B60F011C03E1525899BCAEE88B6A46C78D63E52EB8C546EFB5DE6F75E9CC0D",
            "77B60F011C03E1525899BCAE5545FF1A085EE2EFBF52B2E04BEE1E2336C73E3F762C0C7744FE7E3C",
        ),
        (
            18,
            "D7828D13B2B0BDC325A76236DF93CC6B",
            "00D560912D3F703C9696766CFA",
            12,
            "CD9044D2B71FDB8120EA60C06435ACBAFB11A82E2F071D7CA4A5EBD93A803BA87F",
            "CD9044D2B71FDB8120EA60C0009769ECABDF48625594C59251E6035722675E04C847099E5AE0704551",
        ),
    ];
    for (n, k, nonce, h, input, output) in cases {
        let i = hex(input);
        let o = hex(output);
        assert_eq!(
            ccm8(&hex(k), &hex(nonce), &i[..h], &i[h..]),
            o[h..].to_vec(),
            "RFC 3610 #{n}"
        );
    }
    // 802.11 Annex J.6.4 (CCMP test MPDU 1 of P802.11i/D7.0) and MPDU 2: AAD and nonce as
    // 12.5.3.3 makes them for a data frame without address 4 and QoS (wlan-ref.mjs ccmpParts)
    for (tk, pn, plain, enc) in [
        (
            "c97c1f67ce371185514a8a19f2bdd52f",
            0xB503_9776_E70Cu64,
            "0848c32c0fd2e128a57c5030f1844408abaea5b8fcba8033f8ba1a55d02f85ae967bb62fb6cda8eb7e78a050",
            "0848c32c0fd2e128a57c5030f1844408abaea5b8fcba80330ce70020769703b5f3d0a2fe9a3dbf2342a643e43246e80c3c04d0197845ce0b16f97623",
        ),
        (
            "8f7a053fa577a5597529272097a603d5",
            0x31F3_CBBA_97EA,
            "38c06a51ea100c846850eec1762c88deaf2ee9f46a07e0cc83a0634b5ed7627eb9df225e05740342de194117",
            "38c06a51ea100c846850eec1762c88deaf2ee9f46a07e0ccea9700a0bacbf331814b6965d05bf2b2ed38d4beb069fe82714a610b542fbf8da06aa4ae",
        ),
    ] {
        let p = hex(plain);
        let e = hex(enc);
        let (aad, nonce) = ccmp_parts(&p[..24], pn);
        assert_eq!(
            hx(&ccm8(&hex(tk), &nonce, &aad, &p[24..])),
            hx(&e[32..]),
            "CCMP J.6.4"
        );
    }
}

/// 802.11 12.5.3.3: the AAD and nonce of a data frame without address 4 and QoS
pub fn ccmp_parts(h: &[u8], pn: u64) -> (Vec<u8>, Vec<u8>) {
    let mut aad = vec![h[0] & 0x8f, (h[1] & 0xc7) | 0x40];
    aad.extend_from_slice(&h[4..22]);
    aad.extend_from_slice(&[h[22] & 0x0f, 0]);
    let mut nonce = vec![0u8];
    nonce.extend_from_slice(&h[10..16]);
    nonce.extend_from_slice(&pn.to_be_bytes()[2..]);
    (aad, nonce)
}
