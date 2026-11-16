"""A mini QMP client for VM tests: mouse and keyboard input, screenshots.

Example: from qmp import Qmp; q = Qmp("target/qmp.sock"); q.keys("help\n"); q.shot("s.png")
"""
import json, socket, sys, time

class Qmp:
    def __init__(self, path):
        self.s = socket.socket(socket.AF_UNIX)
        self.s.connect(path)
        self.f = self.s.makefile('rw')
        self.f.readline()
        self.cmd('qmp_capabilities')

    def cmd(self, name, **args):
        self.f.write(json.dumps({'execute': name, 'arguments': args}) + '\n')
        self.f.flush()
        while True:
            r = json.loads(self.f.readline())
            if 'return' in r or 'error' in r:
                return r

    def move(self, x, y, w=1280, h=800):
        ev = [{'type': 'abs', 'data': {'axis': 'x', 'value': int(x * 32767 / (w - 1))}},
              {'type': 'abs', 'data': {'axis': 'y', 'value': int(y * 32767 / (h - 1))}}]
        self.cmd('input-send-event', events=ev)
        time.sleep(0.08)

    def button(self, down, btn='left'):
        self.cmd('input-send-event', events=[{'type': 'btn', 'data': {'down': down, 'button': btn}}])
        time.sleep(0.08)

    def wheel(self, clicks):
        """Wheel scrolling: clicks > 0 — down, < 0 — up."""
        btn = 'wheel-down' if clicks > 0 else 'wheel-up'
        for _ in range(abs(clicks)):
            self.button(True, btn)
            self.button(False, btn)

    def click(self, x, y):
        self.move(x, y); self.button(True); self.button(False)

    def drag(self, x0, y0, x1, y1, steps=10, delay=0.0):
        """Dragging; delay — the pause between steps (0.016 ≈ a live hand, 60 events per second)."""
        self.move(x0, y0); self.button(True)
        for i in range(1, steps + 1):
            self.move(x0 + (x1 - x0) * i // steps, y0 + (y1 - y0) * i // steps)
            if delay:
                time.sleep(delay)
        self.button(False)

    PLAIN = {' ': 'spc', '\n': 'ret', '.': 'dot', '/': 'slash', '-': 'minus', ',': 'comma',
             '=': 'equal', ';': 'semicolon', "'": 'apostrophe', '\b': 'backspace', '\x1b': 'esc',
             '[': 'bracket_left', ']': 'bracket_right', '\\': 'backslash', '`': 'grave_accent', '\t': 'tab'}
    SHIFTED = {'>': 'dot', '<': 'comma', '_': 'minus', '!': '1', '?': 'slash', ':': 'semicolon',
               '+': 'equal', '*': '8', '{': 'bracket_left', '}': 'bracket_right', '#': '3', '(': '9',
               ')': '0', '"': 'apostrophe', '$': '4', '%': '5', '^': '6', '&': '7', '@': '2',
               '~': 'grave_accent', '|': 'backslash'}

    def key_event(self, qcode, down):
        """Press or release a key separately (for example, to hold Alt)."""
        self.cmd('input-send-event', events=[{'type': 'key', 'data': {'down': down, 'key': {'type': 'qcode', 'data': qcode}}}])
        time.sleep(0.05)

    def key(self, *qcodes):
        self.cmd('send-key', keys=[{'type': 'qcode', 'data': k} for k in qcodes])
        time.sleep(0.05)

    def keys(self, text):
        for ch in text:
            if ch in self.SHIFTED:
                self.key('shift', self.SHIFTED[ch])
            elif ch.isupper():
                self.key('shift', ch.lower())
            else:
                self.key(self.PLAIN.get(ch, ch))

    def shot(self, path):
        from PIL import Image
        # Without a connected VNC client QEMU does not update the screen by itself: the first screenshot
        # makes it update, the second takes the fresh picture.
        self.cmd('screendump', filename=path + '.ppm')
        time.sleep(0.2)
        self.cmd('screendump', filename=path + '.ppm')
        time.sleep(0.3)
        Image.open(path + '.ppm').save(path)
