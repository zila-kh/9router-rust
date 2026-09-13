#!/usr/bin/env python3
"""Hermetic storage normalization and actual launcher preflight regressions."""
import importlib.util
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest

ROOT = Path(__file__).resolve().parent.parent
SPEC = importlib.util.spec_from_file_location('storage', ROOT / 'scripts/normalize-storage-paths.py')
storage = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(storage)


class StoragePaths(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.cwd = Path(self.tmp.name).resolve()

    def normal(self, env):
        return storage.normalize(env, self.cwd, 'linux')

    def test_relative_paths_are_shared_and_absolute(self):
        result = self.normal({'DATA_DIR': 'data'})
        self.assertEqual(result['DATA_DIR'], str(self.cwd / 'data'))
        self.assertEqual(result['DATA_DIR'], result['NINEROUTER_DATA_DIR'])

    def test_equivalent_aliases_are_allowed(self):
        result = self.normal({'DATA_DIR': './data', 'NINEROUTER_DATA_DIR': str(self.cwd / 'data')})
        self.assertEqual(result['DATA_DIR'], str(self.cwd / 'data'))

    def test_conflicting_aliases_are_rejected(self):
        with self.assertRaisesRegex(ValueError, 'same directory'):
            self.normal({'DATA_DIR': 'data', 'NINEROUTER_DATA_DIR': 'other'})

    def test_empty_explicit_paths_are_rejected(self):
        for name in ('DATA_DIR', 'NINEROUTER_DATA_DIR', 'NINEROUTER_DB_PATH'):
            with self.subTest(name=name), self.assertRaisesRegex(ValueError, 'empty'):
                self.normal({name: ''})

    def test_compatibility_database_mismatch_is_rejected(self):
        with self.assertRaisesRegex(ValueError, 'Compatibility mode'):
            self.normal({'DATA_DIR': 'data', 'NINEROUTER_DB_PATH': 'other.sqlite'})

    def test_matching_compatibility_database_is_allowed(self):
        result = self.normal({'DATA_DIR': 'data', 'NINEROUTER_DB_PATH': 'data/db/data.sqlite'})
        self.assertEqual(result['NINEROUTER_DB_PATH'], str(self.cwd / 'data/db/data.sqlite'))

    def test_strict_mode_keeps_custom_database_support(self):
        result = self.normal({'NINEROUTER_COMPAT_API': '0', 'NINEROUTER_DB_PATH': 'custom.sqlite'})
        self.assertEqual(result['NINEROUTER_DB_PATH'], str(self.cwd / 'custom.sqlite'))

    def test_default_directory_is_shared(self):
        result = self.normal({'HOME': str(self.cwd)})
        self.assertEqual(result['DATA_DIR'], str(self.cwd / '.9router'))

    def test_boolean_modes_are_canonical_for_both_runtimes(self):
        for value in ('1', 'true', 'YES', ' On '):
            self.assertEqual(self.normal({'NINEROUTER_COMPAT_API': value})['NINEROUTER_COMPAT_API'], '1')
        for value in ('0', 'false', 'NO', ' Off '):
            self.assertEqual(self.normal({'NINEROUTER_COMPAT_API': value})['NINEROUTER_COMPAT_API'], '0')

    def test_invalid_modes_are_rejected(self):
        with self.assertRaisesRegex(ValueError, 'valid boolean'):
            self.normal({'NINEROUTER_COMPAT_API': 'sometimes'})

    def test_shell_metacharacters_remain_literal(self):
        value = "data ' $(touch SHOULD_NOT_EXIST); spaces\nnewline"
        exports = storage.shell_exports(self.normal({'DATA_DIR': value}))
        result = subprocess.run(['bash', '-c', 'set -e; eval "$1"; printf "%s" "$DATA_DIR"', 'test', exports],
                                cwd=self.cwd, check=True, capture_output=True, text=True)
        self.assertEqual(result.stdout, str(self.cwd / value))
        self.assertFalse((self.cwd / 'SHOULD_NOT_EXIST').exists())

    def test_all_launchers_normalize_before_build_or_start(self):
        scripts = self.cwd / 'scripts'
        scripts.mkdir()
        for name in ('run-dev.sh', 'run-prod.sh', 'run-full-stack-strict.sh', 'normalize-storage-paths.py'):
            shutil.copyfile(ROOT / 'scripts' / name, scripts / name)
        # Stop just after preflight: no real build, server, or network operations.
        (scripts / 'materialize-frontend.sh').write_text('python3 -c \'import os,json; print(json.dumps({k:os.environ[k] for k in ("DATA_DIR","NINEROUTER_DATA_DIR")}))\'\nexit 78\n')
        bin_dir = self.cwd / 'bin'
        bin_dir.mkdir()
        for name in ('cargo', 'npm', 'node', 'curl', 'git'):
            p = bin_dir / name
            p.write_text('#!/bin/sh\nexit 0\n')
            p.chmod(0o755)
        env = {k: v for k, v in os.environ.items() if not k.startswith('NINEROUTER_') and k not in ('DATA_DIR', 'PORT')}
        env.update(DATA_DIR='relative-data', PATH=str(bin_dir) + os.pathsep + env.get('PATH', ''))
        for name in ('run-dev.sh', 'run-prod.sh', 'run-full-stack-strict.sh'):
            with self.subTest(launcher=name):
                result = subprocess.run(['bash', str(scripts / name), str(self.cwd)], cwd=self.cwd,
                                        env=env, capture_output=True, text=True, timeout=5)
                self.assertEqual(result.returncode, 78, result.stderr)
                paths = json.loads(result.stdout)
                self.assertEqual(paths['DATA_DIR'], str(self.cwd / 'relative-data'))
                self.assertEqual(paths['DATA_DIR'], paths['NINEROUTER_DATA_DIR'])
        for values in ({'PORT': '0'}, {'PORT': '99999'}, {'PORT': '20129', 'NINEROUTER_UI_PORT': '20129'}):
            with self.subTest(strict_ports=values):
                result = subprocess.run(['bash', str(scripts / 'run-full-stack-strict.sh')], cwd=self.cwd,
                                        env={**env, **values}, capture_output=True, text=True, timeout=5)
                self.assertEqual(result.returncode, 2, result.stderr)


if __name__ == '__main__':
    unittest.main()
