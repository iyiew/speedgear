"""生成 icon.ico（齿轮图案）。只用标准库：python make_icon.py"""
import math
import struct
import zlib
from pathlib import Path

SIZE = 256
TEETH = 8
ACCENT = (245, 166, 35)


def coverage(x: float, y: float) -> float:
    """点 (x, y) 是否在齿轮形状内，坐标以图标中心为原点、半径归一化到 1。"""
    r = math.hypot(x, y)
    if r < 0.26:
        return 0.0
    angle = math.atan2(y, x)
    tooth = math.cos(angle * TEETH) > 0.15
    return 1.0 if r < (0.92 if tooth else 0.70) else 0.0


def pixel(px: int, py: int) -> bytes:
    # 4x4 超采样抗锯齿
    total = 0.0
    for sy in range(4):
        for sx in range(4):
            x = ((px + (sx + 0.5) / 4) / SIZE) * 2 - 1
            y = ((py + (sy + 0.5) / 4) / SIZE) * 2 - 1
            total += coverage(x, y)
    return bytes((*ACCENT, round(total / 16 * 255)))


def png() -> bytes:
    raw = b"".join(
        b"\x00" + b"".join(pixel(x, y) for x in range(SIZE)) for y in range(SIZE)
    )

    def chunk(kind: bytes, data: bytes) -> bytes:
        body = kind + data
        return struct.pack(">I", len(data)) + body + struct.pack(">I", zlib.crc32(body))

    header = struct.pack(">IIBBBBB", SIZE, SIZE, 8, 6, 0, 0, 0)
    return (
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", header)
        + chunk(b"IDAT", zlib.compress(raw, 9))
        + chunk(b"IEND", b"")
    )


def main() -> None:
    image = png()
    # ICO 目录：一张 256x256 的 PNG（宽高字段 0 表示 256）
    ico = struct.pack("<HHH", 0, 1, 1)
    ico += struct.pack("<BBBBHHII", 0, 0, 0, 0, 1, 32, len(image), 22)
    Path(__file__).with_name("icon.ico").write_bytes(ico + image)


if __name__ == "__main__":
    main()
