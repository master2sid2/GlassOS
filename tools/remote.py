#!/usr/bin/env python3
"""Remote access to GlassOS (Settings → Remote Access or `remote on`).

Requests are signed with a shared key (HMAC-SHA256), and so are the machine's answers: this script
checks the signature of every answer. The traffic is not encrypted — use it on your own network only.

    remote.py setup HOST KEY|KEYFILE     remember the machine and the key (~/.config/glassos/remote.cfg)
    remote.py info                       what machine this is and which kernel runs
    remote.py run COMMAND...             a terminal command (--cwd, --wait SECONDS)
    remote.py sh                         a terminal: line by line, the folder is remembered
    remote.py log [-f] [--grep WORD]     the kernel log; -f — follow it
    remote.py ls PATH
    remote.py get REMOTE [LOCAL]
    remote.py put LOCAL REMOTE [--parents]
    remote.py shot [FILE.png] [--screen N]
    remote.py kernel [EFI] [--no-start]  a trial kernel: upload it and boot it once
    remote.py keep                       make the running trial kernel the main one
    remote.py reboot [normal|trial|bios|usb]

The machine and the key can also be given as --host HOST[:7070] --key KEY|KEYFILE
or with the GLASSOS_HOST and GLASSOS_KEY environment variables.
"""
import argparse
import base64
import hashlib
import hmac
import http.client
import os
import secrets
import sys
import time
import urllib.parse

CONFIG = os.path.expanduser('~/.config/glassos/remote.cfg')
DEFAULT_PORT = 7070
ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
KERNEL = os.path.join(ROOT, 'target/x86_64-glassos/release/kernel.efi')


class Refused(Exception):
    pass


def parse_key(text):
    """The key: 32 base32 characters (dashes and spaces do not matter) or a file with it."""
    if os.path.isfile(text):
        with open(text) as f:
            text = f.read()
    clean = ''.join(c for c in text.upper() if c.isalnum())
    key = base64.b32decode(clean)
    if len(key) != 20:
        raise ValueError('the key must be 32 base32 characters')
    return key


def load_config():
    cfg = {}
    if os.path.exists(CONFIG):
        with open(CONFIG) as f:
            for line in f:
                if '=' in line:
                    k, v = line.strip().split('=', 1)
                    cfg[k] = v
    return cfg


class Machine:
    def __init__(self, host, key):
        name, _, port = host.partition(':')
        self.host, self.port = name, int(port or DEFAULT_PORT)
        self.key = key
        self.skew = 0

    def request(self, method, path, query=None, body=b'', timeout=60, retry=True):
        target = path
        if query:
            target += '?' + '&'.join(f'{urllib.parse.quote(k, safe="")}={urllib.parse.quote(str(v), safe="")}' for k, v in query.items())
        stamp = str(int(time.time()) + self.skew)
        nonce = secrets.token_hex(16)
        body_hash = hashlib.sha256(body).hexdigest()
        text = f'{method}\n{target}\n{stamp}\n{nonce}\n{body_hash}'.encode()
        headers = {
            'X-Glass-Time': stamp,
            'X-Glass-Nonce': nonce,
            'X-Glass-Body': body_hash,
            'X-Glass-Sign': hmac.new(self.key, text, hashlib.sha256).hexdigest(),
            'Content-Length': str(len(body)),
        }
        conn = http.client.HTTPConnection(self.host, self.port, timeout=timeout)
        try:
            conn.request(method, target, body=body, headers=headers)
            resp = conn.getresponse()
            data = resp.read()
            own = sorted((k.lower(), v) for k, v in resp.getheaders() if k.lower().startswith('x-glass-'))
        finally:
            conn.close()
        sign = dict(own).pop('x-glass-sign', None)
        if sign is None:
            # Refused before the signature check: the clocks drifted apart — adjust and retry once.
            now = dict(own).get('x-glass-now')
            if resp.status == 401 and now and retry:
                self.skew = int(now) - int(time.time())
                return self.request(method, path, query, body, timeout, retry=False)
            raise Refused(f'{resp.status}: {data.decode(errors="replace").strip()}')
        signed = [(k, v) for k, v in own if k != 'x-glass-sign']
        text = f'{resp.status}\n{nonce}\n' + '\n'.join(f'{k}:{v}' for k, v in signed) + f'\n{hashlib.sha256(data).hexdigest()}'
        expected = hmac.new(self.key, text.encode(), hashlib.sha256).hexdigest()
        if not hmac.compare_digest(expected, sign):
            raise Refused('the answer is not signed with our key: someone else answered')
        meta = {k[len('x-glass-'):]: urllib.parse.unquote(v) for k, v in signed}
        return resp.status, data, meta

    def ok(self, method, path, query=None, body=b'', timeout=60):
        status, data, meta = self.request(method, path, query, body, timeout)
        if status != 200:
            raise Refused(f'{status}: {data.decode(errors="replace").strip()}')
        return data, meta


