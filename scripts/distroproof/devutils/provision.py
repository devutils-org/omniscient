#!/usr/bin/env python3
"""Install witness packages through xpac and retain every preview/result/failure."""
import json
import os
from pathlib import Path
import platform
import shutil
import subprocess
import sys
import time

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
    if provider == 'apk':
        # apk policy exits 0 for an unknown name too, printing nothing.
        result = run(['apk', 'policy', package])
        return result, 'policy:' in result['stdout'], result['code'] == 0
    if provider == 'portage':
        # Masked, missing or (with --getbinpkgonly) not built as a binary:
        # emerge refuses with 1 and says which.
        result = run(['emerge', '--pretend', '--quiet', '--nospinner', '--color=n', '--', package])
        return result, result['code'] == 0, True
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
                  'arch' if shutil.which('pacman') else 'fedora' if shutil.which('dnf') else
                  'alpine' if shutil.which('apk') else 'gentoo' if shutil.which('emerge') else None)
        report['xpac_version'] = run([XPAC, '--version'])
        report['providers'] = run([XPAC, 'providers', '--json'])
        if not family:
            report['engine'] = 'unsupported_native_provider'
            report['reason'] = 'no native package manager xpac drives'
        else:
            selection = json.loads(Path('scripts/distroproof/devutils/packages.json').read_text())[family]
            provider = selection['provider']
            report['provider'] = provider
            # xpac drives DNF4 too since its ADR 016 (Rocky 9's witnesses were
            # installed by dnf directly before, xpac only inventorying them).
            report['engine'] = 'xpac'
            if provider == 'portage':
                # Binaries from the stage's configured host, and nothing built
                # from source: a witness with no binary is unavailable here.
                with open('/etc/portage/make.conf', 'a') as conf:
                    conf.write('FEATURES="${FEATURES} getbinpkg"\nEMERGE_DEFAULT_OPTS="${EMERGE_DEFAULT_OPTS} --getbinpkgonly"\n')
            refresh = (['apt-get', 'update', '-qq'] if provider == 'apt' else
                       ['pacman', '-Sy', '--noconfirm'] if provider == 'pacman' else
                       ['apk', 'update'] if provider == 'apk' else
                       ['emerge', '--sync', '--quiet'] if provider == 'portage' else ['dnf', 'makecache', '-y'])
            result = run(refresh, timeout=900)
            report['bootstrap'].append(result)
            for pause in (30, 60):
                # A Gentoo mirror caught mid-update fails manifest verification.
                if result['code'] == 0 or provider != 'portage':
                    break
                time.sleep(pause)
                result = run(refresh, timeout=900)
                report['bootstrap'].append(result)
            if result['code'] != 0:
                raise ValueError('native metadata refresh failed: ' + result['stderr'][-1200:])
            for required, packages in [(True, selection['required']), (False, selection['optional'])]:
                for package in packages:
                    package = package.replace('KERNEL', platform.release())
                    # `a|b`: the first name this distribution has (Rocky 8's
                    # Python 3 is `python36`, its `python3` no package's name).
                    names = package.split('|')
                    for package in names:
                        native, available, query_ok = candidate(provider, package)
                        if available or not query_ok:
                            break
                    entry = {'package': package, 'required': required, 'status': 'failed', 'engine': report['engine']}
                    if len(names) > 1:
                        entry['alternatives'] = names
                    report['packages'].append(entry)
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
                    manifest = Path('witness-package.toml')
                    manifest.write_text('version = 1\n[packages.' + provider + ']\ninstall = [' + json.dumps(package) + ']\n')
                    entry['manifest'] = manifest.read_text()
                    entry['plan'] = run([XPAC, 'plan', '-f', str(manifest), '--json'])
                    if entry['plan']['code'] != 0:
                        entry['reason'] = 'xpac preview failed'
                        report['errors'].append(package + ': preview failed')
                        continue
                    entry['apply'] = run([XPAC, 'apply', '-f', str(manifest), '--yes', '--state-dir',
                                          '/var/lib/xpac-devutils', '--json'], timeout=1800)
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
        # libbpf-tools first, where the distribution has them: compiled ahead
        # (CO-RE), they start at once and need no kernel headers, where bcc's
        # Python tools compile for seconds or not at all (Debian 13 and
        # Gentoo's bcc did not run; Arch's libbpf ones held every pair). Their
        # names: Debian 12 and Ubuntu `tcplife`, Debian 13 `tcplife-libbpf`,
        # Fedora and Rocky `bpf-tcplife`. Then bcc's.
        for tool in ('runqlat', 'tcplife', 'tcpaccept', 'memleak', 'opensnoop', 'vfsstat', 'biolatency', 'execsnoop'):
            destination = Path('/usr/local/bin') / tool
            candidates = [Path(d) / n for d in ('/usr/sbin', '/usr/bin')
                          for n in (tool + '-libbpf', 'bpf-' + tool, tool)]
            candidates += [Path('/usr/share/bcc/tools') / tool, Path('/usr/sbin') / (tool + '-bpfcc')]
            for path in candidates:
                if os.access(str(path), os.X_OK) and not destination.exists() \
                        and path.resolve() != destination.resolve():
                    destination.symlink_to(path)
                    break
        if shutil.which('sudo'):
            Path('/etc/sudoers.d').mkdir(exist_ok=True)
            policy = Path('/etc/sudoers.d/omn-witnesses')
            # Rocky's secure_path leaves out /usr/local/bin, where the
            # witnesses are linked: `sudo vfsstat` was not found.
            policy.write_text('Defaults:test secure_path="/usr/local/sbin:/usr/local/bin:'
                              '/usr/sbin:/usr/bin:/sbin:/bin"\n'
                              'test ALL=(ALL) NOPASSWD: ALL\n')
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
