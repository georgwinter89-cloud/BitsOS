#!/usr/bin/env python3
"""GM-1c: reads the traces of drmtrace.c (format in drmtrace.md) for the scenarios and by hand.

  drmtrace.py summary TRACE            the ioctls by name with their answers, the first CS with its IBs' PM4 packets
  drmtrace.py info TRACE REF.info      INFO answers of TRACE against a reference (the same record format, or the
                                       "Q <query> a=<a0>,<a1>,<a2>,<a3> sz=<n>:<hex>" lines of a .info file)
Exit 0; 1 when the trace has damaged lines (cut, bad hex: listed, never a traceback) or an INFO answer differs.
As a module: parse(text), name(nr), cs_list(recs), describe_cs(rec), pm4(ib_bytes), info_answers(recs),
read_info(text), compare_info(mine, ref), info_differs(lines), damaged(recs), summary(recs)."""
import re
import struct
import sys

sys.dont_write_bytecode = True

# DRM core and amdgpu command numbers (include/uapi/drm/drm.h, amdgpu_drm.h: DRM_COMMAND_BASE 0x40 + n).
NAMES = {0x00: "VERSION", 0x09: "GEM_CLOSE", 0x0c: "GET_CAP", 0x0d: "SET_CLIENT_CAP", 0x2d: "PRIME_HANDLE_TO_FD",
         0x2e: "PRIME_FD_TO_HANDLE", 0xbf: "SYNCOBJ_CREATE", 0xc0: "SYNCOBJ_DESTROY", 0xc1: "SYNCOBJ_HANDLE_TO_FD",
         0xc2: "SYNCOBJ_FD_TO_HANDLE", 0xc3: "SYNCOBJ_WAIT", 0xc4: "SYNCOBJ_RESET", 0xc5: "SYNCOBJ_SIGNAL",
         0xca: "SYNCOBJ_TIMELINE_WAIT", 0xcb: "SYNCOBJ_QUERY", 0xcc: "SYNCOBJ_TRANSFER", 0xcd: "SYNCOBJ_TIMELINE_SIGNAL",
         0xcf: "SYNCOBJ_EVENTFD",
         0x40: "AMDGPU_GEM_CREATE", 0x41: "AMDGPU_GEM_MMAP", 0x42: "AMDGPU_CTX", 0x43: "AMDGPU_BO_LIST", 0x44: "AMDGPU_CS",
         0x45: "AMDGPU_INFO", 0x46: "AMDGPU_GEM_METADATA", 0x47: "AMDGPU_GEM_WAIT_IDLE", 0x48: "AMDGPU_GEM_VA",
         0x49: "AMDGPU_WAIT_CS", 0x50: "AMDGPU_GEM_OP", 0x51: "AMDGPU_GEM_USERPTR", 0x52: "AMDGPU_WAIT_FENCES",
         0x53: "AMDGPU_VM", 0x54: "AMDGPU_FENCE_TO_HANDLE", 0x55: "AMDGPU_SCHED"}
# PM4 type-3 opcodes RADV and radeonsi use most (the values of linux-6.6/drivers/gpu/drm/amd/amdgpu/nvd.h).
PM4 = {0x10: "NOP", 0x12: "CLEAR_STATE", 0x13: "INDEX_BUFFER_SIZE", 0x15: "DISPATCH_DIRECT", 0x16: "DISPATCH_INDIRECT",
       0x20: "SET_PREDICATION", 0x22: "COND_EXEC", 0x26: "INDEX_BASE", 0x27: "DRAW_INDEX_2", 0x28: "CONTEXT_CONTROL",
       0x2a: "INDEX_TYPE", 0x2d: "DRAW_INDEX_AUTO", 0x2f: "NUM_INSTANCES", 0x37: "WRITE_DATA", 0x3c: "WAIT_REG_MEM",
       0x3f: "INDIRECT_BUFFER", 0x40: "COPY_DATA", 0x41: "CP_DMA", 0x42: "PFP_SYNC_ME", 0x46: "EVENT_WRITE",
       0x49: "RELEASE_MEM", 0x4a: "PREAMBLE_CNTL", 0x50: "DMA_DATA", 0x58: "ACQUIRE_MEM", 0x68: "SET_CONFIG_REG",
       0x69: "SET_CONTEXT_REG", 0x6a: "SET_CONTEXT_REG_INDEX", 0x75: "GFX_PIPE_LOCK", 0x76: "SET_SH_REG",
       0x79: "SET_UCONFIG_REG", 0x7a: "SET_UCONFIG_REG_INDEX", 0x90: "FRAME_CONTROL", 0x9b: "SET_SH_REG_INDEX"}
