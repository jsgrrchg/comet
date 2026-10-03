"""Exercise relocatable runtime packages and reject incompatible or altered inputs."""
import importlib.util
import io
import json
import os
import shutil
import subprocess
import sys
import tarfile
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

SPEC = importlib.util.spec_from_file_location('voice_packaging', Path(__file__).parents[1] / 'package-voice-runtime.py')
packager = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(packager)


def write_json(path, value):
    path.write_text(json.dumps(value))


def fixture(root, target):
    app_target = packager.PACKAGES[target][0]
    voice = root / 'codex-resources/voice'
    helper = 'bin/codex-voice-host' + ('.exe' if target.endswith('windows-msvc') else '')
    extension = '.dll' if target.endswith('windows-msvc') else '.dylib' if target.endswith('apple-darwin') else '.so'
    library = 'lib/native' + extension
    files = {
        'bin/codex': b'CLI: must never be copied',
        'codex-resources/voice/' + helper: b'helper',
        'codex-resources/voice/' + library: b'library',
        'codex-resources/voice/NOTICE.md': b'notices',
        'codex-resources/voice/sources.json': b'{}',
        'codex-resources/voice/licenses/Opus.txt': b'upstream license',
    }
    for name, content in files.items():
        path = root / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(content)
    (voice / helper).chmod(0o755)
    write_json(root / 'codex-package.json', {'layoutVersion': 1, 'version': packager.VERSION, 'target': app_target})
    write_json(voice / 'runtime.json', {
        'target': target, 'sourceCommit': packager.BUILD,
        'libraries': [{'path': library, 'sha256': packager.checksum(voice / library)}],
    })
    refresh_manifest(root, target)
    return voice, helper, library


def refresh_manifest(root, target):
    hashes = {p.relative_to(root).as_posix(): packager.checksum(p)
              for p in root.rglob('*') if p.is_file() and p.name not in ('manifest.json', 'codex-package.json')}
    write_json(root / 'codex-resources/voice/manifest.json', {
        'buildCommit': packager.BUILD, 'appVersion': packager.VERSION,
        'appTarget': packager.PACKAGES[target][0], 'voiceTarget': target, 'sha256': hashes,
    })


