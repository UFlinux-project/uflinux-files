#!/usr/bin/env python3
"""Isolated executable workloads for PGO, BOLT and final artifact verification."""
import ast
import base64
import json
import os
from pathlib import Path
import signal
import subprocess
import sys
import tempfile
import time


def call(interface, path, method, body):
    result = subprocess.run(
        ['gdbus', 'call', '--session', '--dest', interface, '--object-path', path,
         '--method', interface + '.' + method, json.dumps(body)],
        check=True, capture_output=True, text=True, timeout=10)
    return json.loads(ast.literal_eval(result.stdout)[0])


def until(predicate, process=None):
    deadline = time.monotonic() + 45
    while time.monotonic() < deadline:
        if process is not None and process.poll() is not None:
            raise RuntimeError('service exited before workload completed')
        try:
            if predicate():
                return
        except subprocess.CalledProcessError:
            pass  # The bus name may not yet be registered.
        time.sleep(0.1)
    raise RuntimeError('timed out waiting for workload result')


def provider(binary, root):
    desktop = root / 'home/Desktop'
    desktop.mkdir(parents=True)
    for i in range(32):
        (desktop / f'document-{i}.txt').write_text('training\n')
    picture = desktop / 'picture.png'
    picture.write_bytes(base64.b64decode(
        'iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+ip1sAAAAASUVORK5CYII='))
    def ask(method, **body):
        names = {'GetModel': 'get-model', 'SetGeometry': 'set-geometry',
                 'RequestThumbnail': 'request-thumbnail'}
        return call('org.uflinux.FileManager.Desktop1', '/org/uflinux/FileManager/Desktop',
                    method, {'method': names.get(method, method.lower()), **body})
    def workload(process):
        until(lambda: ask('Status'), process)
        ask('SetGeometry', monitors=[dict(monitor=0, columns=8, rows=8, cell_w=96, cell_h=96)])
        assert 'document-0.txt' in json.dumps(ask('GetModel'))
        ask('Place', id='document-0.txt', monitor=0, col=2, row=3)
        ask('RequestThumbnail', path=str(picture), size=128)
        (desktop / 'new-file.txt').write_text('watcher\n')
        until(lambda: 'new-file.txt' in json.dumps(ask('GetModel')), process)
        (desktop / 'new-file.txt').unlink()
        until(lambda: 'new-file.txt' not in json.dumps(ask('GetModel')), process)
    serve(binary, root, workload)


def search(binary, root):
    tree = root / 'documents'
    tree.mkdir()
    for i in range(96):
        (tree / f'document-{i}.txt').write_text('uniquecontent training text\n')
    (tree / '.hidden.txt').write_text('hidden training\n')
    apps = root / 'data/applications'
    apps.mkdir()
    (apps / 'training.desktop').write_text(
        '[Desktop Entry]\nType=Application\nName=Training Application\nExec=/bin/true\nCategories=Utility;\n')
    (root / 'config/index.json').write_text(json.dumps({
        'roots': [{'path': str(tree), 'content': 'text'}], 'idle_exit_secs': 120}))
    def ask(method, **body):
        names = {'SearchFiles': 'search-files', 'SearchApps': 'search-apps', 'Reindex': 'reindex'}
        return call('org.uflinux.Search1', '/org/uflinux/Search', method,
                    {'method': names.get(method, method.lower()), **body})
    def workload(process):
        until(lambda: not ask('Status')['busy'], process)
        until(lambda: ask('SearchFiles', text='document')['total'] >= 96, process)
        for order in ['relevance', 'name-asc', 'size-desc', 'modified-desc']:
            reply = ask('SearchFiles', text='document', scope=str(tree), order=order,
                        page=1, page_size=8, mimes=['text/plain'])
            assert reply['hits']
        assert ask('SearchFiles', text='uniquecontent', content=True)['total'] >= 96
        assert ask('SearchFiles', text='hidden', include_hidden=True)['total'] >= 1
        until(lambda: ask('SearchApps', text='Training')['total'] >= 1, process)
        (tree / 'watcherunique.txt').write_text('watcher\n')
        until(lambda: ask('SearchFiles', text='watcherunique')['total'] == 1, process)
        (tree / 'watcherunique.txt').unlink()
        until(lambda: ask('SearchFiles', text='watcherunique')['total'] == 0, process)
        ask('Reindex')
        until(lambda: not ask('Status')['busy'], process)
        assert ask('SearchFiles', text='document')['total'] >= 96
    serve(binary, root, workload)


def serve(binary, root, workload):
    with (root / 'service.log').open('w+') as log:
        process = subprocess.Popen([binary], stdout=log, stderr=log)
        try:
            workload(process)
            process.send_signal(signal.SIGTERM)
            assert process.wait(timeout=20) == 0
        except BaseException:
            log.seek(0)
            print(log.read(), file=sys.stderr)
            raise
        finally:
            if process.poll() is None:
                process.terminate()
                try:
                    process.wait(timeout=10)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait()


def mount(binary, root):
    if os.geteuid() == 0:
        raise RuntimeError('mount training must run as an ordinary user')
    target = root / 'mountpoint'
    target.mkdir(mode=0o700)
    def run(args, expected):
        result = subprocess.run([binary, *args], capture_output=True, text=True, timeout=10)
        assert result.returncode == expected, (args, result.returncode, result.stderr)
    run(['--help'], 0)
    run([], 2)
    for kind, source in [('nfs', 'localhost:/training'), ('cifs', '//localhost/training')]:
        args = ['unmount', '--type', kind, '--source', source, '--target', str(target), '--options', '']
        run(args, 0)  # Absent mount: validates everything without executing umount.
        run(args[:-1] + ['password=forbidden'], 2)
        run(args + ['--type', kind], 2)
        run(['mount', '--type', kind, '--source', '-invalid', '--target', str(target), '--options', ''], 2)
    run(['device-mount'], 2)


def main():
    name, binary = sys.argv[1:]
    binary = str(Path(binary).resolve())
    with tempfile.TemporaryDirectory(prefix='ufm-pgo-') as scratch:
        root = Path(scratch)
        for key, directory in [('HOME', 'home'), ('XDG_RUNTIME_DIR', 'runtime'),
                               ('XDG_CONFIG_HOME', 'config'), ('XDG_DATA_HOME', 'data'),
                               ('XDG_CACHE_HOME', 'cache')]:
            (root / directory).mkdir(mode=0o700)
            os.environ[key] = str(root / directory)
        os.environ.update(UFLINUX_FILES_CONFIG_DIR=str(root / 'config'),
                          UFLINUX_FILES_INDEX_DIR=str(root / 'cache/index'),
                          XDG_DATA_DIRS=str(root / 'data'))
        {'uflinux-files-provider': provider, 'uflinux-searchd': search,
         'uflinux-files-mount': mount}[name](binary, root)
    print('Training passed:', name)


if __name__ == '__main__':
    main()
