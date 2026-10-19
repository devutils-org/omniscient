#!/usr/bin/env python3
"""Exercise installed tools as an ordinary guest user; do not use Proxmox."""
import hashlib
import json
import os
from pathlib import Path
import platform
import signal
import socket
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request


def run(args, timeout=120, env=None):
    p = subprocess.run(args, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                       universal_newlines=True, timeout=timeout, env=env)
    return {'args': args, 'code': p.returncode, 'stdout': p.stdout, 'stderr': p.stderr}


def require(value, message):
    if not value:
        raise ValueError(message)


def success(result):
    require(result['code'] == 0, result['stderr'][-1500:])
    return result


def main():
    target = os.environ.get('DP_TARGET', '')
    require(Path.cwd() == Path('/distroproof/work') and target and
            Path('/etc/devutils-distroproof-guest').read_text().strip() == target,
            'requires the prepared disposable guest')
    require(os.getuid() != 0, 'the cross-tool scenario must run as an ordinary user')
    provision = json.loads(Path('witnesses.json').read_text())
    bundle = json.loads(Path('target/devutils-input/builds.json').read_text())
    report = {'schema_version': 1, 'target': target, 'image': os.environ['DP_IMAGE'],
              'uid': os.getuid(), 'kernel': platform.release(), 'checks': [], 'builds': bundle,
              'provision_errors': provision['errors']}
    def check(name, action):
        result = {'name': name, 'status': 'pass'}
        report['checks'].append(result)
        try:
            result['detail'] = action()
            print('PASS ' + name, flush=True)
        except Exception as error:
            result['status'] = 'fail'
            result['error'] = str(error)
            print('FAIL ' + name + ': ' + str(error), flush=True)
    check('xpac-installed', lambda: success(run(['/opt/xpac/xpac', 'providers', '--json'])))
    check('omn-installed', lambda: success(run(['/opt/omn/bin/omn', '--version'])))
    check('atfq-installed', lambda: success(run(['/opt/devutils/atfq', '--version'])))
    def memory():
        result = success(run(['/opt/devutils/atfq', 'probe', 'mem.summary', '--json']))
        value = json.loads(result['stdout'])
        fields = dict((line.split(':')[0], int(line.split()[1]) * 1024)
                      for line in Path('/proc/meminfo').read_text().splitlines() if line.split(':')[0] in ('MemTotal', 'SwapTotal'))
        require(value['status'] == 'ok', 'ATFQ memory probe not usable')
        require(value['data']['total'] == fields['MemTotal'] and value['data']['swap_total'] == fields['SwapTotal'], 'ATFQ disagrees with procfs')
        return result
    check('atfq-procfs', memory)
    for query in ('free', 'zombie', 'command not found'):
        check('atfq-' + query.replace(' ', '-'), lambda query=query: success(run(['/opt/devutils/atfq', query, '--json'])))
    def mcp():
        messages = [
            {'jsonrpc':'2.0','id':1,'method':'initialize','params':{'protocolVersion':'2024-11-05','capabilities':{},'clientInfo':{'name':'devutils-chain','version':'1'}}},
            {'jsonrpc':'2.0','method':'notifications/initialized'},
            {'jsonrpc':'2.0','id':2,'method':'tools/list','params':{}},
        ]
        p = subprocess.Popen(['/opt/devutils/atfq','mcp'],stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=subprocess.PIPE,universal_newlines=True)
        try:
            stdout, stderr = p.communicate('\n'.join(json.dumps(m) for m in messages)+'\n',timeout=15)
            values=[json.loads(line) for line in stdout.splitlines() if line.strip()]
            tools=next(v['result']['tools'] for v in values if v.get('id')==2)
            require({'lookup','search','toc','run_probe','list_probes'} <= {t['name'] for t in tools},'MCP tools missing')
            require(all(t.get('annotations',{}).get('readOnlyHint') is True for t in tools),'MCP tools lost their read-only declaration')
            return {'tools':[t['name'] for t in tools], 'stderr':stderr}
        finally:
            if p.poll() is None: p.kill();p.wait()
    check('atfq-mcp', mcp)
    def service():
        with tempfile.TemporaryDirectory(prefix='devutils-service-') as tmp:
            database=str(Path(tmp)/'service.sqlite')
            with socket.socket() as sock:
                sock.bind(('127.0.0.1',0)); port=sock.getsockname()[1]
            env={k:v for k,v in os.environ.items() if not k.startswith(('VMANYWARE_', 'PROXMOX_'))}
            env.update(VMANYWARE_PORT=str(port),VMANYWARE_DATABASE=database)
            binary='/opt/devutils/vmanyware-server'
            def request(path, data=None, content=None, extra_headers=None):
                headers={'Content-Type':content} if content else {}
                headers.update(extra_headers or {})
                req=urllib.request.Request('http://127.0.0.1:'+str(port)+path,data=data,headers=headers)
                try:
                    with urllib.request.urlopen(req,timeout=3) as response:return response.status,json.loads(response.read())
                except urllib.error.HTTPError as error:return error.code,json.loads(error.read())
            def start():
                log=open(str(Path(tmp)/'service.log'),'a')
                proc=subprocess.Popen([binary],env=env,stdout=log,stderr=log)
                for attempt in range(100):
                    if proc.poll() is not None: log.close();raise ValueError('server exited: '+Path(tmp,'service.log').read_text())
                    try:
                        if request('/api/v1/health')==(200,{'status':'ok'}):return proc,log
                    except (OSError,ValueError):pass
                    time.sleep(.05)
                proc.kill();proc.wait();log.close();raise ValueError('server startup timed out')
            def stop(proc,log):
                proc.send_signal(signal.SIGTERM)
                try:proc.wait(timeout=10)
                except subprocess.TimeoutExpired:proc.kill();proc.wait();raise ValueError('graceful shutdown timed out')
                finally:log.close()
                require(proc.returncode==0,'server did not exit cleanly')
            proc,log=start()
            try:
                code,value=request('/api/v1/definitions')
                require(code==200 and value==[], 'definitions not initially empty')
                definition=Path('target/devutils-input/environment.yaml').read_text().replace('name: kernel-lab', 'name: chain-lab').encode()
                code,validated=request('/api/v1/definitions/validate',definition,'application/yaml')
                require(code==200,'definition validation failed: '+repr(validated))
                code,origin=request('/api/v1/definitions',definition,'application/yaml',{'Origin':'http://untrusted.invalid'})
                require(code==403,'browser-origin write was not refused')
                code,saved=request('/api/v1/definitions',definition,'application/yaml')
                require(code==201,'definition was not saved: '+repr(saved))
                code,duplicate=request('/api/v1/definitions',definition,'application/yaml')
                require(code==409,'duplicate definition did not conflict')
                contender=run([binary],env=env,timeout=10)
                require(contender['code']!=0 and 'database_lock_unavailable' in contender['stderr'],'second server did not refuse the locked database')
                code,inventory=request('/api/v1/provider/inventory')
                require(code==503 and inventory['error']['code']=='provider_not_configured','missing provider misreported')
                # Observe this real server process, without granting the driver new rights.
                measurement=run(['/opt/omn/bin/omn','mem.rss','--pid',str(proc.pid),'-m','poll@100ms','-e','timeline,summary','-d','1s','--format','json'])
                success(measurement)
                require(bool(json.loads(measurement['stdout'])),'empty Omniscient measurement')
            finally:stop(proc,log)
            proc,log=start()
            try:
                code,reopened=request('/api/v1/definitions/chain-lab')
                require(code==200 and reopened==saved,'saved definition did not survive process restart')
            finally:stop(proc,log)
            return {'definition':saved,'measurement':measurement,'restart':'persisted','provider':'not_configured'}
    check('vmanyware-observed-by-omn', service)
    def references():
        if provision['engine']=='unsupported_native_provider':
            Path('reference.log').write_text('not applicable: native witness provisioning unsupported\n')
            return {'not_applicable':provision['reason']}
        env=dict(os.environ, OMN_REFERENCE_ROOT=os.getcwd(),OMN_REFERENCE_BIN='/opt/omn/bin/omn')
        # Streamed to the file, so a hung pair still leaves the log up to it.
        with open('reference.log','w') as log:
            p=subprocess.Popen(['target/devutils-input/reference','--test-threads=1','--nocapture','pairs_'],
                               stdout=log,stderr=subprocess.STDOUT,env=env)
            try:p.wait(timeout=2400)
            except subprocess.TimeoutExpired:
                p.kill();p.wait()
                raise ValueError('reference pairs timed out after 2400 s; last lines: '+
                                 Path('reference.log').read_text()[-1500:])
        require(p.returncode==0,Path('reference.log').read_text()[-1500:])
        return {'code':p.returncode,'log':'reference.log'}
    check('omn-reference-pairs', references)
    Path('cross-tools.json').write_text(json.dumps(report,indent=2)+'\n')
    return 1 if report['provision_errors'] or any(c['status']=='fail' for c in report['checks']) else 0


if __name__=='__main__':
    sys.exit(main())
