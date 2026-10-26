#!/bin/bash
# WL: the JS driver's own module tests are the oracles rtw89-core is checked against (tests/pci_js.rs runs
# js/pci.mjs and js/dmac.mjs, tests/pcitx_js.rs js/pcitx.mjs, tests/fake_golden.rs V1's fake chip and
# bringup-golden.txt, tests/pcitx_golden.rs the golden trace's pcitx lines, tests/fw_js.rs js/fw.mjs with
# fw-parse.mjs and fw-h2c.mjs on the real firmware file, tests/macinit_js.rs js/macinit*.mjs and the C's sequences
# of macinit-spec.mjs, tests/phy_js.rs js/phy*.mjs and the C's sequences of phy-test-expect.mjs, tests/rfk1_js.rs
# js/rfk1*.mjs on test-rfk1.mjs's chip model and the RFK tables of the C files, tests/rfk2_js.rs js/rfk2*.mjs on
# test-rfk2.mjs's chip model with the real efuse dump and firmware file; test-rfk2.mjs checks rfk2 against the C;
# tests/chan_js.rs js/chan*.mjs on test-chan.mjs's device with the real firmware file and efuse dump, test-chan.mjs
# checks chan against sequences written from the C; tests/btc_js.rs js/btc*.mjs on test-btc.mjs's device with the
# real firmware's version, test-btc.mjs checks btc against sequences and H2C layouts written from the C;
# tests/txrx_js.rs js/txrx*.mjs on seeded inputs: TX infos, the station's frames, data frames, parsers, RX buffers;
# fake_golden's whole_bringup_trace_is_golden the whole bring-up to the stop, sta (WL-3) included, nothing replayed,
# every kicked H2C and frame against the JS run's (tests/fake/bringup-ref.mjs); fake_golden's sta_life_cycle,
# track_work and wlan_as_netdev tests the station on V1's fake AP (the last with V1's LAN behind it, UDP on the
# lane's first port and the next); tests/sta_c.rs the station's H2Cs and citations against the C; test-sta.mjs and
# test-track.mjs the JS station itself; test-bringup.mjs checks the JS bring-up itself against bringup-golden.txt).
# This script runs the JS module tests of the ported modules, so a broken oracle shows here and not as a puzzling
# Rust diff, then the node-based rtw89-core tests (never with RTW89_SKIP_FAKE). Host only: no QEMU, no ports, no
# hardware.
# The WLAN tree (WLAN_TOOLS, default /home/georg/BitsOS/werkzeuge/plattform/wlan) is only read.
set -euo pipefail
cd "${BITSOS_ROOT:-$(git rev-parse --show-toplevel)}"
WZ="${WLAN_TOOLS:-/home/georg/BitsOS/werkzeuge/plattform/wlan}"
RUN_DIR="${BITSOS_RUN_DIR:-target/xtask/runs/wl-10-rtw89-oracles}"
mkdir -p "$RUN_DIR"
unset RTW89_SKIP_FAKE
export BITSOS_GATE=1                           # the node-based tests refuse RTW89_SKIP_FAKE under it

for t in test-pci.mjs test-pcitx.mjs test-fw.mjs test-macinit.mjs test-phy.mjs test-rfk1.mjs test-rfk2.mjs test-chan.mjs test-btc.mjs test-txrx.mjs test-sta.mjs test-track.mjs test-bringup.mjs; do   # one per ported module (WL-2: pci, dmac, pcitx, fw, macinit, phy, rfk1, rfk2, chan, btc, txrx; WL-3: sta, track), then the whole JS bring-up
    echo "== node js/$t"
    (cd "$RUN_DIR" && node "$WZ/js/$t") | tee "$RUN_DIR/${t%.mjs}.log"
    tail -n 1 "$RUN_DIR/${t%.mjs}.log" | grep -qx OK || { echo "RED: js/$t did not end with OK"; exit 1; }
done

echo "== rtw89-core against the JS modules and the golden trace"
CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$PWD/target}" \
    WL_LAN_PORT="${BITSOS_PORT_LO:-28000}" \
    cargo test -p rtw89-core --offline --features std --test pci_js --test pcitx_js --test pcitx_golden --test fake_golden --test fw_js --test macinit_js --test phy_js --test rfk1_js --test rfk2_js --test chan_js --test btc_js --test txrx_js --test sta_c 2>&1 | tee "$RUN_DIR/cargo.log"
grep -q "SKIPPED" "$RUN_DIR/cargo.log" && { echo "RED: a node-based test skipped"; exit 1; }

echo "== wifi: crypto vectors, the hashcat capture, V1's EAPOL cases, the join, the supervisor"
CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$PWD/target}" cargo test -p wifi --offline 2>&1 | tee "$RUN_DIR/wifi.log"
echo "GREEN"
