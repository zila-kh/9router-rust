#!/usr/bin/env python3
"""Emit shell-quoted shared storage paths before launchers change directories."""
import os
from pathlib import Path
import shlex
import sys


def normalize(env, cwd, platform=sys.platform):
    def absolute(value):
        p = Path(value)
        return (p if p.is_absolute() else cwd / p).resolve()

    names = ('NINEROUTER_DATA_DIR', 'DATA_DIR', 'NINEROUTER_DB_PATH')
    for name in names:
        if name in env and not env[name]:
            raise ValueError(f'{name} must not be empty')
    configured = [absolute(env[n]) for n in names[:2] if n in env]
    if len(configured) == 2 and configured[0] != configured[1]:
        raise ValueError('NINEROUTER_DATA_DIR and DATA_DIR must point to the same directory')
    if configured:
        data = configured[0]
    elif platform == 'win32':
        home = Path(env.get('USERPROFILE', str(Path.home())))
        data = absolute(Path(env.get('APPDATA', str(home / 'AppData' / 'Roaming'))) / '9router')
    else:
        data = absolute(Path(env.get('HOME', str(Path.home()))) / '.9router')
    mode = env.get('NINEROUTER_COMPAT_API', '1').strip().lower()
    if mode not in ('1', 'true', 'yes', 'on', '0', 'false', 'no', 'off'):
        raise ValueError('NINEROUTER_COMPAT_API must be a valid boolean')
    result = {'NINEROUTER_DATA_DIR': str(data), 'DATA_DIR': str(data),
              'NINEROUTER_COMPAT_API': '1' if mode in ('1', 'true', 'yes', 'on') else '0'}
    if 'NINEROUTER_DB_PATH' in env:
        db = absolute(env['NINEROUTER_DB_PATH'])
        if mode in ('1', 'true', 'yes', 'on') and db != (data / 'db' / 'data.sqlite').resolve():
            raise ValueError('Compatibility mode requires NINEROUTER_DB_PATH to match DATA_DIR/db/data.sqlite')
        result['NINEROUTER_DB_PATH'] = str(db)
    return result


def shell_exports(values):
    return '\n'.join(f'export {name}={shlex.quote(value)}' for name, value in values.items())


if __name__ == '__main__':
    try:
        print(shell_exports(normalize(os.environ, Path.cwd())))
    except (ValueError, OSError, RuntimeError) as error:
        print(f'error: {error}', file=sys.stderr)
        sys.exit(2)
