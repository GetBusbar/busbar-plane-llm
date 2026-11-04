#!/usr/bin/env python3
import json, os, sys
from pathlib import Path
args = sys.argv[1:]
case = os.environ['CASE']
event = {'command': 'gh', 'args': args}
if '--body-file' in args:
    event['body'] = Path(args[args.index('--body-file') + 1]).read_text()
elif '--body' in args:
    event['body'] = args[args.index('--body') + 1]
with open(os.environ['EVENTS'], 'a') as f:
    f.write(json.dumps(event) + '\n')
if args[:2] == ['auth', 'setup-git']:
    pass
elif args and args[0] == 'api':
    path = next(x for x in args[1:] if x.startswith('repos/'))
    if '/commits' in path:
        # Each refusal case breaks exactly one conjunct of the bundle's signed-head check.
        n = int(path.split('/pulls/')[1].split('/')[0])
        sha = f'{n + 0x100:040x}' if case == 'head-moved' else f'{n:040x}'
        author = 'person' if case == 'mixed-author' else 'dependabot[bot]'
        committer = 'person' if case == 'foreign-committer' else 'web-flow'
        verified = case != 'unverified'
        print(json.dumps([[{'sha': sha, 'author': {'login': author}, 'committer': {'login': committer}, 'commit': {'verification': {'verified': verified}}}]]))
    else:
        if case == 'api-failure': sys.exit(19)
        count = 31 if case == 'pagination' else (0 if case in ('empty', 'no-ahead') else 1)
        rows = [{'number': n, 'title': f'update {n}', 'user': {'login': 'person' if case == 'foreign-pr' else 'dependabot[bot]'}, 'head': {'sha': f'{n:040x}'}} for n in range(1, count+1)]
        print(json.dumps([rows[:30], rows[30:]] if count > 30 else [rows]))
elif args[:2] == ['pr', 'list']:
    if '--head' in args:
        if case == 'all-conflict':
            print('[]')
        else:
            print(json.dumps([{'number': 77, 'body': 'previous #11 conflict #12'}]))
    else:
        if case == 'api-failure': sys.exit(19)
        count = 30 if case == 'pagination' else (0 if case == 'empty' else 1)
        print('\n'.join(f'{n}\tupdate {n}' for n in range(1,count+1)))
elif args[:2] == ['pr', 'view']:
    if 'headRefOid' in args:
        if case == 'close-query-failure': sys.exit(19)
        n = int(args[2])
        print(f'{n + (0x100 if case == "close-moved" else 0):040x}')
    else:
        print('previous #11 conflict #12')
elif args[:2] == ['pr', 'create']:
    print('https://github.com/fixture/plugin/pull/77')
elif args[:2] in (['pr', 'edit'], ['pr', 'merge'], ['pr', 'close']):
    if args[:2] == ['pr', 'merge'] and case == 'synchronize-on-close':
        with open(os.environ['EVENTS'], 'a') as f:
            f.write(json.dumps({'source_head_advanced': 'f' * 40}) + '\n')
    if args[:2] == ['pr', 'close'] and case == 'synchronize-on-close':
        with open(os.environ['EVENTS'], 'a') as f:
            f.write(json.dumps({'new_unbundled_head_closed': True}) + '\n')
else:
    raise SystemExit('unexpected gh invocation: ' + repr(args))