IP = {0: "gfx", 1: "compute", 2: "sdma", 3: "uvd", 4: "vce", 5: "uvd_enc", 6: "vcn_dec", 7: "vcn_enc", 8: "vcn_jpeg"}
CHUNK = {1: "IB", 2: "FENCE", 3: "DEPENDENCIES", 4: "SYNCOBJ_IN", 5: "SYNCOBJ_OUT", 6: "BO_HANDLES",
         7: "SCHEDULED_DEPENDENCIES", 8: "SYNCOBJ_TIMELINE_WAIT", 9: "SYNCOBJ_TIMELINE_SIGNAL", 10: "CP_GFX_SHADOW"}


def fields(line):
    return dict(m.groups() for m in re.finditer(r"(\w+)=(\S*)", line))


HEXF = ("in", "out", "data")


def _record(line, recs, by_seq, pending):
    """One line into `recs`; raises ValueError/KeyError/IndexError on a damaged line."""
    if line[:2] in ("I ", "P "):
        f = fields(line)
        for k in HEXF:
            if k in f and (len(f[k]) % 2 or not re.fullmatch(r"[0-9a-fA-F]*", f[k])):
                raise ValueError(f"{k}= is no even run of hex digits")
    if line.startswith("I "):
        seq = int(line.split()[1])
        f = fields(line)
        r = {"kind": "I", "seq": seq, "fd": int(f["fd"]), "nr": int(f["nr"], 16), "size": int(f["size"]),
             "ret": int(f["ret"]), "errno": int(f["errno"]), "in": bytes.fromhex(f["in"]),
             "out": bytes.fromhex(f["out"]), "p": pending.pop(seq, [])}
        if len(r["in"]) != r["size"] or len(r["out"]) != r["size"]:
            raise ValueError(f"in= or out= is not size={r['size']} bytes")
        by_seq[seq] = r
        recs.append(r)
    elif line.startswith("P "):
        parts = line.split()
        seq, kind = int(parts[1]), parts[2]
        f = fields(line)
        if len(parts) > 3 and parts[3].isdigit():
            f["index"] = parts[3]
        for state in ("unreadable", "unmapped"):
            if state in parts[3:]:
                f["state"] = state
        # Payloads read before the call come before its I record, those read after it after.
        (by_seq[seq]["p"] if seq in by_seq else pending.setdefault(seq, [])).append((kind, f))
    elif line.startswith("U "):
        f = fields(line)
        recs.append({"kind": "U", "seq": int(line.split()[1]), "fd": int(f["fd"]), "nr": int(f["nr"], 16),
                     "ret": int(f["ret"]), "errno": int(f["errno"])})
    elif line.startswith("C "):
        recs.append({"kind": "C", "fd": int(fields(line)["fd"])})
    elif line.startswith("E "):
        recs.append({"kind": "E", "cs": int(fields(line)["cs"])})
    elif line.strip():
        raise ValueError("no record of drmtrace")


def parse(text):
    """The records of a trace: dicts with kind I/U/C/E and, for I, its payloads (list of (kind, fields)) in `p`;
    a damaged line (cut, bad hex, unknown) becomes {"kind": "bad", "line": n, "why": ...} instead of an exception."""
    recs, by_seq, pending = [], {}, {}
    for n, line in enumerate(text.splitlines(), 1):
        try:
            _record(line, recs, by_seq, pending)
        except (ValueError, KeyError, IndexError) as e:
            recs.append({"kind": "bad", "line": n, "why": str(e) or type(e).__name__})
    return recs


def damaged(recs):
    return [r for r in recs if r["kind"] == "bad"]


