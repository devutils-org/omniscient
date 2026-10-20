#!/usr/bin/env python3
"""Hold the devutils tools to what their READMEs promise, with omn.

    ATFQ=... XPAC=... ENVPROBE=... python3 scripts/devutils/promises.py [OMN]

Runs examples/devutils/{atfq,xpac-plan,envprobe}-read-only.yml from the
checkout's root, as the user who calls it, and judges each run:

- no file created, renamed, removed or synced (io.vfs, every process the tool
  starts included);
- no TCP connection (net.tcp.connect), and for atfq no IPv4 or IPv6 socket
  at all (net.socket: netlink, which reads the interfaces, is not the
  network);
- no program started but the ones the tool says it runs (proc.exec).

Prints one line per check and writes target/devutils-promises.json. Exits 1
if a promise does not hold, 2 if a run could not be made.
"""
import collections
import json
import os
from pathlib import Path
import subprocess
import sys

ROOT = Path(__file__).resolve().parents[2]
OMN = sys.argv[1] if len(sys.argv) > 1 else str(ROOT / 'target/release/omn')
CHANGES = ('vfs_create', 'vfs_mkdir', 'vfs_rename', 'vfs_rmdir', 'vfs_unlink', 'do_fsync', 'vfs_fsync_range')
AF_INET, AF_INET6 = 2, 10

# tool: (session, the variable naming its binary, the programs it may start)
TOOLS = {
    'atfq': ('atfq-read-only.yml', 'ATFQ', {'sh', 'atfq', 'systemctl'}),
    'xpac': ('xpac-plan-read-only.yml', 'XPAC', {'sh', 'xpac', 'pacman', 'pacman-conf'}),
    'envprobe': ('envprobe-read-only.yml', 'ENVPROBE', {'sh', 'envprobe', 'ldconfig'}),
}


def attr(event, name):
    return next((a['value'] for a in event['attrs'] if a['name'] == name), None)


def run(tool):
    session, var, allowed = TOOLS[tool]
    if not os.environ.get(var):
        return {'tool': tool, 'error': f'{var} is not set'}
    p = subprocess.run([OMN, '-f', f'examples/devutils/{session}', '--format', 'json', '--program-stdout', '/dev/null'],
                       cwd=ROOT, stdout=subprocess.PIPE, stderr=subprocess.PIPE, universal_newlines=True)
    if p.returncode != 0:
        return {'tool': tool, 'error': p.stderr.strip().splitlines()[-1] if p.stderr.strip() else f'omn exited {p.returncode}'}
    results = {r['label']: r for r in json.loads(p.stdout)['results']}
    out = {'tool': tool, 'checks': []}

    def check(name, ok, detail):
        out['checks'].append({'name': name, 'ok': ok, 'detail': detail})

    ops = {s['key'].split(':', 1)[1]: s.get('total', 0) for o in results['files']['outputs'] for s in o['series']}
    changed = {op: ops.get(op, 0) for op in CHANGES if ops.get(op, 0)}
    check('no file changed', not changed,
          f"{int(ops.get('vfs_open', 0))} opens, {int(ops.get('vfs_read', 0))} reads; " + (f'changes: {changed}' if changed else 'no create, rename, removal or sync'))

    connects = results['connects']['events_seen']
    check('no TCP connection', connects == 0, f'{connects} connection(s)')

    if 'sockets' in results:
        families = collections.Counter(attr(e, 'family') for o in results['sockets']['outputs'] for e in o.get('events') or [])
        inet = families.get(AF_INET, 0) + families.get(AF_INET6, 0)
        check('no IPv4/IPv6 socket', inet == 0, f'socket families seen: {dict(families) or "none"} (16 is netlink)')

    # by the process's name: the kernel keeps 64 bytes of an exec's path, and
    # a binary deep in a build tree loses its own name
    programs = collections.Counter(attr(e, 'comm') or '?' for o in results['execs']['outputs'] for e in o.get('events') or [])
    unknown = {p: n for p, n in programs.items() if p not in allowed}
    check('only its own programs', not unknown, f'started: {dict(programs)}' + (f'; unexpected: {unknown}' if unknown else ''))

    if 'cpu' in results:
        cpu = sum(s.get('total', 0) for o in results['cpu']['outputs'] for s in o['series'])
        out['cpu_ms'] = round(cpu / 1e6, 1)
    return out


def main():
    report = [run(tool) for tool in TOOLS]
    failed = False
    for r in report:
        if 'error' in r:
            print(f"{r['tool']:9} cannot run: {r['error']}")
            failed = failed or 'error'
            continue
        for c in r['checks']:
            print(f"{r['tool']:9} {'PASS' if c['ok'] else 'FAIL'}  {c['name']:22} {c['detail']}")
            if not c['ok'] and not failed:
                failed = True
        if 'cpu_ms' in r:
            print(f"{r['tool']:9} cost  {r['cpu_ms']} ms of cpu for the whole run")
    (ROOT / 'target').mkdir(exist_ok=True)
    (ROOT / 'target/devutils-promises.json').write_text(json.dumps(report, indent=2) + '\n')
    return 2 if failed == 'error' else 1 if failed else 0


if __name__ == '__main__':
    sys.exit(main())
