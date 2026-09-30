#!/usr/bin/env python3
"""Compare layout replay and conservative fallback with an independent fresh backend."""
import argparse
import json
from pathlib import Path
import subprocess
import time

from incremental_matrix import Matrix, assert_equivalent, observation
from incremental_races import require_success
from smoke_checkpoint import atomic_json, build_metadata, file_digest

LIB = 'project/src/lib.rs'
SOURCE = '''mod child;
use std::{cell::RefCell, rc::Rc};
pub fn selected(input: i32) -> i32 {
    // ordinary comment stays a token
    let café = input + 1;
    let handle = Rc::new(RefCell::new(café));
    let alias = handle.clone();
    *alias.borrow_mut() = child::apply(café);
    let mapper = |value: i32| value + 1;
    let result = mapper(*handle.borrow());
    result
}
'''
FILES = {
 'project/Cargo.toml':'[package]\nname="matrix_fixture"\nversion="0.0.0"\nedition="2021"\n[workspace]\n',
 LIB:SOURCE,
 'project/src/child.rs':'pub fn apply(input: i32) -> i32 { input + 1 }\n',
}
PROTOCOL = {'FLOWISTRY_RESULT_PROTOCOL':'1', 'RUST_LOG':'flowistry::audit=info,flowistry_ide::fast_cache=debug'}


