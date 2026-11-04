"""Generate assets/app.ico: a small fractal tree on a dark rounded square. No dependencies."""
import math
import os
import struct

SS = 768  # master size; every icon size is box-filtered down from this


def render_master():
    px = bytearray(SS * SS * 4)  # BGRA

    # Rounded square background.
    radius = SS * 0.2
    for y in range(SS):
        for x in range(SS):
            dx = max(radius - x, x - (SS - 1 - radius), 0)
            dy = max(radius - y, y - (SS - 1 - radius), 0)
            if dx * dx + dy * dy <= radius * radius:
                i = (y * SS + x) * 4
                px[i:i + 4] = bytes((0x33, 0x2A, 0x1F, 255))

    def stamp(cx, cy, r, color):
        for y in range(max(0, int(cy - r)), min(SS, int(cy + r) + 1)):
            for x in range(max(0, int(cx - r)), min(SS, int(cx + r) + 1)):
                if (x - cx) ** 2 + (y - cy) ** 2 <= r * r:
                    i = (y * SS + x) * 4
                    px[i:i + 4] = color

    def branch(x, y, angle, length, depth):
        nx, ny = x + math.cos(angle) * length, y + math.sin(angle) * length
        width = SS * (0.008 + depth * 0.0075)
        t = depth / 6
        color = bytes((int(0x70 + 0x20 * (1 - t)), int(0xC8 + 0x30 * (1 - t)), int(0x60 + 0x30 * (1 - t)), 255))
        steps = int(length / max(width * 0.5, 1)) + 1
        for s in range(steps + 1):
            k = s / steps
            stamp(x + (nx - x) * k, y + (ny - y) * k, width, color)
        if depth > 0:
            branch(nx, ny, angle - 0.52, length * 0.72, depth - 1)
            branch(nx, ny, angle + 0.52, length * 0.72, depth - 1)

    branch(SS * 0.5, SS * 0.90, -math.pi / 2, SS * 0.21, 6)
    return px


def downsample(px, size):
    k = SS / size
    out = bytearray(size * size * 4)
    for y in range(size):
        y0, y1 = int(y * k), max(int(y * k) + 1, int((y + 1) * k))
        for x in range(size):
            x0, x1 = int(x * k), max(int(x * k) + 1, int((x + 1) * k))
            b = g = r = a = n = 0
            for yy in range(y0, y1):
                row = yy * SS
                for xx in range(x0, x1):
                    i = (row + xx) * 4
                    alpha = px[i + 3]
                    b += px[i] * alpha
                    g += px[i + 1] * alpha
                    r += px[i + 2] * alpha
                    a += alpha
                    n += 1
            j = (y * size + x) * 4
            if a:
                out[j:j + 4] = bytes((b // a, g // a, r // a, a // n))
    return out


def ico_entry(rgba, size):
    # DIB in an ICO: bottom-up BGRA, double height, then a 1-bit AND mask.
    header = struct.pack('<IiiHHIIiiII', 40, size, size * 2, 1, 32, 0, size * size * 4, 0, 0, 0, 0)
    rows = b''.join(bytes(rgba[y * size * 4:(y + 1) * size * 4]) for y in range(size - 1, -1, -1))
    mask = bytes(((size + 31) // 32 * 4) * size)
    return header + rows + mask


def main():
    root = os.path.join(os.path.dirname(os.path.abspath(__file__)), '..', 'assets')
    os.makedirs(root, exist_ok=True)
    master = render_master()
    sizes = [16, 20, 24, 32, 40, 48, 64, 128, 256]
    images = [ico_entry(downsample(master, s), s) for s in sizes]
    out = struct.pack('<HHH', 0, 1, len(sizes))
    offset = 6 + 16 * len(sizes)
    for s, img in zip(sizes, images):
        out += struct.pack('<BBBBHHII', 0 if s == 256 else s, 0 if s == 256 else s, 0, 0, 1, 32, len(img), offset)
        offset += len(img)
    out += b''.join(images)
    with open(os.path.join(root, 'app.ico'), 'wb') as f:
        f.write(out)
    print('wrote', len(out), 'bytes')


if __name__ == '__main__':
    main()
