#!/usr/bin/env python3
"""GM-1d: vkdiff.py and vk_check.py's picture reader on made-up input (30-drmtrace-tools.sh runs this; no QEMU, no GPU)."""
import os
import struct
import sys
import tempfile
import types
import zlib

sys.dont_write_bytecode = True
HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
sys.path.insert(0, os.path.join(HERE, "..", "..", "minipc"))
import vkdiff  # noqa: E402
import vk_check  # noqa: E402

FULL = """Device Properties and Extensions:
=================================
GPU0:
VkPhysicalDeviceProperties:
---------------------------
	apiVersion        = 1.4.354 (4211042)
	vendorID          = 0x1002
	deviceID          = 0x1900
	deviceType        = PHYSICAL_DEVICE_TYPE_INTEGRATED_GPU
	deviceName        = AMD Radeon 780M Graphics (RADV PHOENIX)
VkPhysicalDeviceDriverProperties:
---------------------------------
	driverID   = DRIVER_ID_MESA_RADV
	driverName = radv
	driverInfo = Mesa 26.1.6-arch1.1

Device Extensions: count = 2
	VK_KHR_swapchain      : extension revision 70
	VK_KHR_video_queue    : extension revision 8

VkQueueFamilyProperties:
========================
	queueProperties[0]:
	-------------------
		minImageTransferGranularity = (1,1,1)
		queueCount                  = 1
		queueFlags                  = QUEUE_GRAPHICS_BIT | QUEUE_COMPUTE_BIT
		timestampValidBits          = 64
	queueProperties[1]:
	-------------------
		minImageTransferGranularity = (1,1,1)
		queueCount                  = 1
		queueFlags                  = QUEUE_VIDEO_DECODE_BIT_KHR
		timestampValidBits          = 0

VkPhysicalDeviceMemoryProperties:
=================================
memoryHeaps: count = 2
	memoryHeaps[0]:
		size   = 5638303744 (0x15011b000) (5.25 GiB)
		budget = 5604556800 (0x14e0ec000) (5.22 GiB)
		usage  = 0 (0x00000000) (0.00 B)
		flags:
			None
	memoryHeaps[1]:
		size   = 11276615680 (0x2a0238000) (10.50 GiB)
		budget = 11209105408 (0x29c1d6000) (10.44 GiB)
		usage  = 0 (0x00000000) (0.00 B)
		flags: count = 1
			MEMORY_HEAP_DEVICE_LOCAL_BIT
memoryTypes: count = 2
	memoryTypes[0]:
		heapIndex     = 1
		propertyFlags = 0x0001: count = 1
			MEMORY_PROPERTY_DEVICE_LOCAL_BIT
	memoryTypes[1]:
		heapIndex     = 0
		propertyFlags = 0x0006: count = 2
			MEMORY_PROPERTY_HOST_VISIBLE_BIT
"""
SUMMARY = """Devices:
========
GPU0:
	apiVersion         = 1.4.354
	vendorID           = 0x1002
	deviceID           = 0x1900
	deviceType         = PHYSICAL_DEVICE_TYPE_INTEGRATED_GPU
	deviceName         = AMD Radeon 780M Graphics (RADV PHOENIX)
	driverID           = DRIVER_ID_MESA_RADV
	driverName         = radv
	driverInfo         = Mesa 26.2.5-arch1.1
"""
DEQP = """Test case 'dEQP-VK.info.build'..
  Pass (Not validated)
Test case 'dEQP-VK.api.smoke.triangle'..
  Fail (Image comparison failed) at x.cpp:12
Test case 'dEQP-VK.compute.pipeline.basic.empty_shader'..
  NotSupported (no) at y.cpp:1
Test case 'dEQP-VK.synchronization.basic.fence.one'..
radv/amdgpu: The CS has been cancelled because the context is lost. This context is innocent.
  DeviceLost (queueSubmit: VK_ERROR_DEVICE_LOST at x.cpp:137)
Test case 'dEQP-VK.memory.allocation.basic.size_64.forward.count_1'..
"""
fails = 0


def check(cond, what):
    global fails
    print(("ok: " if cond else "FAIL: ") + what)
    fails += 0 if cond else 1


