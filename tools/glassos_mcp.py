#!/usr/bin/env python3
"""An MCP server (Model Context Protocol, stdio transport) for a GlassOS machine.

It exposes the remote access API (docs/remote-protocol.md) as tools an AI assistant can
call: run terminal commands, read the kernel log, read and write files, take screenshots,
drive the keyboard and mouse, install a trial kernel and reboot.

The machine and its key come from the same places as for tools/remote.py:
the GLASSOS_HOST and GLASSOS_KEY environment variables, or ~/.config/glassos/remote.cfg
written by `remote.py setup HOST KEYFILE`.

Example client configuration (the exact format depends on the client):

    {"mcpServers": {"glassos": {"command": "python3",
                                "args": ["/path/to/glassos/tools/glassos_mcp.py"],
                                "env": {"GLASSOS_HOST": "192.0.2.10"}}}}

Only the Python standard library is used. See docs/ai-agents.md for how to work with it.
"""
import base64
import json
import os
import struct
import sys
import time
import zlib

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import remote  # noqa: E402  (tools/remote.py)

PROTOCOL_VERSIONS = ('2025-06-18', '2025-03-26', '2024-11-05')
MAX_TEXT = 200_000  # characters of a text answer before it is cut


def machine():
    cfg = remote.load_config()
    host = os.environ.get('GLASSOS_HOST') or cfg.get('host')
    name = (host or '').partition(':')[0]
    key = os.environ.get('GLASSOS_KEY') or cfg.get(f'key@{name}') or cfg.get('key')
    if not host or not key:
        raise RuntimeError('no machine configured: set GLASSOS_HOST and GLASSOS_KEY or run remote.py setup HOST KEYFILE')
    return remote.Machine(host, remote.parse_key(key))


def text(s):
    s = s if isinstance(s, str) else s.decode(errors='replace')
    if len(s) > MAX_TEXT:
        s = s[:MAX_TEXT] + f'\n… [cut: {len(s) - MAX_TEXT} more characters]'
    return [{'type': 'text', 'text': s}]


def schema(props=None, required=()):
    return {'type': 'object', 'properties': props or {}, 'required': list(required)}


TOOLS = {}


def tool(name, description, props=None, required=()):
    def wrap(fn):
        TOOLS[name] = {'name': name, 'description': description, 'inputSchema': schema(props, required), 'fn': fn}
        return fn
    return wrap


@tool('glassos_info', 'What machine this is: name, system version, kernel path, whether a trial kernel runs, uptime, screens.')
def t_info(a):
    return text(machine().ok('GET', '/info')[0])


@tool('glassos_run', 'Run a line in the GlassOS terminal (as the administrator who enabled remote access) and return its output. '
      'Use `help` to list commands. Long commands are stopped after `wait` seconds.',
      {'command': {'type': 'string', 'description': 'The terminal line, for example "ls /apps" or "net".'},
       'cwd': {'type': 'string', 'description': 'The folder to run in (default: the home folder).'},
       'wait': {'type': 'integer', 'description': 'Seconds to wait for long commands (default 30, max 600).'}},
      ('command',))
def t_run(a):
    wait = int(a.get('wait', 30))
    query = {'wait': wait}
    if a.get('cwd'):
        query['cwd'] = a['cwd']
    data, meta = machine().ok('POST', '/run', query, a['command'].encode(), timeout=wait + 30)
    out = data.decode(errors='replace')
    if meta.get('cwd'):
        out += f'\n[cwd: {meta["cwd"]}]'
    return text(out)


@tool('glassos_log', 'Read the kernel log. Pass `from` (the position returned last time) to get only new lines.',
      {'from': {'type': 'integer', 'description': 'Log position to start from (0 — everything kept).'},
       'grep': {'type': 'string', 'description': 'Keep only lines containing this text.'}})
def t_log(a):
    start = int(a.get('from', 0))
    data, meta = machine().ok('GET', '/log', {'from': start} if start else None)
    s = data.decode(errors='replace')
    if a.get('grep'):
        s = ''.join(l for l in s.splitlines(True) if a['grep'] in l)
    return text(s + f'\n[next from: {meta.get("log-end", start)}]')


@tool('glassos_list', 'List a folder: one entry per line, folders end with "/".',
      {'path': {'type': 'string', 'description': 'An absolute path, for example /apps or /home/user.'}}, ('path',))
def t_list(a):
    data, _ = machine().ok('GET', '/list', {'path': a['path']})
    lines = []
    for line in data.decode(errors='replace').splitlines():
        kind, size, name = (line.split('\t') + ['', ''])[:3]
        lines.append(f'{name}/' if kind == 'd' else f'{name}  {size} bytes')
    return text('\n'.join(lines) or '(empty)')


