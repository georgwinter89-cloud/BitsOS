// mcp-keys.mjs (MC-8b): this host's side of the TLS pairing with BitsOS's MCP service, for mcp-call.mjs and
// pair.mjs. The key store is a private directory outside the repository (default ~/.config/bitsos-mcp/, 0700):
//   client.key   this host's P-256 key (PKCS#8 PEM, 0600), made once by pair.mjs
//   client.crt   its self-signed certificate (PEM): what the Mini-PC keeps in \BITSOS2\mcp\paired\
//   pins/<host>.der   the Mini-PC's certificate as the pairing received it; every TLS connection must show
//                exactly this one, else the tools refuse loudly and send nothing
// The certificate is built like bitsmcp's (user/services/bitsmcp/src/cert.rs): version 3, one common name,
// valid 2000-01-01 to 9999-12-31, ECDSA with SHA-256, no extensions; node:crypto signs, the DER is written here.
import { createHash, generateKeyPairSync, randomBytes, sign, X509Certificate } from 'node:crypto';
import { chmodSync, existsSync, mkdirSync, readFileSync, writeFileSync } from 'node:fs';
import { homedir, hostname } from 'node:os';
import { dirname, join, relative, resolve, sep } from 'node:path';
import { fileURLToPath } from 'node:url';

export const DEFAULT_STORE = join(homedir(), '.config', 'bitsos-mcp');

// MC-10: the store when none is named (--tls auto, pair.mjs without --store): BITSOS_MCP_STORE, else
// bitsos-mcp/ next to the token file (the client key lives beside the token, 0600, never in git), else
// ~/.config/bitsos-mcp/.
export function defaultStore(tokenFile) {
  if (process.env.BITSOS_MCP_STORE) return process.env.BITSOS_MCP_STORE;
  if (tokenFile) return join(dirname(resolve(tokenFile)), 'bitsos-mcp');
  return DEFAULT_STORE;
}

// --tls's value: a directory, or `auto` for defaultStore.
export const storeOf = (tls, tokenFile) => (tls === 'auto' || tls === true ? defaultStore(tokenFile) : tls);
const REPO = resolve(dirname(fileURLToPath(import.meta.url)), '..', '..');

// A problem with the store or a pin; kind 'pin' (mcp-call.mjs's McpError kinds) so callers stop and say so.
export class KeyError extends Error {
  constructor(message) { super(message); this.kind = 'pin'; }
}

const tlv = (tag, ...parts) => {
  const body = Buffer.concat(parts);
  let len = [body.length];
  if (body.length >= 0x80) {
    const b = [];
    for (let n = body.length; n; n = Math.floor(n / 256)) b.unshift(n & 0xff);
    len = [0x80 | b.length, ...b];
  }
  return Buffer.concat([Buffer.from([tag, ...len]), body]);
};

// The name the Mini-PC's dialog shows for this host (its certificate's common name, at most 40 characters).
export function clientName() {
  return `bitsos-desk ${hostname()}`.replace(/[^\x20-\x7e]/g, '?').slice(0, 40);
}

// A new P-256 key and its self-signed certificate -> { keyPem, der }
export function makeClient(cn = clientName()) {
  const { privateKey, publicKey } = generateKeyPairSync('ec', { namedCurve: 'prime256v1' });
  const alg = tlv(0x30, tlv(0x06, Buffer.from([0x2a, 0x86, 0x48, 0xce, 0x3d, 0x04, 0x03, 0x02])));
  const name = tlv(0x30, tlv(0x31, tlv(0x30, tlv(0x06, Buffer.from([0x55, 0x04, 0x03])), tlv(0x0c, Buffer.from(cn)))));
  const serial = randomBytes(16);
  serial[0] = (serial[0] & 0x7f) | 0x01;
  const validity = tlv(0x30, tlv(0x17, Buffer.from('000101000000Z')), tlv(0x18, Buffer.from('99991231235959Z')));
  const spki = publicKey.export({ type: 'spki', format: 'der' });
  const tbs = tlv(0x30, tlv(0xa0, tlv(0x02, Buffer.from([2]))), tlv(0x02, serial), alg, name, validity, name, spki);
  const der = tlv(0x30, tbs, alg, tlv(0x03, Buffer.from([0]), sign('sha256', tbs, privateKey)));
  return { keyPem: privateKey.export({ type: 'pkcs8', format: 'pem' }), der };
}

export const fingerprint = (der) => createHash('sha256').update(der).digest('hex');
const pem = (der) => `-----BEGIN CERTIFICATE-----\n${der.toString('base64').replace(/.{64}/g, '$&\n').replace(/\n?$/, '\n')}-----END CERTIFICATE-----\n`;

// The store's absolute path; never inside the repository (except a build directory `target`, for the tests)
export function storePath(dir = DEFAULT_STORE) {
  const abs = resolve(dir);
  const rel = relative(REPO, abs);
  if (!rel.startsWith('..') && !rel.split(sep).includes('target')) {
    throw new KeyError(`the key store ${abs} is inside the repository ${REPO}: keys never go there (default ${DEFAULT_STORE})`);
  }
  return abs;
}

// This host's key and certificate from the store; made when `make` and missing -> { key, cert, der, made }
export function clientKey(dir, { make = false } = {}) {
  const store = storePath(dir);
  const kp = join(store, 'client.key'), cp = join(store, 'client.crt');
  if (!existsSync(kp) || !existsSync(cp)) {
    if (!make) throw new KeyError(`no client key in ${store}: pair first (node host/mcp/pair.mjs <host[:port]> --store ${store})`);
    mkdirSync(store, { recursive: true, mode: 0o700 });
    chmodSync(store, 0o700);
    const { keyPem, der } = makeClient();
    writeFileSync(kp, keyPem, { mode: 0o600 });
    writeFileSync(cp, pem(der), { mode: 0o644 });
    return { key: keyPem, cert: pem(der), der, made: true };
  }
  const cert = readFileSync(cp, 'utf8');
  return { key: readFileSync(kp, 'utf8'), cert, der: new X509Certificate(cert).raw, made: false };
}

// pins/<host>.der: the host without its port (the Mini-PC's key is the same on 8765 and 8766)
export function pinFile(dir, host) {
  const name = host.replace(/:\d+$/, '').replace(/[^A-Za-z0-9.-]/g, '_');
  return join(storePath(dir), 'pins', `${name}.der`);
}

export function readPin(dir, host) {
  const f = pinFile(dir, host);
  if (!existsSync(f)) throw new KeyError(`no pinned key for ${host} in ${dirname(dirname(f))}: pair first (node host/mcp/pair.mjs ${host} --store ${dirname(dirname(f))})`);
  return { file: f, der: readFileSync(f) };
}

export function writePin(dir, host, der) {
  const f = pinFile(dir, host);
  mkdirSync(dirname(f), { recursive: true, mode: 0o700 });
  writeFileSync(f, der, { mode: 0o644 });
  return f;
}

// What connect() needs for --tls: the client key and the pin for host
export function tlsFor(dir, host) {
  const c = clientKey(dir);
  const pin = readPin(dir, host);
  return { key: c.key, cert: c.cert, pin: pin.der, pinFile: pin.file };
}
