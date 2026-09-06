import { createPublicKey, verify } from 'node:crypto';
import { readFileSync } from 'node:fs';
const [keyText, signatureText, archive] = process.argv.slice(2);
const rawKey = Buffer.from(keyText ?? '', 'base64');
const signature = Buffer.from(signatureText ?? '', 'base64');
if (rawKey.length !== 32 || signature.length !== 64 || !archive) throw Error('Invalid Ed25519 signature inputs');
const publicKey = createPublicKey({ key: Buffer.concat([Buffer.from('302a300506032b6570032100', 'hex'), rawKey]), format: 'der', type: 'spki' });
if (!verify(null, readFileSync(archive), publicKey, signature)) throw Error('Native update signature does not match the trusted public key');
console.log('Native update signature verified');
