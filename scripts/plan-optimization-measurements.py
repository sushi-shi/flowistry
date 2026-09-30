#!/usr/bin/env python3
"""Pin immediate-predecessor optimization measurements for the quiet acceptance queue."""
import argparse
import importlib.util
import json
from pathlib import Path
import subprocess
import sys

from smoke_checkpoint import atomic_json, file_digest

spec = importlib.util.spec_from_file_location('queue', Path(__file__).with_name('run-acceptance-queue.py'))
queue = importlib.util.module_from_spec(spec)
spec.loader.exec_module(queue)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--artifacts', type=Path, required=True)
    parser.add_argument('--prepared-corpus', type=Path, required=True)
    parser.add_argument('--perf', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True, help='new run directory')
    parser.add_argument('--after', nargs=2, action='append', default=[], metavar=('PID', 'REPORT'))
    args = parser.parse_args()
    root = Path(__file__).resolve().parent.parent
    args.output = args.output.resolve()
    if args.output.exists():
        parser.error('output directory exists; preserve the prior plan and samples')
    cases = {'either': ['src/into_either.rs:58:8'], 'serde_json': ['src/de.rs:329:8'],
             'just': ['src/analyzer.rs:177:4', 'src/compiler.rs:13:4', 'src/error.rs:926:10'],
             'niri': ['src/niri.rs:1605:8', 'src/backend/tty.rs:649:20']}
    corpus = json.loads((root/'scripts/smoke-corpus/corpus.json').read_text())
    for name, positions in cases.items():
        entry = next(e for e in corpus['crates'] if e['name'] == name)
        directory = name if 'git' in entry else name + '-' + entry['version']
        locked = {tuple(line.split('\t')[:3]) for line in (root/'scripts/smoke-corpus'/directory/'positions.tsv').read_text().splitlines()
                  if line and not line.startswith('#')}
        if any(tuple(position.rsplit(':', 2)) not in locked for position in positions):
            parser.error('requested measurement is not a locked position')
    pairs = [('row-groups', 'baseline', 'row-groups', 'perf/combined-chain-measurements', 'perf/integrated-recurse-groups'),
             ('seed-rows', 'row-groups', 'seed-rows', 'perf/integrated-recurse-groups', 'perf/integrated-seed-rows')]
    inputs = [root/'scripts'/name for name in ('run-acceptance-queue.py', 'smoke-real-crates.py',
              'smoke_checkpoint.py', 'summarize-validation.py', 'summarize-benchmarks.py')]
    inputs += [p for p in (root/'scripts/smoke-corpus').rglob('*') if p.is_file()]
    inputs += [args.perf.resolve(), Path('/proc/sys/kernel/random/boot_id')]
    predecessor_proof, jobs, compilers = [], [], set()
    for feature, base, candidate, base_branch, candidate_branch in pairs:
        for archive, branch in ((base, base_branch), (candidate, candidate_branch)):
            directory = args.artifacts.resolve()/archive
            build = json.loads((directory/'build.json').read_text())
            if build['features']:
                parser.error('normal performance requires binaries without reference instrumentation')
            subprocess.run(['git','diff','--exit-code',build['revision'],branch,'--','crates','Cargo.toml','Cargo.lock','rust-toolchain.toml'],cwd=root,check=True)
            branch_revision = subprocess.check_output(['git','rev-parse',branch],cwd=root,text=True).strip()
            predecessor_proof.append({'feature':feature,'archive':archive,'build_revision':build['revision'],
                                      'pr_branch':branch,'pr_revision':branch_revision,'runtime_diff_empty':True})
            compilers.add(build['compiler'])
            inputs += [directory/name for name in ('build.json','cargo-flowistry','flowistry-driver')]
        for kind in ('performance', 'diagnostics'):
            for crate, positions in cases.items():
                name = f'{feature}-{crate}-{kind}'
                report, log = args.output/(name+'.json'), args.output/(name+'.log')
                command = [sys.executable, str(root/'scripts/smoke-real-crates.py'), str(args.artifacts.resolve()/base),
                           '--compare',str(args.artifacts.resolve()/candidate),'--crate',crate,
                           '--work-dir',str(args.prepared_corpus.resolve()),'--modes','SigOnly,Recurse',
                           '--base-cache','off','--compare-cache','off','--cargo-replay','on',
                           '--cache-dir',str(args.output/'caches'/name),'--warmup','2',
                           '--repeat','8' if kind=='performance' else '1','--timeout','600',
                           '--memory-limit','6G','-j','1','--json',str(report)]
                for position in positions:
                    command += ['--position',position]
                command += ['--perf',str(args.perf.resolve())] if kind=='performance' else ['--phases']
                expected = [[file,int(line),int(column),mode] for file,line,column in (p.rsplit(':',2) for p in positions)
                            for mode in ('SigOnly','Recurse')]
                jobs.append({'name':name,'kind':kind,'command':command,'report':str(report),'log':str(log),
                             'repeat':8 if kind=='performance' else 1,'expected_positions':expected})
    if len(compilers) != 1:
        parser.error('compiler identities differ between archived performance builds')
    observed, prerequisites = queue.processes(), []
    for pid, report in args.after:
        item = observed.get(int(pid))
        if not item or item['state'] in ('Z','X'):
            parser.error(f'prerequisite PID {pid} is not live; inspect its report before planning')
        command = (Path('/proc')/pid/'cmdline').read_bytes().split(b'\0')
        if str(Path(report).resolve()).encode() not in command:
            parser.error(f'PID {pid} is not producing the specified report')
        prerequisites.append({'pid':int(pid),'start':item['start'],'name':item['name'],
                              'report':str(Path(report).resolve()),'summary':str(args.output/(f'prerequisite-{pid}-summary.json'))})
    plan = {'schema':1,'cwd':str(root),'predecessor_proof':predecessor_proof,'jobs':jobs,
            'inputs':{str(p.resolve()):file_digest(p) for p in sorted(set(inputs))},
            'prerequisites':prerequisites,'poll_seconds':30,
            'environment':{'CARGO_BUILD_JOBS':'2','RUST_LOG':'','FLOWISTRY_VERIFY_SUMMARIES':'0'},
            'quiet':{'interval_seconds':5,'consecutive':4,'monitor_seconds':2,'max_background_cores':.5,
                     'max_busy_fraction':.05,'max_load1':2.0},
            'scope':'Immediate-predecessor row-group/seed acceptance. Diagnostics are separate from normal timings. '
                    'This does not replace project/save measurements, final reference checks or all resource budgets.'}
    args.output.mkdir(parents=True,exist_ok=False)
    atomic_json(args.output/'plan.json',plan)
    print(json.dumps({'plan':str(args.output/'plan.json'),'jobs':len(jobs),'prerequisites':prerequisites},indent=2))


if __name__ == '__main__':
    main()
