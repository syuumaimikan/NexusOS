"""Check real screenshots for window controls and the floating desktop Dock."""
from pathlib import Path
import sys


def load(path):
    with path.open('rb') as stream:
        assert stream.readline().strip() == b'P6'
        line = stream.readline()
        while line.startswith(b'#'):
            line = stream.readline()
        width, height = map(int, line.split())
        assert stream.readline().strip() == b'255'
        pixels = stream.read()
    assert len(pixels) == width * height * 3
    def pixel(x, y):
        at = (y * width + x) * 3
        return tuple(pixels[at:at + 3])
    return width, height, pixel


root = Path(sys.argv[1])
frames = {name: load(root / (name + '.ppm')) for name in ('desktop', 'maximised', 'restored', 'minimised', 'dock-restored', 'closed')}
for width, height, _ in frames.values():
    assert (width, height) == (1920, 1200), 'Unexpected test display geometry'
normal = frames['desktop'][2]
green = (92, 200, 138)
inactive = (155, 167, 186)
assert normal(559, 223) in (green, inactive), 'Green window control is not visible'
assert normal(527, 223) in ((238, 106, 99), inactive), 'Close control is not visible'
assert normal(720, 1138) == (104, 104, 232), 'Dock launcher icon is not visible'
maximised = frames['maximised'][2]
assert maximised(65, 31) == green, 'Window did not move to its maximised location'
assert maximised(559, 223) != normal(559, 223), 'Old title is still in place after maximising'
for name in ('restored', 'dock-restored'):
    assert frames[name][2](559, 223) == (92, 200, 138), 'Restore did not bring the actual window back'
for name in ('minimised', 'closed'):
    assert frames[name][2](559, 223) != (92, 200, 138), 'Window is still shown after hiding or closing'
print('PASS: screenshot pixels prove maximise, restore, minimise, Dock restore and close')