def machine(args):
    cfg = load_config()
    host = args.host or os.environ.get('GLASSOS_HOST') or cfg.get('host')
    # Each machine has its own key (key@HOST); key is the default machine's key.
    name = (host or '').partition(':')[0]
    key = args.key or os.environ.get('GLASSOS_KEY') or cfg.get(f'key@{name}') or cfg.get('key')
    if not host or not key:
        sys.exit('remote: no machine yet: remote.py setup HOST KEY')
    return Machine(host, parse_key(key))


def out(data):
    sys.stdout.write(data.decode(errors='replace'))
    sys.stdout.flush()


def cmd_setup(args):
    key = parse_key(args.secret)
    os.makedirs(os.path.dirname(CONFIG), exist_ok=True)
    words = base64.b32encode(key).decode()
    # Keys of other machines are kept; the default machine is the first one configured
    # (or this one, with --default).
    cfg = load_config()
    name = args.target.partition(':')[0]
    if 'host' in cfg and 'key' in cfg:
        old = cfg['host'].partition(':')[0]
        cfg.setdefault(f'key@{old}', cfg['key'])
    cfg[f'key@{name}'] = words
    if args.default or 'host' not in cfg:
        cfg['host'], cfg['key'] = args.target, words
    fd = os.open(CONFIG, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
    with os.fdopen(fd, 'w') as f:
        f.write(''.join(f'{k}={v}\n' for k, v in cfg.items()))
    print(f'remote: saved {args.target} in {CONFIG}' + ('' if cfg['host'] == args.target else f' (default stays {cfg["host"]}; use --host {name})'))
    m = Machine(args.target, key)
    data, _ = m.ok('GET', '/info')
    out(data)


def cmd_info(args):
    out(machine(args).ok('GET', '/info')[0])


def cmd_run(args):
    m = machine(args)
    query = {'wait': args.wait}
    if args.cwd:
        query['cwd'] = args.cwd
    data, _ = m.ok('POST', '/run', query, ' '.join(args.command).encode(), timeout=args.wait + 30)
    out(data)


def cmd_sh(args):
    m = machine(args)
    cwd = args.cwd or ''
    while True:
        try:
            line = input(f'glassos:{cwd or "~"}$ ')
        except EOFError:
            print()
            return
        if line.strip() in ('exit', 'quit'):
            return
        query = {'wait': args.wait}
        if cwd:
            query['cwd'] = cwd
        try:
            data, meta = m.ok('POST', '/run', query, line.encode(), timeout=args.wait + 30)
        except (Refused, OSError) as e:
            print(f'remote: {e}')
            continue
        out(data)
        cwd = meta.get('cwd', cwd)


def cmd_log(args):
    m = machine(args)
    start = args.start
    while True:
        try:
            data, meta = m.ok('GET', '/log', {'from': start} if start else None)
        except OSError as e:
            if not args.follow:
                raise
            print(f'remote: {e}; trying again', file=sys.stderr)
            time.sleep(3)
            continue
        text = data.decode(errors='replace')
        if args.grep:
            text = ''.join(l for l in text.splitlines(True) if args.grep in l)
        sys.stdout.write(text)
        sys.stdout.flush()
        start = int(meta.get('log-end', start or 0))
        if not args.follow:
            return
        time.sleep(1)


def cmd_ls(args):
    data, _ = machine(args).ok('GET', '/list', {'path': args.path})
    for line in data.decode(errors='replace').splitlines():
        kind, size, name = (line.split('\t') + ['', ''])[:3]
        print(f'{name}/' if kind == 'd' else f'{name}  {size}')


def cmd_get(args):
    data, _ = machine(args).ok('GET', '/file', {'path': args.remote}, timeout=300)
    local = args.local or os.path.basename(args.remote)
    with open(local, 'wb') as f:
        f.write(data)
    print(f'remote: {args.remote} -> {local}, {len(data)} bytes')


def cmd_put(args):
    with open(args.local, 'rb') as f:
        body = f.read()
    query = {'path': args.remote}
    if args.parents:
        query['parents'] = 1
    out(machine(args).ok('PUT', '/file', query, body, timeout=300)[0])


def cmd_input(args):
    """Input on the machine: type text, press keys, click the mouse (screen pixels, as on shot)."""
    if args.what == 'type':
        lines = ['text ' + ' '.join(args.args)]
    elif args.what == 'key':
        lines = ['key ' + k for k in args.args]
    else:
        if len(args.args) < 2:
            sys.exit(f'remote: {args.what} X Y')
        lines = [args.what + ' ' + ' '.join(args.args)]
    data, _ = machine(args).ok('POST', '/input', None, '\n'.join(lines).encode())
    out(data)


def cmd_shot(args):
    data, _ = machine(args).ok('GET', '/shot', {'screen': args.screen}, timeout=120)
    path = args.file or time.strftime('glassos-%Y%m%d-%H%M%S.png')
    with open(path, 'wb') as f:
        f.write(data)
    print(f'remote: screenshot {path}, {len(data)} bytes')


def wait_for(m, check, seconds):
    end = time.time() + seconds
    while time.time() < end:
        try:
            data, _ = m.ok('GET', '/info', timeout=5)
            text = data.decode(errors='replace')
            if check(text):
                return text
        except (OSError, Refused, http.client.HTTPException):
            pass
        time.sleep(3)
    return None


def cmd_kernel(args):
    m = machine(args)
    with open(args.efi, 'rb') as f:
        image = f.read()
    out(m.ok('PUT', '/kernel', body=image, timeout=300)[0])
    if args.no_start:
        return
    out(m.ok('POST', '/reboot', {'into': 'trial'})[0])
    print('remote: waiting for the trial kernel...')
    time.sleep(10)
    text = wait_for(m, lambda t: 'trial: yes' in t, 300)
    if text is None:
        sys.exit('remote: the trial kernel did not answer in 5 minutes. Turn the machine off and on: the old kernel starts.')
    out(text.encode())
    print('remote: the trial kernel runs. remote.py keep makes it the main one; remote.py reboot goes back to the old one.')


def cmd_keep(args):
    out(machine(args).ok('POST', '/kernel/keep')[0])


def cmd_reboot(args):
    m = machine(args)
    out(m.ok('POST', '/reboot', {'into': args.into})[0])
    if args.into in ('normal', 'trial'):
        time.sleep(10)
        text = wait_for(m, lambda t: True, 300)
        print('remote: back' if text else 'remote: no answer after 5 minutes')


def main():
    p = argparse.ArgumentParser(prog='remote.py', description='Remote access to GlassOS')
    p.add_argument('--host')
    p.add_argument('--key')
    sub = p.add_subparsers(dest='cmd', required=True)
    s = sub.add_parser('setup')
    s.add_argument('target')
    s.add_argument('secret')
    s.add_argument('--default', action='store_true', help='make this the default machine')
    s.set_defaults(fn=cmd_setup)
    sub.add_parser('info').set_defaults(fn=cmd_info)
    s = sub.add_parser('run')
    s.add_argument('--cwd')
    s.add_argument('--wait', type=int, default=30)
    s.add_argument('command', nargs='+')
    s.set_defaults(fn=cmd_run)
    s = sub.add_parser('sh')
    s.add_argument('--cwd')
    s.add_argument('--wait', type=int, default=30)
    s.set_defaults(fn=cmd_sh)
    s = sub.add_parser('log')
    s.add_argument('-f', '--follow', action='store_true')
    s.add_argument('--from', dest='start', type=int, default=0)
    s.add_argument('--grep')
    s.set_defaults(fn=cmd_log)
    s = sub.add_parser('ls')
    s.add_argument('path')
    s.set_defaults(fn=cmd_ls)
    s = sub.add_parser('get')
    s.add_argument('remote')
    s.add_argument('local', nargs='?')
    s.set_defaults(fn=cmd_get)
    s = sub.add_parser('put')
    s.add_argument('local')
    s.add_argument('remote')
    s.add_argument('--parents', action='store_true')
    s.set_defaults(fn=cmd_put)
    for name, help_text in (('type', 'type TEXT — type text'), ('key', 'key NAME... — enter, esc, tab, backspace, up, down…'),
                            ('click', 'click X Y — click'), ('rclick', 'rclick X Y — right click'),
                            ('move', 'move X Y — move the mouse'), ('wheel', 'wheel X Y N — the wheel')):
        s = sub.add_parser(name, help=help_text)
        s.add_argument('args', nargs='+')
        s.set_defaults(fn=cmd_input, what=name)
    s = sub.add_parser('shot')
    s.add_argument('file', nargs='?')
    s.add_argument('--screen', type=int, default=0)
    s.set_defaults(fn=cmd_shot)
    s = sub.add_parser('kernel')
    s.add_argument('efi', nargs='?', default=KERNEL)
    s.add_argument('--no-start', action='store_true')
    s.set_defaults(fn=cmd_kernel)
    sub.add_parser('keep').set_defaults(fn=cmd_keep)
    s = sub.add_parser('reboot')
    s.add_argument('into', nargs='?', default='normal', choices=['normal', 'trial', 'bios', 'usb'])
    s.set_defaults(fn=cmd_reboot)
    args = p.parse_args()
    try:
        args.fn(args)
    except Refused as e:
        sys.exit(f'remote: {e}')
    except (OSError, http.client.HTTPException) as e:
        sys.exit(f'remote: {e}')


if __name__ == '__main__':
    main()