def name(nr):
    return NAMES.get(nr, f"nr_0x{nr:x}")


def pm4(data):
    """[(opcode name, dwords)] of a gfx/compute IB: type-3 packets by their header, type-2 fillers, the rest raw."""
    out, i, n = [], 0, len(data) // 4
    words = struct.unpack(f"<{n}I", data[:n * 4])
    while i < n:
        h = words[i]
        t = h >> 30
        if t == 3:
            op, cnt = (h >> 8) & 0xff, ((h >> 16) & 0x3fff) + 1
            out.append((PM4.get(op, f"op_0x{op:02x}"), cnt + 1))
            i += cnt + 1
        elif t == 2:
            out.append(("TYPE2", 1))
            i += 1
        else:
            out.append((f"type{t}_0x{h:08x}", 1))
            i += 1
    return out


def cs_list(recs):
    return [r for r in recs if r["kind"] == "I" and r["nr"] == 0x44]


def describe_cs(r):
    """The CS's chunks and IBs as plain text lines."""
    ctx = struct.unpack_from('<I', r['in'])[0] if len(r["in"]) >= 4 else "?"
    lines = [f"CS seq={r['seq']} ret={r['ret']} errno={r['errno']} ctx={ctx}"]
    for kind, f in r["p"]:
        if kind == "chunk" and "id" not in f:
            lines.append(f"  chunk {f.get('index', '?')}: unreadable when traced")
        elif kind == "chunks":
            lines.append(f"  chunk array of {f.get('n', '?')}: unreadable when traced")
        elif kind == "chunk":
            lines.append(f"  chunk {f['index']}: {CHUNK.get(int(f['id']), f['id'])} {f['len_dw']} dw")
        elif kind == "ib":
            ip = IP.get(int(f["ip"]), f["ip"])
            if "data" in f:
                pk = pm4(bytes.fromhex(f["data"])) if ip in ("gfx", "compute") else []
                ops = {}
                for o, _ in pk:
                    ops[o] = ops.get(o, 0) + 1
                lines.append(f"  IB {f['index']}: {ip} ring {f['ring']} va {f['va']} {f['bytes']} bytes flags {f['flags']}"
                             f", {len(pk)} packets: " + " ".join(f"{k}x{v}" for k, v in sorted(ops.items(), key=lambda kv: -kv[1])))
            elif f.get("state") == "unreadable":
                lines.append(f"  IB {f['index']}: {ip} va {f['va']} {f['bytes']} bytes (unreadable when traced)")
            else:
                lines.append(f"  IB {f['index']}: {ip} va {f['va']} {f['bytes']} bytes (its buffer is not CPU-mapped)")
        elif kind == "subib":
            if "data" in f:
                pk = pm4(bytes.fromhex(f["data"]))
                ops = {}
                for o, _ in pk:
                    ops[o] = ops.get(o, 0) + 1
                lines.append(f"    called IB va {f['va']} {f['bytes']} bytes, {len(pk)} packets: "
                             + " ".join(f"{k}x{v}" for k, v in sorted(ops.items(), key=lambda kv: -kv[1])[:12]))
            else:
                lines.append(f"    called IB va {f['va']} {f['bytes']} bytes (not CPU-mapped)")
        elif kind == "bolist":
            lines.append(f"  BO list: {f['n']} buffers")
    return lines


def info_answers(recs):
    """{(query, a0, a1, a2, a3): answer bytes} of the INFO calls that succeeded (the arguments after the query)."""
    out = {}
    for r in recs:
        if r["kind"] != "I" or r["nr"] != 0x45 or r["ret"] != 0 or len(r["in"]) < 16:
            continue
        query = struct.unpack_from("<I", r["in"], 12)[0]
        args = struct.unpack_from("<4I", r["in"], 16) if len(r["in"]) >= 32 else (0, 0, 0, 0)
        for kind, f in r["p"]:
            if kind == "info" and "data" in f:
                out[(query, *args)] = bytes.fromhex(f["data"])
    return out