p = vkdiff.parse_full(FULL)
check(p["identity"]["deviceName"].startswith("AMD Radeon 780M") and p["identity"]["driverID"] == "DRIVER_ID_MESA_RADV", "identity of a full run")
check(p["heaps"] == [(5638303744, "none"), (11276615680, "MEMORY_HEAP_DEVICE_LOCAL_BIT")], f"heaps {p['heaps']}")
check(p["types"] == [(1, 1), (0, 6)], f"types {p['types']}")
d = vkdiff.compare_types([(1, 1), (1, 33), (0, 6)], [(0, 6), (1, 1), (1, 1)])
check(d == [("type heap=1 flags=0x0001", "1", "2"), ("type heap=1 flags=0x0021", "1", "absent")], f"memory types by what they are, not their place: {d}")
check(len(p["queues"]) == 2 and p["queues"][0][1:] == (1, 64) and "QUEUE_GRAPHICS_BIT" in p["queues"][0][0], f"queues {p['queues']}")
check(p["extensions"] == {"VK_KHR_swapchain": 70, "VK_KHR_video_queue": 8}, f"extensions {p['extensions']}")
ref = vkdiff.read_ref(vkdiff.write_ref(p))
check(ref == p, "a reference reads back as it was written")
check(not any(vkdiff.compare_all(p, ref).values()), "a run against its own reference: no differences")
check(vkdiff.devices(SUMMARY) == 1 and not vkdiff.compare_identity(vkdiff.identity(SUMMARY), ref["identity"]), "--summary's identity against the reference, its version ignored")
check(vkdiff.compare_identity({"deviceName": "llvmpipe"}, {"deviceName": "x"}) == [("identity deviceName", "llvmpipe", "x")], "another device is a difference")

other = {**p, "heaps": [(5638303744 * 3, "none"), (11276615680, "x")]}
d = vkdiff.compare_heaps(other["heaps"], p["heaps"])
check(("heap 0 size", str(5638303744 * 3), "5638303744") in d and ("heap 1 flags", "x", "MEMORY_HEAP_DEVICE_LOCAL_BIT") in d and len(d) == 2, f"heap size past the ratio and heap flags: {d}")
check(not vkdiff.compare_heaps([(int(5638303744 * 1.5), "none"), (11276615680, "MEMORY_HEAP_DEVICE_LOCAL_BIT")], p["heaps"]), "a heap 1.5 times the size is the same order")
d = vkdiff.compare_queues(p["queues"][:1], p["queues"])
check(d == [("queue QUEUE_VIDEO_DECODE_BIT_KHR", "absent", "count=1 ts=0")], f"a missing family shifts nothing: {d}")
d = vkdiff.compare_extensions({"VK_KHR_swapchain": 70, "VK_NEW": 1}, p["extensions"])
check(("ext VK_NEW", "1", "absent") in d and ("ext VK_KHR_video_queue", "absent", "8") in d and len(d) == 2, f"extensions both ways: {d}")
allow = vkdiff.read_allow("# c\next VK_KHR_video_* | absent | * | VCN later\next VK_GONE | absent | 1 | old\n")
new, known, unused = vkdiff.split_allowed(d, allow)
check(new == [("ext VK_NEW", "1", "absent")] and known[0][3] == "VCN later" and unused == ["ext VK_GONE"], f"allow patterns: {new} {known} {unused}")

# The real files: one compute queue on the hardware backend against the capture's four is known, with its reason; another count is new.
ref_dir = os.path.join(HERE, "..", "ref-780m")
real = vkdiff.read_ref(open(os.path.join(ref_dir, "vulkan.ref")).read())
real_allow = vkdiff.read_allow(open(os.path.join(ref_dir, "vulkan-allow.txt")).read())
COMPUTE = "QUEUE_COMPUTE_BIT QUEUE_SPARSE_BINDING_BIT QUEUE_TRANSFER_BIT"
for count, is_known in ((1, True), (2, False)):
    mine = [(f, count if f == COMPUTE else c, t) for f, c, t in real["queues"]]
    new, known, _ = vkdiff.split_allowed(vkdiff.compare_queues(mine, real["queues"]), real_allow)
    check((len(known), len(new)) == ((1, 0) if is_known else (0, 1)), f"{count} compute queues: known {known}, new {new}")
    check(not is_known or "one kernel compute ring" in known[0][3], f"the reason is named: {known}")

r = vkdiff.parse_deqp(DEQP)
check(r == {"dEQP-VK.info.build": "Pass", "dEQP-VK.api.smoke.triangle": "Fail", "dEQP-VK.compute.pipeline.basic.empty_shader": "NotSupported",
            "dEQP-VK.synchronization.basic.fence.one": "DeviceLost", "dEQP-VK.memory.allocation.basic.size_64.forward.count_1": "Died"}, f"deqp statuses, a case without a result is Died: {r}")
