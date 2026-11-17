#!/usr/bin/env python3
"""Exercise build orchestration without compiling or requiring a display."""
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[1]
NAMES = ['uflinux-files', 'uflinux-files-provider', 'uflinux-searchd', 'uflinux-files-mount']
MOCK = '''#!/usr/bin/env python3
import os, pathlib, subprocess, sys
p = pathlib.Path
args = sys.argv[1:]
tool = p(sys.argv[0]).name
if tool == 'id':
    print('1000')
elif tool == 'rustc':
    print('host: ' + os.environ['TEST_HOST'])
elif tool == 'cargo':
    if args == ['pgo', 'info']:
        sys.exit(0)
    flags = os.environ.get('RUSTFLAGS', '')
    assert '-Ctarget-cpu=generic' in flags, flags
    assert '-Csymbol-mangling-version=v0' in flags, flags
    assert flags.index('-Clink-arg=-Wl,-z,nopack-relative-relocs') > flags.index('-Clink-arg=-Wl,-z,pack-relative-relocs'), flags
    assert ('-Clink-arg=-Wl,-fini,0' in flags) == (os.environ['TEST_FINI_ARRAY'] == '1'), flags
    if args[:1] == ['pgo']:
        cargo_args = args[args.index('--') + 1:]
        subprocess.run(['cargo', 'build', *cargo_args], check=True)
        work = p(cargo_args[cargo_args.index('--target-dir') + 1])
        name = cargo_args[cargo_args.index('--bin') + 1]
        directory = work / ('bolt-profiles' if args[1] == 'bolt' else 'pgo-profiles')
        if args[1] != 'bolt' or args[2] == 'build':
            directory.mkdir(parents=True, exist_ok=True)
    else:
        assert args[:2] == ['auditable', 'build'], args
        work = p(args[args.index('--target-dir') + 1])
        name = args[args.index('--bin') + 1]
        target = args[args.index('--target') + 1]
        for suffix in ['', '-bolt-instrumented', '-bolt-optimized']:
            binary = work / target / 'release' / (name + suffix)
            binary.parent.mkdir(parents=True, exist_ok=True)
            binary.write_text(name + suffix)
            binary.chmod(0o755)
elif tool == 'python3':
    assert args == ['tests/files-pgo-symbols.py'], args
    if os.environ.get('TEST_FAIL') == 'symbols':
        sys.exit(1)
elif tool == 'timeout':
    binary = p(args[-1])
    name = binary.name.removesuffix('-bolt-instrumented').removesuffix('-bolt-optimized')
    phase = 'pgo' if binary.name == name else ('bolt' if binary.name.endswith('-instrumented') else 'final')
    if os.environ.get('TEST_FAIL') == name + ':' + phase:
        sys.exit(1 if phase == 'final' else 0)
    if phase == 'pgo':
        profile = p(os.environ['LLVM_PROFILE_FILE'].replace('%m', '1').replace('%p', '2'))
    elif phase == 'bolt':
        profile = binary.parents[2] / 'bolt-profiles' / name / 'profile.1.fdata'
    else:
        sys.exit(0)
    profile.parent.mkdir(parents=True, exist_ok=True)
    profile.write_text('profile')
'''


class DriverTests(unittest.TestCase):
    def run_driver(self, arch='x86_64', failure='', target=None, abi='musl'):
        with tempfile.TemporaryDirectory() as scratch:
            root = Path(scratch)
            tools = root / 'tools'
            tools.mkdir()
            for name in ['id', 'rustc', 'cargo', 'cargo-pgo', 'cargo-auditable', 'llvm-bolt',
                         'merge-fdata', 'labwc', 'dbus-run-session', 'gdbus', 'timeout', 'python3']:
                path = tools / name
                path.write_text(MOCK.replace('#!/usr/bin/env python3', '#!' + sys.executable, 1))
                path.chmod(0o755)
            host = arch + '-test-linux-' + abi
            output = root / 'custom-target'
            for name in NAMES:
                for directory, filename in [('pgo-profiles', 'stale.profraw'),
                                             ('bolt-profiles/' + name, 'stale.fdata')]:
                    stale = output / 'pgo-bolt' / host / name / directory / filename
                    stale.parent.mkdir(parents=True, exist_ok=True)
                    stale.write_text('stale')
            env = dict(os.environ, PATH=str(tools) + ':' + os.environ['PATH'],
                       CARGO_TARGET_DIR=str(output), TEST_HOST=host, TEST_FAIL=failure,
                       RUSTFLAGS='-Ctarget-cpu=generic -Clink-arg=-Wl,-z,pack-relative-relocs',
                       TEST_FINI_ARRAY=str(int(arch == 'aarch64' and abi == 'musl')))
            env.pop('CARGO_ENCODED_RUSTFLAGS', None)
            result = subprocess.run(['sh', str(ROOT / 'scripts/files-pgo-build.sh'), target or host],
                                    env=env, capture_output=True, text=True, timeout=30)
            if failure or target:
                self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
                self.assertFalse((output / 'release').exists())
            else:
                self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
                for name in NAMES:
                    self.assertEqual((output / 'release' / name).read_text(), name + '-bolt-optimized')
            return result

    def test_native_architectures_and_custom_directory(self):
        for arch in ['x86_64', 'aarch64']:
            with self.subTest(arch=arch):
                self.run_driver(arch)

    def test_gnu_targets_keep_their_legacy_fini_hook(self):
        for arch in ['x86_64', 'aarch64']:
            with self.subTest(arch=arch):
                self.run_driver(arch, abi='gnu')

    def test_every_executable_requires_profiles_and_final_check(self):
        for name in NAMES:
            for phase in ['pgo', 'bolt', 'final']:
                with self.subTest(name=name, phase=phase):
                    result = self.run_driver(failure=name + ':' + phase)
                    if phase != 'final':
                        self.assertIn('no profiles for ' + name, result.stderr)

    def test_symbol_check_fails_before_publishing(self):
        self.run_driver(failure='symbols')

    def test_cross_target_rejected(self):
        self.assertIn('requires a native build', self.run_driver(target='aarch64-test-linux-musl').stderr)


if __name__ == '__main__':
    unittest.main()
