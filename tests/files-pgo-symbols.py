#!/usr/bin/env python3
"""Check that real Rust symbols can be instrumented and profiled by BOLT."""
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest

# Legacy drop_in_place monomorphizations contain `.warming` and `.cold_cache`.
# BOLT 22's unanchored fragment matcher interprets those as split functions and
# aborts with "parent function not found". Neither function is actually split.
SOURCE = """
mod warming {
    pub struct WarmingStateInner(pub u64);
    impl Drop for WarmingStateInner {
        #[inline(never)]
        fn drop(&mut self) { std::hint::black_box(self.0); }
    }
}
mod cold_cache {
    pub struct Cache(pub u64);
    impl Drop for Cache {
        #[inline(never)]
        fn drop(&mut self) { std::hint::black_box(self.0); }
    }
}
#[inline(never)]
fn drop_value<T>(value: T) { std::hint::black_box(value); }
fn main() {
    drop_value(warming::WarmingStateInner(7));
    drop_value(cold_cache::Cache(9));
}
"""


@unittest.skipUnless(sys.platform.startswith('linux') and
                     shutil.which('rustc') and shutil.which('llvm-bolt'),
                     'requires Linux, rustc and LLVM BOLT')
class SymbolTests(unittest.TestCase):
    def run_command(self, args):
        result = subprocess.run(args, stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
                                text=True, timeout=60)
        self.assertEqual(result.returncode, 0, f'{args}\n{result.stdout}')
        return result.stdout

    def test_module_names_do_not_become_split_functions(self):
        host = self.run_command(['rustc', '-vV']).split('host: ', 1)[1].splitlines()[0]
        if not host.startswith(('x86_64-', 'aarch64-')):
            self.skipTest('requires an x86_64 or aarch64 toolchain')
        with tempfile.TemporaryDirectory(prefix='ufm-bolt-symbols-') as scratch:
            root = Path(scratch)
            source = root / 'symbols.rs'
            source.write_text(SOURCE)
            binary = root / 'symbols'
            instrumented = root / 'symbols-instrumented'
            profile = root / 'profile.fdata'
            flags = ['-Csymbol-mangling-version=v0', '-Copt-level=0', '-Cpanic=abort',
                     '-Cstrip=none', '-Ctarget-feature=-crt-static',
                     '-Clink-arg=-Wl,-q', '-Clink-arg=-Wl,-z,nopack-relative-relocs']
            if host.startswith('aarch64-') and 'linux-musl' in host:
                flags.append('-Clink-arg=-Wl,-fini,0')
            self.run_command(['rustc', str(source), '--crate-name', 'symbols',
                              *flags, '-o', str(binary)])
            self.run_command(['llvm-bolt', str(binary), '-instrument',
                              '--instrumentation-file=' + str(profile),
                              '--instrumentation-binpath=' + str(instrumented),
                              '-o', str(instrumented)])
            self.run_command([str(instrumented)])
            self.assertTrue(profile.is_file(), 'BOLT did not flush its profile')
            self.assertGreater(profile.stat().st_size, 0, 'BOLT profile is empty')


if __name__ == '__main__':
    unittest.main()
