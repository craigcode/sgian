#!/usr/bin/env python3
"""Validate the complete native desktop release before writing publishable files."""
import argparse
import base64
import hashlib
import json
import re
from pathlib import Path
import shutil
import subprocess
import tempfile
import xml.etree.ElementTree as ET

REPO = 'https://github.com/craigcode/sgian'
SPARKLE = '{http://www.andymatuschak.org/xml-namespaces/sparkle}'
ROOT = Path(__file__).resolve().parent.parent


def verify_mac(path, signature, key):
    subprocess.run(['node', str(ROOT / 'scripts/verify-native-signature.mjs'), key, signature, str(path)], check=True)


def verify_linux(path, signature, key):
    with tempfile.TemporaryDirectory() as folder:
        public = Path(folder) / 'public.key'
        signed = Path(folder) / 'signature'
        public.write_bytes(base64.b64decode(key, validate=True))
        signed.write_bytes(base64.b64decode(signature, validate=True))
        subprocess.run(['minisign', '-V', '-m', str(path), '-p', str(public), '-x', str(signed)], check=True)


def prepare(source, output, config, sparkle_key, mac_verifier=verify_mac, linux_verifier=verify_linux):
    source, output = Path(source), Path(output)
    version = config['version']
    if not re.fullmatch(r'(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)', version):
        raise ValueError('Native releases require a stable three-part version')
    if any(int(part) > 65535 for part in version.split('.')):
        raise ValueError('Version exceeds the Windows package version limit')
    if len(base64.b64decode(sparkle_key, validate=True)) != 32:
        raise ValueError('A trusted Sparkle public key is required')
    files = {}
    for path in source.rglob('*'):
        if path.is_symlink(): raise ValueError('Symlink in release inputs')
        if not path.is_file(): continue
        if path.name in files: raise ValueError(f'Duplicate asset: {path.name}')
        if path.name == 'STUB-BUILD': raise ValueError('Development update build cannot be released')
        files[path.name] = path
    def require(name):
        if name not in files: raise ValueError(f'Missing release asset: {name}')
        return files[name]
    def one(suffix):
        matches = [path for name, path in files.items() if name.endswith(suffix)]
        if len(matches) != 1: raise ValueError(f'Expected one {suffix} asset')
        return matches[0]
    def digest(path): return hashlib.sha256(path.read_bytes()).hexdigest()
    def metadata(name, asset):
        data = json.loads(require(name).read_text(encoding='utf-8-sig'))
        if data.get('version') != version or data.get('asset') != asset.name or data.get('sha256') != digest(asset):
            raise ValueError(f'Invalid package metadata: {name}')
        return data
    mac = require(f'Sgian_{version}_macos_universal.dmg')
    windows = require(f'Sgian_{version}_windows_x64.msix')
    mac_info = metadata('macos-package.json', mac)
    windows_info = metadata('windows-package.json', windows)
    if mac_info.get('public_key') != sparkle_key: raise ValueError('Embedded Sparkle key differs from trusted release key')
    publisher = windows_info.get('publisher', '')
    if not publisher.startswith('CN=') or 'Development' in publisher or windows_info.get('architecture') != 'x64':
        raise ValueError('Expected a production Windows publisher and x64 package')
    appcast = require('appcast.xml')
    items = ET.fromstring(appcast.read_bytes()).findall('./channel/item')
    if len(items) != 1: raise ValueError('Expected one appcast item')
    enclosure = items[0].find('enclosure')
    base_url = f'{REPO}/releases/download/v{version}/'
    if enclosure is None or enclosure.get('url') != base_url + mac.name or int(enclosure.get('length', '0')) != mac.stat().st_size:
        raise ValueError('Appcast does not describe the release archive')
    if items[0].findtext(SPARKLE + 'version') != version:
        raise ValueError('Appcast version differs from release version')
    signature = enclosure.get(SPARKLE + 'edSignature', '')
    mac_verifier(mac, signature, sparkle_key)
    linux = one('.AppImage')
    deb = one('.deb')
    for asset in (linux, deb):
        if not asset.name.startswith((f'Sgian_{version}_', f'sgian_{version}_')):
            raise ValueError('Linux package version differs from release version')
        if not any(arch in asset.name for arch in ('x86_64', 'amd64')):
            raise ValueError('Expected x86_64 Linux artifacts')
    linux_signature = require(linux.name + '.sig')
    linux_verifier(linux, linux_signature.read_text().strip(), config['plugins']['updater']['pubkey'])
    if output.exists() and any(output.iterdir()): raise ValueError('Output directory must be empty')
    output.mkdir(parents=True, exist_ok=True)
    for asset in (mac, windows, appcast, linux, deb, linux_signature): shutil.copy2(asset, output / asset.name)
    installer = ET.Element('AppInstaller', {'xmlns': 'http://schemas.microsoft.com/appx/appinstaller/2018',
        'Version': version + '.0', 'Uri': f'{REPO}/releases/latest/download/Sgian.appinstaller'})
    ET.SubElement(installer, 'MainPackage', {'Name': 'dev.sgian.windows', 'Publisher': publisher,
        'Version': version + '.0', 'ProcessorArchitecture': 'x64', 'Uri': base_url + windows.name})
    settings = ET.SubElement(installer, 'UpdateSettings')
    # Keep the descriptor compatible with Windows 10 1809. ShowPrompt and
    # UpdateBlocksActivation require 1903 and do not prompt for desktop apps:
    # https://learn.microsoft.com/uwp/schemas/appinstallerschema/element-s3-onlaunch
    ET.SubElement(settings, 'OnLaunch', {'HoursBetweenUpdateChecks': '4'})
    ET.SubElement(settings, 'AutomaticBackgroundTask')
    ET.ElementTree(installer).write(output / 'Sgian.appinstaller', encoding='utf-8', xml_declaration=True)
    feed = {'version': version, 'platforms': {'linux-x86_64': {'signature': linux_signature.read_text().strip(), 'url': base_url + linux.name}}}
    (output / 'latest.json').write_text(json.dumps(feed, indent=2) + '\n')
    (output / 'SHA256SUMS.txt').write_text(''.join(f'{digest(path)}  {path.name}\n' for path in sorted(output.iterdir()) if path.is_file()))
    return feed


if __name__ == '__main__':
    parser = argparse.ArgumentParser()
    parser.add_argument('input'); parser.add_argument('output'); parser.add_argument('--sparkle-key', required=True)
    args = parser.parse_args()
    prepare(args.input, args.output, json.loads((ROOT / 'src-tauri/tauri.conf.json').read_text()), args.sparkle_key)
    print('Verified native macOS, Windows and Linux release assets')
