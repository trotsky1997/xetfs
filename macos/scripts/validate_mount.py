#!/usr/bin/env python3
"""Exercise a fresh, isolated XetFS mount without touching existing files."""
import fcntl
import mmap
import os
from pathlib import Path
import sqlite3
import subprocess
import sys
import tempfile

if not __debug__:
    raise SystemExit("Run without Python optimization so validation checks remain enabled.")


def child(code, path):
    return subprocess.run([sys.executable, '-c', code, str(path)], capture_output=True, text=True, timeout=15)


def run_checks(root):
    with tempfile.TemporaryDirectory(prefix='.xetfs-validation-', dir=root) as temporary:
        path = Path(temporary)
        original = path / 'file'
        original.write_bytes(b'original')
        hardlink = path / 'hardlink'
        os.link(original, hardlink)
        assert original.stat().st_ino == hardlink.stat().st_ino
        os.symlink('file', path / 'symlink')
        assert (path / 'symlink').read_bytes() == b'original'
        subprocess.run(['xattr', '-w', 'user.xetfs-test', 'value', str(original)], check=True, capture_output=True)
        assert subprocess.check_output(['xattr', '-p', 'user.xetfs-test', str(hardlink)]).strip() == b'value'
        with open(original, 'r+b') as opened:
            with mmap.mmap(opened.fileno(), 0) as mapping:
                mapping[:4] = b'EDIT'
                mapping.flush()
            opened.flush()
            os.fsync(opened.fileno())
        assert hardlink.read_bytes() == b'EDITinal'
        with open(original, 'rb') as opened:
            original.unlink()
            assert opened.read() == b'EDITinal'
        renamed = path / 'renamed'
        hardlink.rename(renamed)
        assert renamed.read_bytes() == b'EDITinal'
        with open(renamed, 'rb') as held:
            fcntl.flock(held, fcntl.LOCK_EX | fcntl.LOCK_NB)
            result = child('import fcntl,sys; f=open(sys.argv[1],"rb"); fcntl.flock(f, fcntl.LOCK_EX | fcntl.LOCK_NB)', renamed)
            assert result.returncode != 0 and ('BlockingIOError' in result.stderr or 'Resource temporarily unavailable' in result.stderr), result.stderr
        database = path / 'test.sqlite'
        with sqlite3.connect(database) as db:
            db.execute('create table data (value text)')
            db.execute("insert into data values ('kept')")
            db.commit()
            db.execute('begin immediate')
            probe = "import sqlite3,sys; d=sqlite3.connect(sys.argv[1],timeout=0); d.execute('begin immediate')"
            result = child(probe, database)
            assert result.returncode != 0 and 'locked' in result.stderr.lower(), result.stderr
            db.rollback()
        with sqlite3.connect(database) as db:
            assert db.execute('select value from data').fetchall() == [('kept',)]
        git = path / 'git'
        git.mkdir()
        env = os.environ.copy()
        env.update({'GIT_CONFIG_NOSYSTEM':'1', 'GIT_CONFIG_GLOBAL':os.devnull, 'GIT_AUTHOR_NAME':'XetFS validation', 'GIT_AUTHOR_EMAIL':'xetfs-validation@invalid', 'GIT_COMMITTER_NAME':'XetFS validation', 'GIT_COMMITTER_EMAIL':'xetfs-validation@invalid'})
        def run_git(*args):
            subprocess.run(['git', '-C', str(git), *args], env=env, check=True, capture_output=True, timeout=30)
        run_git('init', '-q')
        (git / 'tracked').write_text('before\n')
        run_git('add', 'tracked')
        run_git('commit', '-qm', 'validation')
        (git / 'tracked').write_text('after\n')
        run_git('checkout', '--', 'tracked')
        assert (git / 'tracked').read_text() == 'before\n'
    print('Passed: read/write, fsync, mmap, links, xattrs, open-unlink, flock, SQLite locking, Git')


def main():
    if len(sys.argv) != 2:
        raise SystemExit('Usage: validate_mount.py EMPTY_XETFS_MOUNTPOINT')
    root = Path(sys.argv[1]).resolve(strict=True)
    if not root.is_dir() or not os.path.ismount(root):
        raise SystemExit('Expected an actual mountpoint.')
    entries = subprocess.check_output(['/sbin/mount'], text=True).splitlines()
    if not any(f' on {root} (xetfs,' in line or line.endswith(f' on {root} (xetfs)') for line in entries):
        raise SystemExit('Expected a mounted XetFS volume.')
    if next(root.iterdir(), None) is not None:
        raise SystemExit('Expected a fresh, empty isolated volume.')
    run_checks(root)


if __name__ == '__main__':
    main()
