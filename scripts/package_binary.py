#!/usr/bin/env python3
"""Package a clean, pinned Linux executable and its GitHub build provenance."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import struct
import subprocess
import tomllib


TARGETS = {
    'x86_64': ('x86_64-unknown-linux-musl', 62),
    'aarch64': ('aarch64-unknown-linux-musl', 183),
}


def inspect_binary(path, machine):
    data = Path(path).read_bytes()
    if len(data) < 64 or data[:6] != b'\x7fELF\x02\x01':
        raise ValueError('Expected a little-endian ELF64 Linux executable')
    if struct.unpack_from('<H', data, 18)[0] != TARGETS[machine][1]:
        raise ValueError('Executable architecture differs from the selected platform')
    offset = struct.unpack_from('<Q', data, 32)[0]
    size, count = struct.unpack_from('<HH', data, 54)
    if size < 56 or count == 0 or offset + size * count > len(data):
        raise ValueError('Executable has an invalid ELF program table')
    if any(struct.unpack_from('<I', data, offset + size * i)[0] == 3 for i in range(count)):
        raise ValueError('Managed Linux releases must not require a dynamic interpreter')
    return hashlib.sha256(data).hexdigest()


def package(binary, output, machine, revision, environment=None, root=None):
    root = Path(root or Path(__file__).resolve().parents[1])
    environment = os.environ if environment is None else environment
    if not re.fullmatch(r'[0-9a-f]{40}', revision):
        raise ValueError('Supply the full reviewed source revision')
    actual = subprocess.check_output(['git', '-C', str(root), 'rev-parse', 'HEAD'], text=True).strip()
    if actual != revision or subprocess.check_output(
            ['git', '-C', str(root), 'status', '--porcelain', '--untracked-files=all'], text=True).strip():
        raise ValueError('Packaging requires a clean checkout at the selected source revision')
    version = tomllib.loads((root / 'Cargo.toml').read_text())['package']['version']
    identity = subprocess.check_output([str(Path(binary).resolve()), '--version'], text=True).strip()
    if (not identity.startswith('sequencer-sync ' + version + ' commit ' + revision + ' at ')
            or 'dirty' in identity or 'VERGEN_IDEMPOTENT_OUTPUT' in identity):
        raise ValueError('Executable version and embedded commit must match the clean source')
    repository = environment.get('GITHUB_REPOSITORY', '')
    run_id = environment.get('GITHUB_RUN_ID', '')
    attempt = environment.get('GITHUB_RUN_ATTEMPT', '')
    if (repository != 'ssi-dk/sequencer-sync' or not run_id.isdigit()
            or not attempt.isdigit() or environment.get('GITHUB_SHA') != revision):
        raise ValueError('Managed packaging requires matching GitHub Actions publication inputs')
    filename = 'sequencer-sync-linux-' + machine
    checksum = inspect_binary(binary, machine)
    manifest = {
        'schema_version': 1, 'application': 'sequencer-sync',
        'application_version': version, 'source_revision': revision, 'source_kind': 'git',
        'platform': {'system': 'Linux', 'machine': machine, 'target': TARGETS[machine][0]},
        'binary': {'filename': filename, 'sha256': checksum},
        'publication': {
            'provider': 'github-actions', 'repository': repository, 'run_id': run_id,
            'run_attempt': attempt, 'run_url': f'https://github.com/{repository}/actions/runs/{run_id}',
            'workflow': '.github/workflows/build-standalone.yml',
            'ref': environment.get('GITHUB_REF', ''),
        },
    }
    output = Path(output)
    output.mkdir(parents=True, exist_ok=True)
    destination = output / filename
    # Refuse an existing build rather than mix files from separate attempts.
    with destination.open('xb') as stream:
        stream.write(Path(binary).read_bytes())
    destination.chmod(0o755)
    (output / (filename + '.manifest.json')).write_text(json.dumps(manifest, indent=2) + '\n')
    (output / (filename + '.sha256')).write_text(checksum + '  ' + filename + '\n')
    return manifest


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary', required=True, type=Path)
    parser.add_argument('--output', required=True, type=Path)
    parser.add_argument('--machine', required=True, choices=TARGETS)
    parser.add_argument('--revision', required=True)
    args = parser.parse_args()
    try:
        print(json.dumps(package(args.binary, args.output, args.machine, args.revision)))
    except (ValueError, OSError, subprocess.CalledProcessError) as error:
        parser.exit(1, str(error) + '\n')


if __name__ == '__main__':
    main()
