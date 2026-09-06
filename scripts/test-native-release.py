#!/usr/bin/env python3
import base64
import hashlib
import importlib.util
import json
from pathlib import Path
import subprocess
import tempfile
import unittest
import xml.etree.ElementTree as ET

spec = importlib.util.spec_from_file_location('native_release', Path(__file__).with_name('prepare-native-release.py'))
release = importlib.util.module_from_spec(spec)
spec.loader.exec_module(release)


class NativeReleaseTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.source = self.root / 'input'; self.source.mkdir()
        self.output = self.root / 'output'
        self.version = '0.1.0'
        self.mac = self.source / 'Sgian_0.1.0_macos_universal.dmg'; self.mac.write_bytes(b'native mac package')
        self.windows = self.source / 'Sgian_0.1.0_windows_x64.msix'; self.windows.write_bytes(b'native windows package')
        self.linux = self.source / 'Sgian_0.1.0_amd64.AppImage'; self.linux.write_bytes(b'linux package')
        (self.source / 'sgian_0.1.0_amd64.deb').write_bytes(b'deb')
        (self.source / (self.linux.name + '.sig')).write_text('linux-signature')
        signed = json.loads(subprocess.check_output(['node', '--input-type=module', '-e', '''
import {generateKeyPairSync, sign} from 'node:crypto';
import {readFileSync} from 'node:fs';
const {publicKey, privateKey} = generateKeyPairSync('ed25519');
console.log(JSON.stringify({key: publicKey.export({format:'der',type:'spki'}).subarray(-32).toString('base64'),
signature: sign(null,readFileSync(process.argv[1]),privateKey).toString('base64')}));
''', str(self.mac)], text=True))
        self.key = signed['key']
        self.config = {'version': self.version, 'plugins': {'updater': {'pubkey': 'linux-key'}}}
        def metadata(path, **extra):
            return dict(version=self.version, asset=path.name, sha256=hashlib.sha256(path.read_bytes()).hexdigest(), **extra)
        (self.source / 'macos-package.json').write_text(json.dumps(metadata(self.mac, public_key=self.key)))
        (self.source / 'windows-package.json').write_text(json.dumps(metadata(self.windows, publisher='CN=Sgian Publisher', architecture='x64')))
        rss = ET.Element('rss'); channel=ET.SubElement(rss,'channel'); item=ET.SubElement(channel,'item')
        ET.SubElement(item, release.SPARKLE+'version').text=self.version
        ET.SubElement(item,'enclosure',{'url':f'{release.REPO}/releases/download/v{self.version}/{self.mac.name}',
            'length':str(self.mac.stat().st_size),release.SPARKLE+'edSignature':signed['signature']})
        ET.ElementTree(rss).write(self.source/'appcast.xml')
        self.linux_calls = []

    def prepare(self):
        return release.prepare(self.source, self.output, self.config, self.key,
            linux_verifier=lambda *args: self.linux_calls.append(args))

    def test_complete_native_release_and_update_descriptors(self):
        feed=self.prepare()
        self.assertEqual(list(feed['platforms']), ['linux-x86_64'])
        self.assertEqual(len(self.linux_calls),1)
        installer=ET.parse(self.output/'Sgian.appinstaller').getroot()
        package=installer.find('{http://schemas.microsoft.com/appx/appinstaller/2018}MainPackage')
        self.assertEqual(package.get('Publisher'),'CN=Sgian Publisher')
        self.assertEqual(package.get('Version'),'0.1.0.0')
        self.assertIn('Sgian.appinstaller',(self.output/'SHA256SUMS.txt').read_text())

    def test_missing_windows_package_prevents_any_output(self):
        self.windows.unlink()
        with self.assertRaises(ValueError): self.prepare()
        self.assertFalse(self.output.exists())

    def test_tampered_mac_is_rejected_even_with_updated_checksum(self):
        self.mac.write_bytes(b'malicious mac file!')
        path=self.source/'macos-package.json'; metadata=json.loads(path.read_text())
        metadata['sha256']=hashlib.sha256(self.mac.read_bytes()).hexdigest(); path.write_text(json.dumps(metadata))
        tree=ET.parse(self.source/'appcast.xml'); tree.find('./channel/item/enclosure').set('length',str(self.mac.stat().st_size)); tree.write(self.source/'appcast.xml')
        with self.assertRaises(subprocess.CalledProcessError): self.prepare()
        self.assertFalse(self.output.exists())

    def test_wrong_trusted_key_is_rejected(self):
        self.key=base64.b64encode(bytes(32)).decode()
        with self.assertRaises(ValueError): self.prepare()
        self.assertFalse(self.output.exists())

    def test_duplicate_and_missing_signature_are_rejected(self):
        duplicate=self.source/'nested'; duplicate.mkdir(); (duplicate/self.mac.name).write_bytes(self.mac.read_bytes())
        with self.assertRaises(ValueError): self.prepare()
        (duplicate/self.mac.name).unlink()
        (self.source/(self.linux.name+'.sig')).unlink()
        with self.assertRaises(ValueError): self.prepare()
        self.assertFalse(self.output.exists())

    def test_stale_version_and_foreign_download_are_rejected(self):
        self.config['version']='0.2.0'
        with self.assertRaises(ValueError): self.prepare()
        self.config['version']='0.1.0'
        tree=ET.parse(self.source/'appcast.xml'); tree.find('./channel/item/enclosure').set('url','https://example.com/untrusted.dmg'); tree.write(self.source/'appcast.xml')
        with self.assertRaises(ValueError): self.prepare()
        self.assertFalse(self.output.exists())


if __name__ == '__main__': unittest.main()
