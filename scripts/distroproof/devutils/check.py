#!/usr/bin/env python3
"""Judge cross-tool cells, including setup, actual comparisons and exclusions."""
import argparse
import json
from pathlib import Path
import re
import sys
import tomllib

ROOT = Path(__file__).resolve().parents[3]
REQUIRED = {'xpac-installed','omn-installed','atfq-installed','atfq-procfs','atfq-free',
            'atfq-zombie','atfq-command-not-found','atfq-mcp','atfq-agrees-with-envprobe',
            'vmanyware-observed-by-omn','omn-reference-pairs'}


def require(value, message):
    if not value: raise ValueError(message)


def fact(probe, section, key):
    return next(e['value'] for s in probe['sections'] if s['name']==section
                for e in s['entries'] if e['key']==key and e['status']=='ok')


def judge(target, cell, witnesses, manifest, reference_log):
    require(target['outcome']=='success', 'VM outcome '+target['outcome'])
    require(target['setup']['exit_code']==0 and target['command']['exit_code']==0,'setup or command failed')
    require(cell['schema_version']==1 and cell['target']==target['name'] and cell['image']==target['image'],'wrong cell identity')
    require(cell['builds']==manifest,'cell used a different frozen bundle')
    require(cell['uid']!=0 and target['variant']['user']=='test','scenario was not an ordinary user')
    require(str(cell['uid'])==str(fact(target['envprobe'],'privileges','uid')),'UID disagrees with envprobe')
    require(cell['kernel']==fact(target['envprobe'],'kernel','release'),'kernel disagrees with envprobe')
    require(witnesses['target']==target['name'] and witnesses['image']==target['image'],'wrong provisioning identity')
    require(not witnesses['errors'] and not cell['provision_errors'],'provisioning failed: '+repr(witnesses['errors']))
    checks={c['name']:c for c in cell['checks']}
    require(set(checks)==REQUIRED and len(cell['checks'])==len(REQUIRED),'missing or duplicate scenario checks')
    require(all(c['status']=='pass' for c in checks.values()),'scenario failed: '+repr([(c['name'],c.get('error')) for c in checks.values() if c['status']!='pass']))
    engine=witnesses['engine']
    if engine=='unsupported_native_provider':
        # xpac drives every image's package manager since ADR 014 and 015.
        require(False,'unjustified missing native provider')
        require(bool(checks['omn-reference-pairs']['detail'].get('not_applicable')),'missing reference exclusion')
        pairs={'held':0,'skipped':0,'scope':'native provisioning unavailable'}
    else:
        require(engine in {'xpac','dnf4-bootstrap'},'unknown installation engine')
        if engine=='dnf4-bootstrap':
            require(target['image'].startswith('rocky-'),'unexpected DNF4 fallback')
        require(bool(witnesses['packages']),'no witness packages checked')
        for p in witnesses['packages']:
            require(p['status'] in {'installed','unavailable'},'package failed: '+p['package'])
            require(not p['required'] or p['status']=='installed','required package unavailable')
            if p['status']=='installed':
                require(p['apply']['code']==0 and p['info']['code']==0,'installation/inspection failed')
                if engine=='xpac':require(p['plan']['code']==0,'missing successful xpac plan')
            else:require(p['candidate']['code']==0 or p['engine']=='xpac','missing absence evidence')
        result=re.search(r'test result: (\w+)\. (\d+) passed; (\d+) failed;',reference_log)
        require(result and result[1]=='ok' and int(result[2])>0 and int(result[3])==0,'reference tests failed or did not run')
        held=len(re.findall(r'^reference .*: same numbers',reference_log,re.M))
        skipped=len(re.findall(r'^skipped: reference ',reference_log,re.M))
        require(held>0,'no reference comparison actually held')
        pairs={'held':held,'skipped':skipped,'scope':'reference comparisons'}
    return {'target':target['name'],'kernel':cell['kernel'],'engine':engine,
            'packages':sum(p['status']=='installed' for p in witnesses['packages']), 'references':pairs}


def main():
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('report',type=Path)
    parser.add_argument('--matrix',type=Path,default=ROOT/'distroproof-devutils.toml')
    parser.add_argument('--only')
    parser.add_argument('--output',type=Path,default=ROOT/'target/matrix/devutils-summary.json')
    args=parser.parse_args()
    matrix=tomllib.loads(args.matrix.read_text())
    expected={t['name']:t['image'] for t in matrix['target']}
    if args.only:
        chosen=set(args.only.split(','));require(chosen<=set(expected),'unknown target subset')
        expected={n:expected[n] for n in chosen}
    report=json.loads(args.report.read_text())
    targets=report['targets']
    require({t['name'] for t in targets}==set(expected) and len(targets)==len(expected),'missing/duplicate matrix targets')
    manifest=json.loads((ROOT/'target/devutils-input/builds.json').read_text())
    rows,failures=[],[]
    for target in targets:
        try:
            require(target['image']==expected[target['name']],'wrong image')
            paths={Path(p).name:Path(p) for p in target.get('collected',[])}
            cell=json.loads(paths['cross-tools.json'].read_text())
            witnesses=json.loads(paths['witnesses.json'].read_text())
            log=paths['reference.log'].read_text()
            row=judge(target,cell,witnesses,manifest,log)
            rows.append(row);print(json.dumps(row))
        except (ValueError,KeyError,OSError,StopIteration,TypeError) as error:
            failure={'target':target['name'],'error':str(error)};failures.append(failure);print(json.dumps(failure),file=sys.stderr)
    args.output.parent.mkdir(parents=True,exist_ok=True)
    args.output.write_text(json.dumps({'rows':rows,'failures':failures},indent=2)+'\n')
    return 1 if failures else 0


if __name__=='__main__':
    try:sys.exit(main())
    except (ValueError,OSError,KeyError) as error:sys.exit(str(error))
