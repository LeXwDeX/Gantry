import { randomBytes } from "node:crypto";

const ALPHABET = "0123456789ABCDEFGHJKMNPQRSTVWXYZ";

export function newUlid(now = Date.now()) {
  let time = "";
  let value = now;
  for (let index = 0; index < 10; index += 1) {
    time = ALPHABET[value % 32] + time;
    value = Math.floor(value / 32);
  }
  let entropy = "";
  for (const byte of randomBytes(16)) {
    entropy += ALPHABET[byte % 32];
  }
  return time + entropy;
}

export function entityId(prefix, now) {
  return `${prefix}_${newUlid(now)}`;
}