def read_info(text):
    """A trace's INFO answers, or the "Q <query> a=..,..,..,.. sz=n:hex" lines of a .info reference."""
    if re.search(r"^Q 0x", text, re.M):
        out = {}
        for m in re.finditer(r"^Q (0x[0-9a-f]+) a=(\d+),(\d+),(\d+),(\d+) sz=\d+:([0-9a-f]*)", text, re.M):
            out[(int(m.group(1), 16), *(int(m.group(i)) for i in range(2, 6)))] = bytes.fromhex(m.group(6))
        return out
    return info_answers(parse(text))


def compare_info(mine, ref):
    """Lines: for each query both made, equal or the dword offsets that differ; queries only one side made."""
    lines = []
    for k in sorted(set(mine) | set(ref)):
        q = f"query 0x{k[0]:x} args {k[1]},{k[2]},{k[3]},{k[4]}"
        if k not in ref:
            lines.append(f"{q}: only here ({len(mine[k])} bytes)")
        elif k not in mine:
            lines.append(f"{q}: only in the reference")
        else:
            a, b = mine[k], ref[k]
            n = min(len(a), len(b)) // 4
            diff = [i for i in range(n) if a[i * 4:i * 4 + 4] != b[i * 4:i * 4 + 4]]
            if not diff and len(a) == len(b):
                lines.append(f"{q}: equal")
            else:
                shown = ", ".join(f"dw{i} 0x{struct.unpack_from('<I', a, i * 4)[0]:x}/0x{struct.unpack_from('<I', b, i * 4)[0]:x}" for i in diff[:12])
                lines.append(f"{q}: {len(diff)} dwords differ (here/reference){': ' + shown if shown else ''}"
                             f"{'' if len(a) == len(b) else f'; sizes {len(a)}/{len(b)}'}")
    return lines


def info_differs(lines):
    """The lines of compare_info() where both sides made a query and the answers are not the same."""
    return [l for l in lines if " differ" in l]


def summary(recs):
    counts, failed = {}, {}
    for r in recs:
        if r["kind"] != "I":
            continue
        counts[name(r["nr"])] = counts.get(name(r["nr"]), 0) + 1
        if r["ret"] != 0:
            key = f"{name(r['nr'])} errno={r['errno']}"
            failed[key] = failed.get(key, 0) + 1
    lines = [f"{sum(counts.values())} ioctls: " + " ".join(f"{k}x{v}" for k, v in sorted(counts.items(), key=lambda kv: -kv[1]))]
    lines.append("failed: " + (" ".join(f"{k}x{v}" for k, v in sorted(failed.items())) or "none"))
    cs = cs_list(recs)
    if cs:
        lines += describe_cs(cs[0])
    if any(r["kind"] == "E" for r in recs):
        lines.append(f"trace ended after CS number {[r['cs'] for r in recs if r['kind'] == 'E'][0]}")
    u = [r for r in recs if r["kind"] == "U"]
    if u:
        lines.append(f"unreadable arguments: {len(u)} (" + " ".join(sorted({name(r['nr']) for r in u})) + ")")
    bad = damaged(recs)
    if bad:
        lines.append(f"damaged lines: {len(bad)}; first: line {bad[0]['line']}: {bad[0]['why']}")
    return lines


if __name__ == "__main__":
    try:
        if len(sys.argv) == 3 and sys.argv[1] == "summary":
            recs = parse(open(sys.argv[2], errors="replace").read())
            print("\n".join(summary(recs)))
            sys.exit(1 if damaged(recs) else 0)
        elif len(sys.argv) == 4 and sys.argv[1] == "info":
            texts = [open(a, errors="replace").read() for a in sys.argv[2:4]]
            bad = [len(damaged(parse(t))) for t in texts if not re.search(r"^Q 0x", t, re.M)]
            lines = compare_info(read_info(texts[0]), read_info(texts[1]))
            differ = info_differs(lines)
            print("\n".join(lines))
            print(f"info: {len(lines)} queries, {len(differ)} differ, {sum(bad)} damaged lines")
            sys.exit(1 if differ or sum(bad) else 0)
        else:
            sys.exit(__doc__)
    except OSError as e:
        sys.exit(f"drmtrace.py: {e}")
