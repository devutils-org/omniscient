#!/usr/bin/env python3
"""Freeze the exact binaries, release archives, input tree identities and hashes."""
import argparse
import hashlib
import json
from pathlib import Path
import shutil
import subprocess
import tempfile

ROOT = Path(__file__).resolve().parents[3]


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def source(path):
    revision = subprocess.check_output(['git', 'rev-parse', 'HEAD'], cwd=path).decode().strip()
    diff = subprocess.check_output(['git', 'diff', 'HEAD', '--binary'], cwd=path)
    return {'revision': revision, 'dirty': bool(diff), 'tracked_diff_sha256': hashlib.sha256(diff).hexdigest()}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ('xpac', 'atfq', 'vmanyware', 'build-dir'):
        parser.add_argument('--' + name, required=True, type=Path)
    args = parser.parse_args()
    output = ROOT / 'target/devutils-input'
    manifest = {'schema_version': 1, 'files': {}, 'sources': {}}
    with tempfile.TemporaryDirectory(dir=ROOT / 'target', prefix='devutils-bundle-') as tmp:
        stage = Path(tmp)
        def add(path, name):
            shutil.copy2(path, stage / name)
            manifest['files'][name] = digest(stage / name)
        add(ROOT / 'target/dp-reference/reference', 'reference-gnu')
        add(ROOT / 'target/dp-reference/reference-musl', 'reference-musl')
        add(args.vmanyware / 'examples/kernel-lab.yaml', 'environment.yaml')
        for flavor in ('gnu', 'musl'):
            archives = list((ROOT / 'target/dist').glob('omn-*-x86_64-linux-' + flavor + '.tar.gz'))
            if len(archives) != 1:
                raise ValueError('expected exactly one Omniscient ' + flavor + ' release archive')
            archive = archives[0]
            expected = archive.with_name(archive.name + '.sha256').read_text().split()[0]
            if digest(archive) != expected:
                raise ValueError('Omniscient archive checksum mismatch')
            add(archive, 'omn-' + flavor + '.tar.gz')
            build = args.build_dir / ('x86_64-unknown-linux-' + flavor) / 'release'
            add(build / 'atfq', 'atfq-' + flavor)
            add(build / 'vmanyware-server', 'vmanyware-server-' + flavor)
        xpac_builds = json.loads((args.xpac / 'dist/distroproof/builds.json').read_text())
        xpac_archive = args.xpac / 'dist/distroproof/musl.tar.gz'
        if digest(xpac_archive) != xpac_builds['musl']['archive_sha256']:
            raise ValueError('xpac archive checksum mismatch')
        add(xpac_archive, 'xpac.tar.gz')
        for name, repo in [('omniscient', ROOT), ('xpac', args.xpac), ('atfq', args.atfq), ('vmanyware', args.vmanyware)]:
            manifest['sources'][name] = source(repo)
        (stage / 'builds.json').write_text(json.dumps(manifest, indent=2) + '\n')
        # Replace only this generated bundle; no source checkout is modified.
        output.mkdir(parents=True, exist_ok=True)
        for path in stage.iterdir():
            shutil.copy2(path, output / path.name)
    print(output / 'builds.json')


if __name__ == '__main__':
    main()