@tool('glassos_read_file', 'Read a file. Text is returned as is; binary data as base64. PNG/JPEG files are returned as images.',
      {'path': {'type': 'string'}}, ('path',))
def t_read(a):
    data, _ = machine().ok('GET', '/file', {'path': a['path']}, timeout=300)
    if data[:8] == b'\x89PNG\r\n\x1a\n':
        return [{'type': 'image', 'data': base64.b64encode(data).decode(), 'mimeType': 'image/png'}]
    if data[:3] == b'\xff\xd8\xff':
        return [{'type': 'image', 'data': base64.b64encode(data).decode(), 'mimeType': 'image/jpeg'}]
    try:
        return text(data.decode('utf-8'))
    except UnicodeDecodeError:
        return text(f'[binary, {len(data)} bytes, base64]\n' + base64.b64encode(data).decode())


@tool('glassos_write_file', 'Write a file (created or replaced). Use `base64: true` for binary content.',
      {'path': {'type': 'string'},
       'content': {'type': 'string'},
       'base64': {'type': 'boolean', 'description': 'The content is base64-encoded bytes.'},
       'parents': {'type': 'boolean', 'description': 'Create missing folders.'}},
      ('path', 'content'))
def t_write(a):
    body = base64.b64decode(a['content']) if a.get('base64') else a['content'].encode()
    query = {'path': a['path']}
    if a.get('parents'):
        query['parents'] = 1
    return text(machine().ok('PUT', '/file', query, body, timeout=300)[0])


