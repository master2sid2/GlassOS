"""Helpers for testing GlassOS in the test VM (NAME=test ./run.sh).

    exec(open('tools/vmlib.py').read())   # from the repository root
    shot('name')         # a screenshot to target/shots/name.png
    term('ls')           # type a command and Enter
    dbl(x, y)            # a double click (faster than two clicks)
    rclick(x, y)         # a right click with the mouse moved there first
    quit_vm()            # close the test VM

Coordinates are in pixels of a 1280×800 screen.
"""
import os
import sys
import time

sys.path.insert(0, 'tools')
from qmp import Qmp  # noqa: E402

SOCK = os.environ.get('VM_QMP', 'target/test-qmp.sock')
SHOTS = 'target/shots'
os.makedirs(SHOTS, exist_ok=True)
q = Qmp(SOCK)


def shot(name):
    path = f'{SHOTS}/{name}.png'
    q.shot(path)
    return path


def term(cmd, wait=0.6):
    q.keys(cmd + '\n')
    time.sleep(wait)


def dbl(x, y):
    q.move(x, y)
    time.sleep(0.3)
    for _ in range(2):
        q.cmd('input-send-event', events=[{'type': 'btn', 'data': {'down': True, 'button': 'left'}}])
        q.cmd('input-send-event', events=[{'type': 'btn', 'data': {'down': False, 'button': 'left'}}])
        time.sleep(0.03)


def rclick(x, y):
    q.move(x - 3, y - 1)
    time.sleep(0.3)
    q.move(x, y)
    time.sleep(0.3)
    q.button(True, 'right')
    time.sleep(0.1)
    q.button(False, 'right')
    time.sleep(1.0)


def quit_vm():
    q.cmd('quit')
