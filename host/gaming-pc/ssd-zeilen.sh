#!/bin/bash
# GP-DISK: prints the CONFIG.TXT lines that give BitsOS its two partitions on the Gaming-PC's Lexar SSD, read under
# CachyOS as the normal user (sysfs and lsblk only; nothing is written, no root). Georg copies them into
# \BITSOS2\CONFIG.TXT himself (plan/guides/gaming-pc-ssd.md). The output holds the drive's serial number: it stays on
# this machine, never in git or a chat.
# Usage: bash host/gaming-pc/ssd-zeilen.sh             (the lines; read only)
#        bash host/gaming-pc/ssd-zeilen.sh einhaengen  (mounts the Lexar's BITSOS partition for the copy, guide step 7)
#        bash host/gaming-pc/ssd-zeilen.sh aushaengen  (unmounts it again)
# The two modes exist because udisks hides every EFI System partition (80-udisks2.rules: UDISKS_IGNORE), so Dolphin
# never shows the new BITSOS partition; the script names the device itself, Georg never types a device name.
set -eu
mode="${1:-}"
case "$mode" in ""|einhaengen|aushaengen) ;; *) echo "Unbekannt: $mode (nichts, einhaengen oder aushaengen)"; exit 1 ;; esac
LEXAR_IDS="0x1d97 0x5236"
ESP_TYPE="c12a7328-f81f-11d2-ba4b-00a0c93ec93b"
ctrl=""
for d in /sys/class/nvme/nvme*; do
    [ "$(cat "$d/device/vendor") $(cat "$d/device/device")" = "$LEXAR_IDS" ] && ctrl="$d"
done
[ -n "$ctrl" ] || { echo "Keine Lexar-SSD (1d97:5236) gefunden."; exit 1; }
place="$(basename "$(readlink -f "$ctrl/device")")"
place="${place#0000:}"
ns="$(ls "$ctrl" | grep -E '^nvme[0-9]+(c[0-9]+)?n1$' | head -1)"
[ -n "$ns" ] || { echo "Die Lexar-SSD hat keinen Namensraum 1."; exit 1; }
blk="$(ls /sys/block | grep -E "^nvme[0-9]+n1$" | while read -r b; do [ "$(readlink -f "/sys/block/$b/device")" = "$(readlink -f "$ctrl")" ] && echo "$b"; done | head -1)"
[ -n "$blk" ] || blk="$ns"
lbs="$(cat "/sys/block/$blk/queue/logical_block_size")"
serial="$(sed 's/[[:space:]]*$//' "$ctrl/serial")"
esp=""
n_esp=0
while read -r name ptype puuid; do
    [ "$ptype" = "$ESP_TYPE" ] || continue
    n_esp=$((n_esp + 1))
    [ "$(lsblk -no LABEL "/dev/$name")" = "BITSOS" ] && esp="$name:$puuid"
done < <(lsblk -lno NAME,PARTTYPE,PARTUUID "/dev/$blk" | awk 'NF==3')
echo "# Die Lexar-SSD ($blk, $place) hat diese Partitionen:"
lsblk -lno NAME,SIZE,FSTYPE,LABEL,PARTTYPENAME "/dev/$blk" | awk 'NR>1 {print "#   " $0}'
echo "# Fuer \\BITSOS2\\CONFIG.TXT auf dem Stick (und spaeter auf der Partition BITSOS):"
echo "gp_nvme=$place/1d97:5236"
echo "nvme_write=on"
if [ "$n_esp" -gt 1 ]; then
    echo "# Achtung: die SSD hat $n_esp EFI-Partitionen. BitsOS nimmt sie so nicht. Nur BITSOS darf eine sein."
    exit 1
fi
if [ -z "$esp" ]; then
    echo "# Die Partition BITSOS (FAT32, Markierung boot/esp) fehlt noch: erst Schritt 2 der Anleitung."
    exit 1
fi
part="${esp%%:*}"
guid="${esp#*:}"
if [ "$mode" = einhaengen ]; then
    # udisks asks for Georg's password (a system partition); it prints where the partition is now.
    udisksctl mount -b "/dev/$part"
    exit
fi
if [ "$mode" = aushaengen ]; then
    udisksctl unmount -b "/dev/$part"
    exit
fi
start="$(cat "/sys/block/$blk/$part/start")"
size="$(cat "/sys/block/$blk/$part/size")"
# sysfs counts 512-byte sectors; BitsOS counts the drive's own blocks.
echo "nvme_boot=$serial:${guid^^}:$((start * 512 / lbs)):$((size * 512 / lbs))"
echo "# Zum Kopieren (Schritt 7) haengt 'bash $0 einhaengen' die Partition $part ein, 'bash $0 aushaengen' wieder aus."
