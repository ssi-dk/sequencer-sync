"""Check release identity and architecture without executing downloaded programs."""
import importlib.util
import json
from pathlib import Path
import struct
import tempfile
import unittest
from unittest.mock import patch

SPEC = importlib.util.spec_from_file_location(
    'package_binary', Path(__file__).resolve().parents[1] / 'scripts/package_binary.py')
packaging = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(packaging)


def elf(machine=62, interpreter=False):
    data = bytearray(120)
    data[:6] = b'\x7fELF\x02\x01'
    struct.pack_into('<H', data, 18, machine)
    struct.pack_into('<Q', data, 32, 64)
    struct.pack_into('<HH', data, 54, 56, 1)
    struct.pack_into('<I', data, 64, 3 if interpreter else 1)
    return data


class PackageBinaryTest(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.binary = self.root / 'program'
        self.binary.write_bytes(elf())
        (self.root / 'Cargo.toml').write_text('[package]\nversion="3.0.5"\n')
        self.revision = 'a' * 40
        self.environment = dict(GITHUB_REPOSITORY='ssi-dk/sequencer-sync', GITHUB_RUN_ID='123',
                                GITHUB_RUN_ATTEMPT='1', GITHUB_SHA=self.revision, GITHUB_REF='refs/tags/test-fixture')

    def package(self, dirty='', identity=None):
        identity = identity or 'sequencer-sync 3.0.5 commit ' + self.revision + ' at 2026-10-28'
        with patch.object(packaging.subprocess, 'check_output', side_effect=[self.revision, dirty, identity]):
            return packaging.package(self.binary, self.root / 'dist', 'x86_64', self.revision,
                                     self.environment, self.root)

    def test_published_bytes_and_sidecars_share_one_identity(self):
        manifest = self.package()
        output = self.root / 'dist'
        filename = manifest['binary']['filename']
        self.assertEqual((output / filename).read_bytes(), self.binary.read_bytes())
        self.assertEqual((output / (filename + '.sha256')).read_text(),
                         manifest['binary']['sha256'] + '  ' + filename + '\n')
        self.assertEqual(json.loads((output / (filename + '.manifest.json')).read_text()), manifest)

    def test_dirty_source_is_not_published(self):
        with self.assertRaisesRegex(ValueError, 'clean checkout'):
            self.package(dirty=' M Cargo.toml')
        self.assertFalse((self.root / 'dist').exists())

    def test_missing_embedded_source_is_not_published(self):
        for identity in ['sequencer-sync 3.0.5 commit VERGEN_IDEMPOTENT_OUTPUT',
                         'sequencer-sync 3.0.5 commit ' + self.revision[:7] + ' at 2026-10-28',
                         'sequencer-sync 3.0.5 commit ' + 'b' * 40 + ' at 2026-10-28',
                         'sequencer-sync 3.0.5 commit ' + self.revision + ' at 2026-10-28 (dirty git repository)']:
            with self.subTest(identity=identity), self.assertRaises(ValueError):
                self.package(identity=identity)

    def test_wrong_architecture_and_dynamic_interpreter_are_rejected(self):
        for contents in [elf(183), elf(interpreter=True), b'#!/bin/sh\n']:
            self.binary.write_bytes(contents)
            with self.assertRaises(ValueError):
                packaging.inspect_binary(self.binary, 'x86_64')

    def test_publication_source_must_match(self):
        self.environment['GITHUB_SHA'] = 'b' * 40
        with self.assertRaisesRegex(ValueError, 'GitHub Actions'):
            self.package()


if __name__ == '__main__':
    unittest.main()