want = {"dEQP-VK.info.build": "Pass", "dEQP-VK.api.smoke.triangle": "Pass", "dEQP-VK.api.smoke.compute": "Pass", "dEQP-VK.compute.pipeline.basic.empty_shader": "NotSupported"}
d = vkdiff.compare_deqp(r, want)
check(("dEQP-VK.api.smoke.triangle", "Fail", "Pass") in d and ("dEQP-VK.api.smoke.compute", "not run", "Pass") in d and ("dEQP-VK.memory.allocation.basic.size_64.forward.count_1", "Died", "not in the reference") in d and len(d) == 4, f"deqp against a reference: {d}")
check(not vkdiff.compare_deqp({"a": "QualityWarning"}, {"a": "Pass"}), "a QualityWarning counts as its Pass")
check(vkdiff.compare_deqp({"a": "Pass"}, {"a": "NotSupported"}) == [("a", "Pass", "NotSupported")], "a Pass where the reference has NotSupported is a difference: the device is not the one captured")
d = vkdiff.compare_deqp_support({"a": "Fail", "b": "NotSupported", "c": "Pass", "e": "Died"}, {"a": "Pass", "b": "Pass", "c": "NotSupported", "d": "Pass", "e": "Pass"})
check(sorted(x[0] for x in d) == ["b", "c", "d", "e"], f"on a node that cannot draw a Fail is fine, support and a dead program are not: {d}")
check(vkdiff.group("dEQP-VK.info.build") == "info" and vkdiff.group("dEQP-VK.api.smoke.triangle") == "api.smoke", "step names of the deqp groups")


def png(w, h, rows):
    """An 8-bit RGB PNG with each row filtered by the type given with it."""
    def chunk(k, b):
        return struct.pack(">I", len(b)) + k + b + struct.pack(">I", zlib.crc32(k + b))
    raw = b"".join(bytes([t]) + r for t, r in rows)
    return b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", struct.pack(">IIBBBBB", w, h, 8, 2, 0, 0, 0)) + chunk(b"IDAT", zlib.compress(raw)) + chunk(b"IEND", b"")


with tempfile.TemporaryDirectory() as d:
    f = os.path.join(d, "a.png")
    # Row 0 unfiltered, row 1 Sub, row 2 Up: decoded to the same two pixels (1,2,3) (11,12,13).
    open(f, "wb").write(png(2, 3, [(0, bytes([1, 2, 3, 11, 12, 13])), (1, bytes([1, 2, 3, 10, 10, 10])), (2, bytes(6))]))
    w, h, bpp, rows = vk_check.read_png(f)
    check((w, h, bpp) == (2, 3, 3) and all(r == bytes([1, 2, 3, 11, 12, 13]) for r in rows), f"PNG filters none, Sub, Up: {rows}")
    h0, c0 = vk_check.crop_hash(f, (0, 0, 1, 3))
    h1, c1 = vk_check.crop_hash(f, (1, 0, 5, 9))
    check(h0 != h1 and c0 == 1, "a crop is clipped to the picture and hashes only its own pixels")
    open(f, "wb").write(b"not a png")
    try:
        vk_check.read_png(f)
        check(False, "garbage is refused")
    except ValueError:
        check(True, "garbage is refused")


class Stub(vk_check.Check):
    """Check without a node: `ctl` and `run` answer from the test; `read_file` hands out the test's files."""

    def __init__(self, out, profile="real"):
        self.a = types.SimpleNamespace(out=out, profile=profile, steps="deqp", target="stub")
        self.pr = vk_check.PROFILES[profile]
        self.passed, self.failed = 0, 0
        self.env = {}
        self.ref_dir = None
        self.lines, self.ctls, self.files, self.ran, self.ends = [], [], {}, [], {}
        self.vkcube_text, self.vkcube = "pid 7 ended: exit 0", 1

    def step(self, name, ok, detail=""):
        self.lines.append((name, ok, detail))
        return super().step(name, ok, detail)

    def ctl(self, *words):
        self.ctls.append(words[0])
        return self.list_text

    def run(self, path, args, env, secs):
        self.ran.append(args)
        for c, end in self.ends.items():
            if f"--deqp-case={c} " in args[-1]:
                self.files["out"] += end[2]
                return end
        return "exit", 0, ""

    def mcp(self, tool, args=None, png=None, save=None, wait=None):
        body = self.files.get("subset" if args["path"].endswith("subset.txt") else "out")
        open(save, "w").write(body)
        return False, ""


