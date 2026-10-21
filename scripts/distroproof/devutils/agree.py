#!/usr/bin/env python3
"""Do atfq and envprobe describe the same machine the same way?

    python3 scripts/distroproof/devutils/agree.py ENVPROBE ATFQ

The two tools read the machine with their own code, on purpose: neither
depends on the other. Where they read the same fact, they must agree. This
runs both and compares every fact they share: the CPU model and the online
CPUs, the memory, the kernel release, and what decides whether a tool may
watch the kernel (perf_event_paranoid, unprivileged BPF, kptr_restrict,
lockdown, BTF). A fact neither can read agrees; one that only one of them
reads, or that they read differently, does not.

Prints one line per fact and exits 1 on any disagreement. chain.py runs it
in every guest of the cross-tool chain. Python 3.6 or later.
"""
import json
import subprocess
import sys


def human(n):
    """envprobe's sizes: binary units, one decimal (`62.7 GiB`)."""
    units = ['B', 'KiB', 'MiB', 'GiB', 'TiB']
    value, unit = float(n), 0
    while value >= 1024 and unit < len(units) - 1:
        value /= 1024
        unit += 1
    return '%d B' % n if unit == 0 else '%.1f %s' % (value, units[unit])


def cpu_count(cpu_list):
    """`0-3,8` -> 5."""
    total = 0
    for part in filter(None, cpu_list.split(',')):
        a, _, b = part.partition('-')
        total += int(b) - int(a) + 1 if b else 1
    return total


def envprobe_values(report):
    """{(section, key): value or None when not ok}."""
    out = {}
    for section in report['sections']:
        for e in section['entries']:
            out[(section['name'], e['key'])] = e['value'] if e['status'] == 'ok' else None
    return out


def atfq_probe(atfq, name):
    p = subprocess.run([atfq, 'probe', name, '--json'], stdout=subprocess.PIPE,
                       stderr=subprocess.PIPE, universal_newlines=True, timeout=60)
    value = json.loads(p.stdout)
    return value.get('data') if value.get('status') == 'ok' else None


def facts(report, atfq):
    """[(fact, envprobe's, atfq's)], each side normalised to a string or None."""
    e = envprobe_values(report)
    cpu = atfq_probe(atfq, 'cpu.summary') or {}
    mem = atfq_probe(atfq, 'mem.summary') or {}
    kernel = atfq_probe(atfq, 'kernel.overview') or {}
    tracing = atfq_probe(atfq, 'kernel.tracing') or {}
    text = lambda v: None if v is None else str(v)
    btf = e.get(('btf', 'vmlinux'))
    return [
        ('cpu model', e.get(('hardware', 'cpu_model')), cpu.get('model')),
        ('online cpus', e.get(('hardware', 'cpus')),
         text(cpu_count(cpu['online'])) if cpu.get('online') else None),
        ('memory', e.get(('hardware', 'memory')), human(mem['total']) if 'total' in mem else None),
        ('kernel release', e.get(('kernel', 'release')), kernel.get('release')),
        ('perf_event_paranoid', e.get(('sysctl', 'kernel.perf_event_paranoid')),
         text(tracing.get('perf_event_paranoid'))),
        ('unprivileged_bpf_disabled', e.get(('sysctl', 'kernel.unprivileged_bpf_disabled')),
         text(tracing.get('unprivileged_bpf_disabled'))),
        ('kptr_restrict', e.get(('sysctl', 'kernel.kptr_restrict')), text(tracing.get('kptr_restrict'))),
        ('lockdown', e.get(('security', 'lockdown')), tracing.get('lockdown')),
        # envprobe: `present, N bytes` or not ok; atfq: true or false
        ('btf', 'present' if btf and btf.startswith('present') else 'absent',
         None if 'btf' not in tracing else ('present' if tracing['btf'] else 'absent')),
    ]


def compare(envprobe, atfq):
    p = subprocess.run([envprobe, '--json'], stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                       universal_newlines=True, timeout=120)
    if p.returncode != 0:
        raise ValueError('envprobe failed: ' + p.stderr[-500:])
    rows = facts(json.loads(p.stdout), atfq)
    disagree = [(f, a, b) for f, a, b in rows if a != b]
    return rows, disagree


def main():
    envprobe, atfq = sys.argv[1], sys.argv[2]
    rows, disagree = compare(envprobe, atfq)
    for fact, a, b in rows:
        print('%-5s %-26s envprobe %-40s atfq %s' % ('ok' if a == b else 'DIFF', fact, a, b))
    return 1 if disagree else 0


if __name__ == '__main__':
    sys.exit(main())
