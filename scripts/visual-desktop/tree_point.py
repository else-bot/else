"""Translate GPUI's AT-SPI screenshot-pixel bounds into omabox layout coordinates."""
import re
import sys
name=sys.argv[1]
scale=float(sys.argv[2])
for line in sys.stdin:
    if f'"{name}"' not in line: continue
    match=re.search(r'@(-?\d+),(-?\d+) (\d+)x(\d+)',line)
    if match:
        x,y,w,h=map(int,match.groups())
        print(round((x+w/2)/scale),round((y+h/2)/scale))
        sys.exit(0)
sys.exit(1)
