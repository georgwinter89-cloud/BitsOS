# HDA codec dump: format and the reads it holds

For the desk, who will read the Mini-PC's two codecs (ALC269 on C7:00.6, AMD AA01 on C7:00.1)
before V2 runs there, and for V2 later (`hdacodec::walk::dump_codec` does the same walk over the
CORB/RIRB driver). A dump replaces the synthetic fixture in `fixtures/`; once real, set
`synthetic: no` and the generator test stops comparing it.

## Format

Plain text, one read per line:

```
<cad> <nid> <verb> <answer>
0 00 F0000 10EC0269
0 15 F0900 -
```

- `cad`: codec address, hex `0`-`F`.
- `nid`: node, hex `00`-`7F`.
- `verb`: the low 20 bits of the command word, hex, up to 5 digits (`F0000` = get parameter 0x00,
  `F0204` = connection list from index 4, `BA000` = output amp left).
- `answer`: the 32-bit response, hex, up to 8 digits; `-` if the codec did not answer.
- Optional `0x` prefixes; spaces or tabs between fields. Only the digits 0-9, A-F, a-f (no sign).
- `#` starts a comment line. `# key: value` comments are header fields. Use at least
  `format: hdacodec-dump 1`, `synthetic: no`, `machine`, `pci: <bdf> <vendor:device>`,
  `codec: <vendor:device> (<name>)`, and when/how it was read.
- Only read-only verbs are accepted (`Fxx`, and the 4-bit gets `A`, `B`, `C`, `D`); a set verb
  in a dump is an error. The same read twice must have the same answer.

## The read walk (the exact list)

Read-only verbs only; nothing is powered up, selected or unmuted. Each later step depends on
earlier answers, so the order is fixed. "param X" means verb `F00XX`. Hex throughout.

**1. Root (node 0)**
1. param 00 (vendor/device). No answer: stop, the codec is not there.
2. param 02 (revision).
3. param 04 (subordinate nodes): start = bits 23:16, count = bits 7:0. The function-group nodes
   are start .. start+count-1, keeping only 01..7F.

**2. Each function-group node, in order**
1. param 05 (group type). Bits 7:0 = 1: audio group, continue with 3. Others: nothing more.

**3. Each audio function group** (node `G`)
1. param 0A (PCM sizes/rates), param 0B (stream formats), param 12 (output amp caps), param 0D
   (input amp caps), param 04 (widget range: start/count as above, only 01..7F, a node already
   named as a function group or by an earlier group is skipped).

**4. Each widget node `W` in the range, in order**
1. param 09 (widget caps). No answer: skip this widget. Type = bits 23:20.
2. If caps bit 3 (amp override): param 12, then param 0D.
3. If type 4 (pin): param 0C (pin caps), then `F1C00` (default configuration).
4. If caps bit 8 (connection list): param 0E. Entries n = bits 6:0, long form = bit 7.
   Then `F02ii` for ii = 0, 4, 8, ... below n (short form) or 0, 2, 4, ... below n (long form).
5. If type 0 or 1 (converter) and caps bit 4 (format override): param 0A, then param 0B.

**5. State reads** (after all of 1-4; not needed for the model, they show what the firmware left)

Per audio group `G`: `F0500` (power state), `F2000` (subsystem ID), param 0F (power states),
param 11 (GPIO caps).

Per widget `W` of that group, in node order, in this order:
1. caps bit 10 (power control): `F0500`.
2. type 0 or 1: `A0000` (converter format), `F0600` (stream/channel).
3. caps bit 2 (output amp): `BA000` (left), `B8000` (right).
4. caps bit 1 (input amp): for i = 0 .. min(max(entries, 1), 16) - 1: `B200i` (left),
   `B000i` (right). "entries" = the decoded connection list length (ranges spelled out, at most 256: the select
   verb carries an 8-bit index, so later entries are dropped).
5. type not 2 (not a mixer) and more than one connection: `F0100` (selected input).
6. type 4 (pin): `F0700` (pin control); caps bit 7: `F0800` (unsolicited); pin caps bit 2:
   `F0900` (pin sense, read without triggering); pin caps bit 16: `F0C00` (EAPD); vendor
   1002 (AMD): `F7C00` (HBR control).

A read that does not answer is written with `-` and the walk goes on, leaving out what depends on
it: a widget whose caps do not answer is skipped entirely; after a silent param 0E or `F02ii`, no
further `F02` of that widget is read. The model marks such a widget incomplete and never routes
through it.

## Checking a dump

`hdacodec::walk::check(&dump, cad)` lists reads the walk wants that are missing and reads that are
extra. A complete dump has neither; the fixtures test this.

## Things to know when reading on the Mini-PC

- C7:00.1 (AMD HDMI): the firmware leaves its memory access (bus master) off. V1 read it through
  the immediate command registers without DMA and put it back into reset afterwards
  (`HARDWARE-MINIPC.md`). Without DP audio on, every HDMI pin answers `F0900` with `7FFFFFFF`.
- Reads only. In particular no `705` (power), `707` (pin control), `709` (sense trigger) or
  anything from `ati::setup`; those are SN-2's, and the HDMI ones need DP audio (SN-5), which is a
  display change and only with Georg.