def shrink_png(png, max_width):
    """The kernel writes uncompressed RGB PNGs without filters (src/png.rs): downscale by an integer
    step to at most `max_width` and compress. Anything else is returned unchanged."""
    pos, chunks = 8, []
    while pos + 8 <= len(png):
        n, kind = struct.unpack('>I4s', png[pos:pos + 8])
        chunks.append((kind, png[pos + 8:pos + 8 + n]))
        pos += 12 + n
    head = dict(chunks).get(b'IHDR')
    if not head:
        return png, None
    w, h, depth, color = struct.unpack('>IIBB', head[:10])
    if depth != 8 or color != 2:
        return png, None
    raw = zlib.decompress(b''.join(d for k, d in chunks if k == b'IDAT'))
    stride = w * 3 + 1
    if any(raw[y * stride] != 0 for y in range(h)):
        return png, None
    step = max(1, -(-w // max_width))
    nw, nh = w // step, h // step
    rows = bytearray()
    for y in range(nh):
        line = raw[y * step * stride + 1:(y * step + 1) * stride]
        rows.append(0)
        if step == 1:
            rows += line
        else:
            for x in range(nw):
                rows += line[x * step * 3:x * step * 3 + 3]

    def chunk(kind, data):
        return struct.pack('>I', len(data)) + kind + data + struct.pack('>I', zlib.crc32(kind + data))
    out = b'\x89PNG\r\n\x1a\n' + chunk(b'IHDR', struct.pack('>IIBBBBB', nw, nh, 8, 2, 0, 0, 0)) \
        + chunk(b'IDAT', zlib.compress(bytes(rows), 6)) + chunk(b'IEND', b'')
    return out, (w, h, nw, nh)


@tool('glassos_screenshot', 'Take a screenshot of a screen (PNG). It is downscaled to at most `max_width` pixels '
      '(1280 by default; 0 keeps the full size). Input coordinates are always full-size screen pixels.',
      {'screen': {'type': 'integer', 'description': 'Screen number, 0 is the primary one.'},
       'max_width': {'type': 'integer', 'description': 'Downscale to at most this width (0 — no downscaling).'}})
def t_shot(a):
    data, _ = machine().ok('GET', '/shot', {'screen': int(a.get('screen', 0))}, timeout=120)
    max_width = int(a.get('max_width', 1280))
    note = []
    if max_width > 0:
        data, size = shrink_png(data, max_width)
        if size and size[0] != size[2]:
            note = text(f'screen {size[0]}x{size[1]}, image {size[2]}x{size[3]}: multiply image coordinates by '
                        f'{size[0] // size[2]} for glassos_input')
    return [{'type': 'image', 'data': base64.b64encode(data).decode(), 'mimeType': 'image/png'}] + note


@tool('glassos_input', 'Send keyboard and mouse input (needs a build with the debug-tools feature and `debug input on` '
      'on the machine). Coordinates are screen pixels as on the screenshot. Actions, one per item: '
      '"text Hello", "key enter", "key ctrl+s", "click X Y", "rclick X Y", "move X Y", "wheel X Y N".',
      {'actions': {'type': 'array', 'items': {'type': 'string'}}}, ('actions',))
def t_input(a):
    return text(machine().ok('POST', '/input', None, '\n'.join(a['actions']).encode())[0])


def wait_for(m, check, seconds):
    end = time.time() + seconds
    while time.time() < end:
        try:
            data, _ = m.ok('GET', '/info', timeout=5)
            s = data.decode(errors='replace')
            if check(s):
                return s
        except Exception:
            pass
        time.sleep(3)
    return None


@tool('glassos_reboot', 'Reboot the machine and wait until it answers again. `into`: normal, trial (boot the uploaded trial '
      'kernel once), bios (open the firmware setup), usb (boot once from a USB stick).',
      {'into': {'type': 'string', 'enum': ['normal', 'trial', 'bios', 'usb']}})
def t_reboot(a):
    m, into = machine(), a.get('into', 'normal')
    first = m.ok('POST', '/reboot', {'into': into})[0].decode(errors='replace')
    if into not in ('normal', 'trial'):
        return text(first)
    time.sleep(10)
    back = wait_for(m, lambda s: True, 300)
    return text(first + ('\n' + back if back else '\nno answer after 5 minutes'))


@tool('glassos_trial_kernel', 'Upload a kernel image (kernel.efi built on this computer) as a trial kernel and boot it once. '
      'If it hangs, a power cycle (or the watchdog) brings back the previous kernel. Use glassos_keep_kernel to keep it.',
      {'efi': {'type': 'string', 'description': 'A local path to kernel.efi (default: the repository build output).'}})
def t_trial(a):
    m = machine()
    with open(a.get('efi') or remote.KERNEL, 'rb') as f:
        image = f.read()
    log = [m.ok('PUT', '/kernel', body=image, timeout=300)[0].decode(errors='replace')]
    log.append(m.ok('POST', '/reboot', {'into': 'trial'})[0].decode(errors='replace'))
    time.sleep(10)
    s = wait_for(m, lambda s: 'trial: yes' in s, 300)
    log.append(s if s else 'the trial kernel did not answer in 5 minutes; power-cycle the machine to get the old kernel back')
    return text('\n'.join(log))


@tool('glassos_keep_kernel', 'Make the running trial kernel the main one (the previous one is kept as OLD.EFI).')
def t_keep(a):
    return text(machine().ok('POST', '/kernel/keep')[0])


def reply(id_, result=None, error=None):
    msg = {'jsonrpc': '2.0', 'id': id_}
    if error:
        msg['error'] = error
    else:
        msg['result'] = result
    sys.stdout.write(json.dumps(msg) + '\n')
    sys.stdout.flush()


def handle(msg):
    method, id_, params = msg.get('method'), msg.get('id'), msg.get('params') or {}
    if id_ is None:
        return  # a notification (initialized, cancelled …)
    if method == 'initialize':
        asked = params.get('protocolVersion')
        version = asked if asked in PROTOCOL_VERSIONS else PROTOCOL_VERSIONS[0]
        reply(id_, {'protocolVersion': version, 'capabilities': {'tools': {}},
                    'serverInfo': {'name': 'glassos', 'version': '0.2'},
                    'instructions': 'Tools for a GlassOS machine over its remote access API. Start with glassos_info, '
                                    'then glassos_run "help". See docs/ai-agents.md in the GlassOS repository.'})
    elif method == 'ping':
        reply(id_, {})
    elif method == 'tools/list':
        reply(id_, {'tools': [{k: v for k, v in t.items() if k != 'fn'} for t in TOOLS.values()]})
    elif method == 'tools/call':
        t = TOOLS.get(params.get('name'))
        if not t:
            reply(id_, error={'code': -32602, 'message': f'unknown tool {params.get("name")}'})
            return
        try:
            reply(id_, {'content': t['fn'](params.get('arguments') or {})})
        except Exception as e:  # a tool error is a result the model should see, not a protocol error
            reply(id_, {'content': text(f'error: {e}'), 'isError': True})
    else:
        reply(id_, error={'code': -32601, 'message': f'unknown method {method}'})


def main():
    for line in sys.stdin:
        line = line.strip()
        if not line:
            continue
        try:
            msg = json.loads(line)
        except json.JSONDecodeError:
            reply(None, error={'code': -32700, 'message': 'parse error'})
            continue
        for m in msg if isinstance(msg, list) else [msg]:
            handle(m)


if __name__ == '__main__':
    main()
