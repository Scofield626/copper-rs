#!/usr/bin/env python3
"""Regenerate the checked-in `depot.pgm` demo map.

A 32x32 free grid with two rectangular obstacles that force the planner
around them. Cell value 255 = free, 0 = occupied.
"""

from pathlib import Path

WIDTH, HEIGHT = 32, 32


def main() -> None:
    cells = bytearray([255]) * (WIDTH * HEIGHT)
    for y in range(6, 18):
        for x in range(10, 14):
            cells[y * WIDTH + x] = 0
    for y in range(14, 26):
        for x in range(18, 22):
            cells[y * WIDTH + x] = 0
    out = Path(__file__).with_name("depot.pgm")
    header = f"P5\n# {WIDTH}x{HEIGHT} demo map with two rectangular obstacles\n{WIDTH} {HEIGHT}\n255\n".encode()
    out.write_bytes(header + bytes(cells))


if __name__ == "__main__":
    main()
