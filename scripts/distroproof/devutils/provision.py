#!/usr/bin/env python3
"""Install witness packages through xpac and retain every preview/result/failure."""
import json
import os
from pathlib import Path
import platform
import shutil
import subprocess
import sys

XPAC = '/opt/xpac/xpac'


def run(args, timeout=300):
    try:
        p = subprocess.run(args, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                           universal_newlines=True, timeout=timeout,
                           env=dict(os.environ, LC_ALL='C', LANG='C', HOME='/root'))
        return {'args': args, 'code': p.returncode, 'stdout': p.stdout, 'stderr': p.stderr}
    except subprocess.TimeoutExpired:
        return {'args': args, 'code': None, 'stdout': '', 'stderr': 'timeout'}


def candidate(provider, package):
    if provider == 'apt':
        result = run(['apt-cache', 'policy', package])
        lines = [line.strip() for line in result['stdout'].splitlines()]
        exists = any(line.startswith('Candidate:') and line.split(':', 1)[1].strip() != '(none)' for line in lines)
        # apt-cache returns success with no candidate for a package absent here.
        return result, exists, result['code'] == 0
    if provider == 'pacman':
        result = run(['pacman', '-Si', '--', package])
        absent = 'was not found' in result['stderr'] or 'target not found' in result['stderr']
        return result, result['code'] == 0, result['code'] == 0 or absent
    result = run(['dnf', 'repoquery', '--available', '--qf', '%{name}', package])
    return result, bool(result['stdout'].strip()) and result['code'] == 0, result['code'] == 0


def main():
    target = os.environ.get('DP_TARGET', '')
    if (Path.cwd() != Path('/distroproof/work') or not target or os.getuid() != 0 or
            Path('/etc/devutils-distroproof-guest').read_text().strip() != target):
        raise SystemExit('provisioning requires the prepared disposable guest')
    report = {'schema_version': 1, 'target': target, 'image': os.environ['DP_IMAGE'],
              'packages': [], 'errors': [], 'bootstrap': [], 'provider': None, 'engine': None}
    try:
        os_release = dict(line.split('=', 1) for line in Path('/etc/os-release').read_text().splitlines()
                          if '=' in line and not line.startswith('#'))
        identity = os_release.get('ID', '').strip('"')
        family = ('ubuntu' if identity == 'ubuntu' else 'debian' if shutil.which('apt-get') else
                  'arch' if shutil.which('pacman') else 'fedora' if shutil.which('dnf') else None)
        report['xpac_version'] = run([XPAC, '--version'])
        report['providers'] = run([XPAC, 'providers', '--json'])
        if not family:
            report['engine'] = 'unsupported_native_provider'
            report['reason'] = 'xpac has no APK/Portage adapter; companion binaries remain testable'
        else:
            selection = json.loads(Path('scripts/distroproof/devutils/packages.json').read_text())[family]
            provider = selection['provider']
            report['provider'] = provider
            report['engine'] = 'dnf4-bootstrap' if provider == 'dnf' and not shutil.which('dnf5') else 'xpac'
            refresh = (['apt-get', 'update', '-qq'] if provider == 'apt' else
                       ['pacman', '-Sy', '--noconfirm'] if provider == 'pacman' else ['dnf', 'makecache', '-y'])
            result = run(refresh)
            report['bootstrap'].append(result)
            if result['code'] != 0:
                raise ValueError('native metadata refresh failed: ' + result['stderr'][-1200:])
            for required, packages in [(True, selection['required']), (False, selection['optional'])]:
                for package in packages:
                    package = package.replace('KERNEL', platform.release())
                    entry = {'package': package, 'required': required, 'status': 'failed', 'engine': report['engine']}
                    report['packages'].append(entry)
                    native, available, query_ok = candidate(provider, package)
                    entry['candidate'] = native
                    if not query_ok:
                        entry['reason'] = 'native candidate query failed'
                        report['errors'].append(package + ': candidate query failed')
                        continue
                    if not available:
                        entry['status'] = 'unavailable'
                        entry['reason'] = 'no candidate in this distribution’s configured repositories'
                        if required:
                            report['errors'].append(package + ': required package unavailable')
                        continue
                    if report['engine'] == 'dnf4-bootstrap':
                        entry['coverage'] = 'native DNF4 installation; xpac inventory only'
                        entry['apply'] = run(['dnf', 'install', '-y', package], timeout=600)
                    else:
                        manifest = Path('witness-package.toml')
                        manifest.write_text('version = 1\n[packages.' + provider + ']\ninstall = [' + json.dumps(package) + ']\n')
                        entry['manifest'] = manifest.read_text()
                        entry['plan'] = run([XPAC, 'plan', '-f', str(manifest), '--json'])
                        if entry['plan']['code'] != 0:
                            entry['reason'] = 'xpac preview failed'
                            report['errors'].append(package + ': preview failed')
                            continue
                        entry['apply'] = run([XPAC, 'apply', '-f', str(manifest), '--yes', '--state-dir',
                                              '/var/lib/xpac-devutils', '--json'], timeout=600)
                    if entry['apply']['code'] != 0:
                        entry['reason'] = 'installation failed despite an available candidate'
                        report['errors'].append(package + ': installation failed')
                        continue
                    entry['info'] = run([XPAC, 'info', provider + ':' + package, '--json'])
                    if entry['info']['code'] != 0:
                        entry['reason'] = 'xpac could not inspect the installed witness'
                        report['errors'].append(package + ': post-install inspection failed')
                        continue
                    entry['status'] = 'installed'
                    print('witnesses: ' + package + ' installed through ' + report['engine'], flush=True)
            inventory = run([XPAC, 'list', '--provider', provider, '--json'])
            report['inventory'] = inventory
            Path('installed-packages.json').write_text(inventory['stdout'] if inventory['code'] == 0 else json.dumps(inventory))
            if inventory['code'] != 0:
                report['errors'].append('final xpac inventory failed')
            # Preserve all transaction records, including failed/partial outcomes.
            history = run([XPAC, 'history', '--limit', '100', '--state-dir', '/var/lib/xpac-devutils', '--json'])
            Path('xpac-history.json').write_text(history['stdout'] if history['code'] == 0 else json.dumps(history))
            if history['code'] != 0:
                report['errors'].append('journal export failed')
        for directory in ('/usr/share/bcc/tools', '/usr/sbin'):
            for tool in ('runqlat', 'tcplife', 'tcpaccept', 'memleak', 'opensnoop', 'vfsstat', 'biolatency', 'execsnoop'):
                destination = Path('/usr/local/bin') / tool
                for name in (tool, tool + '-bpfcc'):
                    path = Path(directory) / name
                    if os.access(str(path), os.X_OK) and not destination.exists():
                        destination.symlink_to(path)
        if shutil.which('sudo'):
            Path('/etc/sudoers.d').mkdir(exist_ok=True)
            policy = Path('/etc/sudoers.d/omn-witnesses')
            policy.write_text('test ALL=(ALL) NOPASSWD: ALL\n')
            policy.chmod(0o440)
    except Exception as error:
        report['errors'].append(str(error))
    finally:
        # Non-provider images still produce explicit inventory/history documents.
        for name in ('installed-packages.json', 'xpac-history.json'):
            if not Path(name).exists():
                Path(name).write_text(json.dumps({'not_applicable': report.get('reason', 'setup failed')}))
        Path('witnesses.json').write_text(json.dumps(report, indent=2) + '\n')
    return 1 if report['errors'] else 0


if __name__ == '__main__':
    sys.exit(main())
