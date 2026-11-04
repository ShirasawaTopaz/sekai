"""sekai-worker stdio 模式端到端烟雾测试。

喂入：一条 Task(echo) Stdin 帧 + 一条非法帧（kind=9）。
期待：stdout 输出 Hello(boot=42) → Accepted/Started/Completed，进程正常退出。
"""
import struct
import subprocess
import sys
import time


def stdin_frame(cmd_id, name, args):
    payload = struct.pack(">H", len(name)) + name.encode() + args
    head = struct.pack(">BBQQII", 1, 1, cmd_id, int(time.time() * 1000), 0, 500)
    frame = head + struct.pack(">I", len(payload)) + payload
    return struct.pack(">I", len(frame)) + frame


wire = stdin_frame(1, "echo", b"smoke")
bad = (
    bytes([1, 9])
    + struct.pack(">QQII", 2, int(time.time() * 1000), 0, 500)
    + struct.pack(">I", 1)
    + b"x"
)
wire += struct.pack(">I", len(bad)) + bad

exe = sys.argv[1]
proc = subprocess.run(
    [exe, "--mode", "stdio", "--worker-id", "smoke-1", "--boot-id", "42"],
    input=wire,
    capture_output=True,
    timeout=30,
)
print("exit:", proc.returncode)
stderr = proc.stderr.decode("utf-8", "replace").strip()
if stderr:
    print("stderr:", stderr[:300])

out = proc.stdout
frames = []
i = 0
while i + 4 <= len(out):
    (n,) = struct.unpack(">I", out[i : i + 4])
    frames.append(out[i + 4 : i + 4 + n])
    i += 4 + n

phases = []
for f in frames:
    phase = f[1]
    boot, cmd, seq = struct.unpack(">QQI", f[2:22])
    phases.append((phase, boot, cmd, seq))
    if phase == 4:
        print("Completed payload:", f[34:])
print("frames (phase, boot, cmd, seq):", phases)

assert phases[0][0] == 6 and phases[0][1] == 42, "首帧应为 Hello(boot=42)"
assert [p[0] for p in phases[1:]] == [1, 2, 4], "应为 Accepted/Started/Completed"
assert phases[-1][3] == 4, "seq 应为 4"
print("SMOKE OK")
