#!/usr/bin/env python3
"""Sign a fresh copy of the FSKit app with explicitly supplied profiles."""
import argparse
import datetime
import hashlib
import plistlib
from pathlib import Path
import re
import shutil
import subprocess
import tempfile

HERE = Path(__file__).resolve().parent


def command(*args):
    return subprocess.run(args, check=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE).stdout


def profile(path, identity, required):
    info = plistlib.loads(command('security', 'cms', '-D', '-i', str(path)))
    if info['ExpirationDate'].replace(tzinfo=datetime.timezone.utc) <= datetime.datetime.now(datetime.timezone.utc):
        raise ValueError(f'Expired provisioning profile: {path}')
    teams = info.get('TeamIdentifier', [])
    entitlements = info.get('Entitlements', {})
    app_id = entitlements.get('com.apple.application-identifier') or entitlements.get('application-identifier')
    if len(teams) != 1 or not isinstance(app_id, str) or not app_id.startswith(teams[0] + '.') or '*' in app_id:
        raise ValueError(f'Profile must have one team and an explicit application identifier: {path}')
    if required is not None and entitlements.get(required) is not True:
        raise ValueError(f'Profile lacks {required}: {path}')
    certificates = {hashlib.sha1(cert).hexdigest().upper() for cert in info.get('DeveloperCertificates', [])}
    if identity not in certificates:
        raise ValueError(f'Signing certificate is absent from profile: {path}')
    return info, teams[0], app_id[len(teams[0]) + 1:]


def signing_entitlements(path, profile_info):
    with path.open('rb') as file:
        wanted = plistlib.load(file)
    source = profile_info['Entitlements']
    for key in ('com.apple.application-identifier', 'application-identifier', 'com.apple.developer.team-identifier', 'get-task-allow'):
        if key in source:
            wanted[key] = source[key]
    return wanted


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('unsigned_app', type=Path)
    parser.add_argument('new_output_directory', type=Path)
    parser.add_argument('identity', help='40-character SHA-1 code signing identity')
    parser.add_argument('host_profile', type=Path)
    parser.add_argument('extension_profile', type=Path)
    args = parser.parse_args()
    identity = args.identity.upper()
    if not re.fullmatch(r'[0-9A-F]{40}', identity):
        parser.error('identity must be a 40-character SHA-1 fingerprint')
    available = command('security', 'find-identity', '-v', '-p', 'codesigning').decode('utf-8', errors='replace')
    if not re.search(r'\b' + identity + r'\b', available):
        raise ValueError('The signing identity is not available in the keychain')
    host, host_team, host_id = profile(args.host_profile, identity, None)
    extension, ext_team, ext_id = profile(args.extension_profile, identity, 'com.apple.developer.fskit.fsmodule')
    if host_team != ext_team or ext_id != host_id + '.filesystem':
        raise ValueError('Profiles must use the same team and extension ID must equal host ID plus .filesystem')
    source = args.unsigned_app.resolve(strict=True)
    if not (source / 'Contents' / 'PlugIns' / 'XetFS.appex' / 'Contents' / 'MacOS' / 'XetFSExtension').is_file():
        raise ValueError('Expected an unsigned XetFS app bundle')
    destination = args.new_output_directory.resolve()
    if destination == source or source in destination.parents:
        raise ValueError('Output directory must be outside the source app')
    args.new_output_directory.mkdir()
    output = args.new_output_directory.resolve() / 'XetFS.app'
    shutil.copytree(source, output, symlinks=True)
    ext = output / 'Contents' / 'PlugIns' / 'XetFS.appex'
    for bundle, bundle_id, provisioning in ((output, host_id, args.host_profile), (ext, ext_id, args.extension_profile)):
        info_path = bundle / 'Contents' / 'Info.plist'
        with info_path.open('rb') as file:
            info = plistlib.load(file)
        info['CFBundleIdentifier'] = bundle_id
        with info_path.open('wb') as file:
            plistlib.dump(info, file)
        shutil.copyfile(provisioning, bundle / 'Contents' / 'embedded.provisionprofile')
    with tempfile.TemporaryDirectory(prefix='xetfs-sign-') as temporary:
        for name, bundle, info, entitlement_file in (
            ('extension', ext, extension, HERE / 'entitlements.plist'),
            ('host', output, host, HERE / 'host-entitlements.plist'),
        ):
            entitlement_path = Path(temporary) / (name + '.plist')
            with entitlement_path.open('wb') as file:
                plistlib.dump(signing_entitlements(entitlement_file, info), file)
            command('codesign', '--force', '--sign', identity, '--timestamp=none', '--entitlements', str(entitlement_path), str(bundle))
        command('codesign', '--verify', '--strict', '--deep', str(output))
    print(f'Signed app: {output}')
    print('Signature verified locally; activation and mounting still require system validation.')


if __name__ == '__main__':
    main()
