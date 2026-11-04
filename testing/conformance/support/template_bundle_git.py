#!/usr/bin/env python3
import json, os, sys
from pathlib import Path
args = sys.argv[1:]
with open(os.environ['EVENTS'], 'a') as f:
    f.write(json.dumps({'command': 'git', 'args': args}) + '\n')
if args[:1] == ['-C']:
    os.chdir(args[1]); args = args[2:]
case = os.environ['CASE']
if args[:1] == ['clone']:
    Path(args[-1]).mkdir()
elif args[:1] == ['ls-remote']:
    # all-conflict: no deps/bundle yet; every other case has one.
    if case != 'all-conflict':
        print('a'*40 + '\trefs/heads/deps/bundle')
elif args[:1] == ['merge']:
    if '--abort' in args:
        pass
    elif args[-1] == 'origin/dev' and case == 'target-conflict':
        sys.exit(1)
    elif args[-1] == 'FETCH_HEAD' and case == 'all-conflict':
        sys.exit(1)
elif args[:1] == ['fetch']:
    for arg in args:
        if arg.startswith('pull/'):
            n = int(arg.split("/")[1])
            # fetch-moved: the head moved between the commit check and the fetch.
            Path('.fetched').write_text(f'{n + 0x100 if case == "fetch-moved" else n:040x}')
elif args[:2] == ['rev-parse', 'FETCH_HEAD']:
    print(Path('.fetched').read_text())
elif args[:1] == ['rev-list']:
    print(0 if case in ('no-ahead', 'all-conflict') else 1)
elif args[:1] == ['rev-parse']:
    if 'MERGE_HEAD' in args and case not in ('all-conflict', 'target-conflict'): sys.exit(1)
    print('a'*40)
elif args[:1] in (['config'], ['checkout'], ['push']):
    pass
else:
    raise SystemExit('unexpected git invocation: ' + repr(args))