def main():
 p=argparse.ArgumentParser(description=__doc__)
 p.add_argument('--backend-dir',type=Path,required=True)
 p.add_argument('--reference-dir',type=Path,required=True)
 p.add_argument('--work-dir',type=Path,required=True)
 p.add_argument('--json',type=Path,required=True)
 p.add_argument('--rustfmt',required=True)
 args=p.parse_args()
 m=Matrix(args.backend_dir,args.work_dir,files=FILES,anchor='let café')
 report={'schema':1,'candidate':build_metadata(args.backend_dir),'reference':build_metadata(args.reference_dir),
         'harness_sha256':file_digest(Path(__file__)),'records':[]}

 def reference(case,mode):
  cmd=m.command(mode);cmd[0]=str(args.reference_dir.resolve()/'cargo-flowistry')
  env=m.env(case,'off');env['PATH']=str(args.reference_dir.resolve())+':'+env['PATH']
  start=time.monotonic();r=subprocess.run(cmd,cwd=m.project,env=env,capture_output=True,timeout=120)
  return observation(r,time.monotonic()-start)

 def case(mode,name,setup,edit,compiler,kind='lib',policy='on'):
  m.reset();setup(m)
  m.source=m.project/('src/main.rs' if kind=='bin' else 'src/lib.rs')
  if kind=='bin':
   m.write('project/src/main.rs',m.source.with_name('lib.rs').read_text()+'\nfn main() {}\n')
   m.source.with_name('lib.rs').unlink()
   m.changed.discard(LIB)
  key=mode+'-'+name
  records={}
  try:
   records['cold']=m.run(key,mode,extra=PROTOCOL);require_success(records['cold'])
   records['warm']=m.run(key,mode,extra=PROTOCOL);require_success(records['warm'])
   assert records['warm']['compiler_invocations']==0,records['warm']['stderr']
   edit(m)
   reused=records['edited']=m.run(key,mode,policy,extra=PROTOCOL)
   fresh=records['fresh']=reference(key,mode)
   require_success(reused);require_success(fresh);assert_equivalent(reused,fresh)
   assert reused['compiler_invocations']==compiler,reused['stderr']
   assert reused['publication']['status']=='current',reused['publication']
   if compiler==0:
    assert reused['maybe_slice_indices'] > 0, 'fixture must exercise maybe-slice index relocation'
    assert reused['cache']['validation']=='layout',reused['cache']
    assert not reused['solved_bodies']
    assert reused['publication']['revision']!=records['warm']['publication']['revision']
    assert reused['publication']['generation']!=records['warm']['publication']['generation']
    assert 'audit layout-hit' in reused['stderr']
   records['replayed']=m.run(key,mode,extra=PROTOCOL)
   require_success(records['replayed']);assert_equivalent(records['replayed'],fresh)
   assert records['replayed']['compiler_invocations']==0,records['replayed']['stderr']
   passed,error=True,None
  except Exception as exc:
   passed,error=False,str(exc)
  record={'mode':mode,'case':name,'kind':kind,'expected_compilers':compiler,'passed':passed,'error':error,'observations':records}
  report['records'].append(record);atomic_json(args.json,report)
  print(mode,name,'PASS' if passed else 'FAIL: '+error,flush=True)

 def rustfmt(m):
  subprocess.run([args.rustfmt,'--edition','2021',str(m.source)],check=True,capture_output=True)
 def corrupt(m):
  for path in (m.root/'caches').glob('*/responses-v1/*'):
   if path.is_file():
    try:
     value=json.loads(path.read_text())
    except ValueError:continue
    if value.get('layout'):
     value['layout']['sources']={}
     path.write_text(json.dumps(value))
  newline(m)
 noop=lambda m:None
 newline=lambda m:m.write(str(m.source.relative_to(m.root)),'\n\n'+m.source.read_text())
 def add(text):return lambda m:m.write(LIB,text+m.source.read_text())
 for mode in ('SigOnly','Recurse'):
  for kind in ('lib','bin'):
   case(mode,kind+'-leading-lines',noop,newline,0,kind)
  case(mode,'rustfmt',lambda m:m.write(LIB,SOURCE.replace('    let café','\t let café')),rustfmt,0)
  case(mode,'interior-indent',noop,lambda m:m.write(LIB,m.source.read_text().replace('    let café','\n\t\tlet café')),0)
  case(mode,'closure-indent',noop,lambda m:m.write(LIB,m.source.read_text().replace('    let mapper','\n      let mapper')),0)
  case(mode,'callee-layout',noop,lambda m:m.write('project/src/child.rs','\n\n'+m.files['project/src/child.rs']),0)
  case(mode,'both-files',noop,lambda m:(newline(m),m.write('project/src/child.rs','\n'+m.files['project/src/child.rs'])),0)
  case(mode,'comment-text',noop,lambda m:m.write(LIB,m.source.read_text().replace('ordinary comment','different comment')),1)
  case(mode,'new-comment',noop,lambda m:m.write(LIB,'// inserted\n'+m.source.read_text()),1)
  case(mode,'semantic-edit',noop,lambda m:m.write(LIB,m.source.read_text().replace('input + 1','input + 2')),1)
  case(mode,'line-macro',add('fn observer() -> u32 { line!() }\n'),newline,1)
  case(mode,'doc-comment',add('/// docs\nfn documented() {}\n'),newline,1)
  case(mode,'source-location',add('const POSITION: u32 = std::panic::Location::caller().line();\n'),newline,1)
  case(mode,'include',lambda m:(m.write('project/src/data.txt','value'),m.write(LIB,'const DATA: &str = include_str!("data.txt");\n'+SOURCE)),newline,1)
  case(mode,'cfg-attribute',add('#[cfg(any())] fn hidden() {}\n'),newline,1)
  case(mode,'build-script',lambda m:m.write('project/build.rs','fn main() {}\n'),newline,1)
  case(mode,'dependency',lambda m:(m.write('dependency/Cargo.toml','[package]\nname="layout_dep"\nversion="0.0.0"\nedition="2021"\n'),m.write('dependency/src/lib.rs','pub fn value() -> i32 { 1 }\n'),m.write('project/Cargo.toml',FILES['project/Cargo.toml']+'[dependencies]\nlayout_dep={path="../dependency"}\n')),newline,1)
  case(mode,'input-membership',noop,lambda m:(newline(m),m.write('project/new-input.txt','new')),1)
  case(mode,'manifest-change',noop,lambda m:(newline(m),m.write('project/Cargo.toml',FILES['project/Cargo.toml']+'\n# modified\n')),1)
  case(mode,'cargo-config',lambda m:m.write('project/.cargo/config.toml','[build]\njobs=2\n'),newline,1)
  case(mode,'wrapper',lambda m:m.compiler_gate(),newline,1)
  case(mode,'proof-corruption',noop,corrupt,1)
  case(mode,'refresh',noop,newline,1,policy='refresh')
 report['passed']=all(r['passed'] for r in report['records']);report['completed']=True
 atomic_json(args.json,report)
 raise SystemExit(0 if report['passed'] else 1)


if __name__=='__main__':main()