class RuntimePackagingTests(unittest.TestCase):
    @unittest.skipUnless(sys.platform.startswith('linux'), 'Linux package/install integration')
    def test_linux_tarball_and_install_preserve_runtime(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            project = root / 'project'
            repository = Path(__file__).parents[2]
            files = {
                'Cargo.toml': 'version = "9.9.9"',
                'dist/zeron.desktop': '[Desktop Entry]\nExec=zeron\nTryExec=zeron\nIcon=zeron\n',
                'dist/zeron.png': 'icon',
                'dist/voice/Codex-LICENSE.txt': 'helper license',
                'crates/ui/assets/fonts/licenses/Font.txt': 'font license',
                'crates/voice/NOTICE.md': 'dictation notice',
                'target/debug/zeron': '#!/bin/sh\nexit 0\n',
                'shim/cargo': '#!/bin/sh\nexit 0\n',
            }
            for name, content in files.items():
                path = project / name
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_text(content)
            for name in ['target/debug/zeron', 'shim/cargo']:
                (project / name).chmod(0o755)
            (project / 'scripts').mkdir()
            for script in ['package-linux.sh', 'package-voice-runtime.py']:
                shutil.copy2(repository / 'scripts' / script, project / 'scripts' / script)
            target = packager.host_target()
            _, helper, library = fixture(root / 'source', target)
            env = dict(os.environ, PROFILE='debug', ZERON_VOICE_RUNTIME_PACKAGE=str(root / 'source'))
            env['PATH'] = str(project / 'shim') + os.pathsep + env['PATH']
            subprocess.run(['bash', str(project / 'scripts/package-linux.sh')], env=env, check=True, capture_output=True)
            archive = next((project / 'target/package').glob('*.tar.gz'))
            with tarfile.open(archive) as package:
                package.extractall(root / 'extracted', filter='data')
            unpacked = next((root / 'extracted').iterdir())
            runtime = unpacked / 'codex-resources/voice'
            self.assertTrue((runtime / helper).is_file())
            self.assertTrue((runtime / library).is_file())
            env['HOME'] = str(root / 'isolated home')
            env['XDG_DATA_HOME'] = str(root / 'isolated data')
            subprocess.run(['bash', str(unpacked / 'install.sh')], env=env, check=True, capture_output=True)
            installed = Path(env['HOME']) / '.zeron/app/current/codex-resources/voice'
            manifest = json.loads((installed / 'zeron-runtime.json').read_text())
            for name, digest in manifest['sha256'].items():
                self.assertEqual(packager.checksum(installed / name), digest)
            self.assertTrue((installed / helper).stat().st_mode & 0o111)

    def test_all_platforms_preserve_complete_runtime_without_copying_cli(self):
        for target in packager.PACKAGES:
            with self.subTest(target=target), tempfile.TemporaryDirectory() as temporary:
                root = Path(temporary)
                source, helper, library = fixture(root / 'source', target)
                destination = root / 'application with spaces/codex-resources/voice'
                with patch.object(packager.subprocess, 'run') as codesign:
                    packager.install(root / 'source', destination, target)
                self.assertEqual(codesign.call_count, 2 if target.endswith('apple-darwin') else 0)
                manifest = json.loads((destination / 'zeron-runtime.json').read_text())
                self.assertEqual(manifest['target'], target)
                for path in source.rglob('*'):
                    if path.is_file():
                        self.assertEqual(path.read_bytes(), (destination / path.relative_to(source)).read_bytes())
                self.assertIn(helper, manifest['sha256'])
                self.assertIn(library, manifest['sha256'])
                self.assertIn('licenses/Codex-LICENSE.txt', manifest['sha256'])
                for name, digest in manifest['sha256'].items():
                    self.assertNotIn('\\', name)
                    self.assertEqual(packager.checksum(destination / name), digest)
                self.assertFalse((destination / 'bin/codex').exists())
                if not target.endswith('windows-msvc'):
                    self.assertTrue((destination / helper).stat().st_mode & 0o111)

    def test_platform_and_version_mismatches_do_not_replace_existing_runtime(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            target = 'x86_64-unknown-linux-gnu'
            fixture(root / 'source', target)
            destination = root / 'app/codex-resources/voice'
            destination.mkdir(parents=True)
            (destination / 'previous').write_text('keep')
            for wrong in ['aarch64-unknown-linux-gnu', 'x86_64-pc-windows-msvc']:
                with self.assertRaises(ValueError):
                    packager.install(root / 'source', destination, wrong)
            metadata = json.loads((root / 'source/codex-package.json').read_text())
            metadata['version'] = '0.159.0'
            write_json(root / 'source/codex-package.json', metadata)
            with self.assertRaises(ValueError):
                packager.install(root / 'source', destination, target)
            self.assertEqual((destination / 'previous').read_text(), 'keep')

    def test_tampered_and_untracked_files_are_rejected(self):
        for kind in ['tampered', 'untracked', 'symlink', 'missing']:
            with self.subTest(kind=kind), tempfile.TemporaryDirectory() as temporary:
                root = Path(temporary)
                target = 'x86_64-pc-windows-msvc'
                source, helper, _ = fixture(root / 'source', target)
                if kind == 'tampered':
                    (source / helper).write_bytes(b'altered')
                elif kind == 'untracked':
                    (source / 'untracked.dll').write_bytes(b'altered')
                elif kind == 'missing':
                    (source / helper).unlink()
                else:
                    if not hasattr(os, 'symlink'):
                        continue
                    try:
                        (source / 'linked').symlink_to(source / helper)
                    except OSError:
                        continue  # Windows may not grant symlink creation.
                with self.assertRaises((ValueError, FileNotFoundError)):
                    packager.install(root / 'source', root / 'app/codex-resources/voice', target)

    def test_inconsistent_library_inventory_is_rejected_before_replacement(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            target = 'x86_64-unknown-linux-gnu'
            source, _, _ = fixture(root / 'source', target)
            runtime = json.loads((source / 'runtime.json').read_text())
            runtime['libraries'][0]['sha256'] = '0' * 64
            write_json(source / 'runtime.json', runtime)
            refresh_manifest(root / 'source', target)
            destination = root / 'app/codex-resources/voice'
            destination.mkdir(parents=True)
            (destination / 'previous').write_text('keep')
            with self.assertRaisesRegex(ValueError, 'runtime library'):
                packager.install(root / 'source', destination, target)
            self.assertEqual((destination / 'previous').read_text(), 'keep')

    def test_manifest_paths_cannot_escape_on_either_platform(self):
        for name in ['../outside', '/absolute', 'C:/outside', 'C:outside', 'lib\\outside.dll', 'lib/../outside', '.', 'lib//file']:
            with self.subTest(name=name), self.assertRaises(ValueError):
                packager.relative_path(name)

    def test_download_checksum_failure_prevents_extraction(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            with patch.object(packager.urllib.request, 'urlopen', return_value=io.BytesIO(b'wrong archive')):
                with self.assertRaisesRegex(ValueError, 'archive checksum'):
                    packager.download_package(root, 'x86_64-pc-windows-msvc')
            self.assertFalse((root / 'package').exists())


if __name__ == '__main__':
    unittest.main()