LIST = """tile=0 page=0/1 visible=true focused=false frame=0,0 800x600 id=1 app_id=panel title="p" pid=30 hung=false ws=1
tile=1 page=0/1 visible=true focused=true frame=800,0 800x600 id=2 app_id=note title="n" pid=31 hung=false ws=1
tile=2 page=0/1 visible=true focused=false frame=0,600 800x600 id=3 app_id= title="vkcube" pid=32 hung=false ws=1
"""
line = vk_check.find_window(LIST, {1}, 32)
check(line is not None and " id=3 " in line, f"another new window is not vkcube's: {line}")
check(vk_check.find_window(LIST, {1, 3}, 32) is None, "vkcube's window must be new")
check(vk_check.find_window(LIST, {1, 2}, 99) is None, "a known pid decides, not the title")
check(" id=3 " in (vk_check.find_window(LIST, {1, 2}, None) or ""), "without a pid the title decides")

with tempfile.TemporaryDirectory() as d:
    c = Stub(d)
    c.list_text = LIST
    c.close(line)
    # The window is still listed by the stub, so the step fails on "gone"; what counts here is who was closed: by id,
    # whatever has the focus (CR-GM1d-5).
    check("close 3" in c.ctls, f"vkcube's window is closed by its id: {c.ctls}")

    # Render nodes: the sorted `ls /dev/dri` of `run --check`.
    def tree_with(text):
        c = Stub(d)
        c.run = lambda *a: ("exit", 0, "mesa: libraries: 9 programs and libraries resolve inside /l\n" + text)
        c.tree()
        return c.failed == 0
    check(tree_with("mesa: render nodes: renderD128 \n"), "render node alone")
    check(tree_with("mesa: render nodes: by-path card0 renderD128 \n"), "render node beside a primary node")
    check(not tree_with("mesa: render nodes: none (/dev/dri missing)\n"), "no render node is a FAIL")
    check(not tree_with("mesa: render nodes: card0 \n"), "only a primary node is a FAIL")

    # A case that dies, one that gets stuck: named, and the cases after them still run.
    c = Stub(d)
    c.files = {"subset": "dEQP-VK.a.x\ndEQP-VK.a.y\ndEQP-VK.a.z\ndEQP-VK.a.w\n", "out": ""}
    tc = "Test case '%s'..\n"
    c.ends = {"dEQP-VK.a.x": ("exit", 0, tc % "dEQP-VK.a.x" + "  Pass (ok)\n"),
              "dEQP-VK.a.y": ("signal", 11, tc % "dEQP-VK.a.y"),
              "dEQP-VK.a.z": (None, None, tc % "dEQP-VK.a.z"),
              "dEQP-VK.a.w": ("exit", 1, tc % "dEQP-VK.a.w" + "  Fail (bad)\n")}
    mine, troubles = c.run_cases("radeon_icd", "deqp")
    check(mine == {"dEQP-VK.a.x": "Pass", "dEQP-VK.a.y": "Died", "dEQP-VK.a.z": "Died", "dEQP-VK.a.w": "Fail"}, f"each case's result: {mine}")
    check(list(troubles) == ["dEQP-VK.a.y", "dEQP-VK.a.z"] and "signal 11" in troubles["dEQP-VK.a.y"] and "stuck" in troubles["dEQP-VK.a.z"], f"killed and stuck are named, a failed case is not: {troubles}")
    check(all("exec /bin/sh" in a[-1] for a in c.ran), "MCP's stop must reach deqp-vk itself")

    # The node's work folder goes in any case; a second Unreachable there changes nothing.
    c = Stub(d)
    c.a.steps = "boom"
    check(c.main() == 1 and c.ran[-1][-1].startswith("rm -rf"), "an unknown step is a FAIL, the work folder is removed")
    c = Stub(d)
    c.a.steps = "tree"
    c.tree = lambda: (_ for _ in ()).throw(vk_check.Unreachable("gone"))
    c.run = lambda *a: (_ for _ in ()).throw(vk_check.Unreachable("gone"))
    check(c.main() == 1 and c.lines[-1][0] == "mcp" and not c.lines[-1][1], "no answer: FAIL mcp, the cleanup's second Unreachable is swallowed")
print(f"test_vkdiff: {'RED ' + str(fails) if fails else 'GREEN'}")
sys.exit(1 if fails else 0)
