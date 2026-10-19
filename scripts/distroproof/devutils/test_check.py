#!/usr/bin/env python3
"""Planted failures must not turn missing tools or comparisons into success."""
import copy
import importlib.util
from pathlib import Path
import unittest

spec=importlib.util.spec_from_file_location('chain_check',Path(__file__).with_name('check.py'))
check=importlib.util.module_from_spec(spec);spec.loader.exec_module(check)


def baseline():
    probe={'sections':[{'name':name,'entries':[{'key':key,'status':'ok','value':value}]}
                       for name,key,value in [('kernel','release','6.1'),('privileges','uid','1000')]]}
    target={'name':'chain-debian-12','image':'debian-12','outcome':'success','setup':{'exit_code':0},
            'command':{'exit_code':0},'variant':{'user':'test'},'envprobe':probe}
    manifest={'files':{'xpac.tar.gz':'digest'}}
    cell={'schema_version':1,'target':target['name'],'image':target['image'],'builds':manifest,
          'uid':1000,'kernel':'6.1','provision_errors':[],
          'checks':[{'name':name,'status':'pass','detail':{}} for name in sorted(check.REQUIRED)]}
    witnesses={'target':target['name'],'image':target['image'],'errors':[],'engine':'xpac',
               'packages':[{'package':'strace','status':'installed','required':True,
                            'apply':{'code':0},'info':{'code':0},'plan':{'code':0}}]}
    log='reference strace counts: same numbers\ntest result: ok. 1 passed; 0 failed; 0 ignored;\n'
    return target,cell,witnesses,manifest,log


class EvidenceTests(unittest.TestCase):
    def test_complete_cell_passes(self):
        self.assertEqual(check.judge(*baseline())['references']['held'],1)

    def test_missing_failure_and_wrong_binary_are_rejected(self):
        for mutation in [
            lambda t,c,w:c['checks'].pop(),
            lambda t,c,w:c['checks'][0].update(status='fail'),
            lambda t,c,w:c.update(builds={}),
            lambda t,c,w:c.update(uid=0),
            lambda t,c,w:t.update(outcome='failure'),
            lambda t,c,w:t['setup'].update(exit_code=1),
            lambda t,c,w:w['errors'].append('available candidate failed to install'),
            lambda t,c,w:w['packages'][0].update(status='unavailable'),
            lambda t,c,w:w.update(engine='dnf4-bootstrap'),
        ]:
            with self.subTest(mutation=mutation):
                target,cell,witnesses,manifest,log=copy.deepcopy(baseline())
                mutation(target,cell,witnesses)
                with self.assertRaises(ValueError):check.judge(target,cell,witnesses,manifest,log)

    def test_skips_and_failed_pairs_are_not_proof(self):
        target,cell,witnesses,manifest,_=baseline()
        for log in ['test result: ok. 1 passed; 0 failed;',
                    'reference strace counts: same numbers\ntest result: FAILED. 1 passed; 1 failed;']:
            with self.assertRaises(ValueError):check.judge(target,cell,witnesses,manifest,log)


if __name__=='__main__':unittest.main()
