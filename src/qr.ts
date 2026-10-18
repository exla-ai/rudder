// A small QR code encoder for the terminal: byte mode, error correction L or
// M, versions 1 to 10 (up to ~270 bytes), which covers the approve link that
// `rudder login` prints. No dependency: the algorithm follows ISO/IEC 18004
// the same way Project Nayuki's reference encoder does.

export type QrLevel = "L" | "M";

const LEVEL_FORMAT_BITS: Record<QrLevel, number> = { L: 1, M: 0 };
// Index = version. Error correction codewords per block, and number of blocks.
const ECC_PER_BLOCK: Record<QrLevel, number[]> = {
  L: [-1, 7, 10, 15, 20, 26, 18, 20, 24, 30, 18],
  M: [-1, 10, 16, 26, 18, 24, 16, 18, 22, 22, 26],
};
const ECC_BLOCKS: Record<QrLevel, number[]> = {
  L: [-1, 1, 1, 1, 1, 1, 2, 2, 2, 2, 4],
  M: [-1, 1, 1, 1, 2, 2, 4, 4, 4, 5, 5],
};
const MAX_VERSION = 10;

/** The module matrix (true = dark), without a quiet zone. */
export function encodeQr(text: string, level: QrLevel = "M"): boolean[][] {
  const data = Buffer.from(text, "utf8");
  let version = 1;
  for (; version <= MAX_VERSION; version += 1) {
    // Mode (4) + count (8 or 16 bits) + payload.
    const countBits = version <= 9 ? 8 : 16;
    if (4 + countBits + data.length * 8 <= dataCodewords(version, level) * 8) {
      break;
    }
  }
  if (version > MAX_VERSION) {
    throw new Error("text is too long for a terminal QR code");
  }
  const capacityBits = dataCodewords(version, level) * 8;
  const bits: number[] = [];
  const push = (value: number, length: number) => {
    for (let i = length - 1; i >= 0; i -= 1) {
      bits.push((value >>> i) & 1);
    }
  };
  push(0b0100, 4);
  push(data.length, version <= 9 ? 8 : 16);
  for (const byte of data) {
    push(byte, 8);
  }
  push(0, Math.min(4, capacityBits - bits.length));
  push(0, (8 - (bits.length % 8)) % 8);
  for (let pad = 0xec; bits.length < capacityBits; pad ^= 0xec ^ 0x11) {
    push(pad, 8);
  }
  const codewords: number[] = [];
  for (let i = 0; i < bits.length; i += 8) {
    let byte = 0;
    for (let j = 0; j < 8; j += 1) {
      byte = (byte << 1) | bits[i + j];
    }
    codewords.push(byte);
  }

  const qr = new Matrix(version);
  qr.drawFunctionPatterns();
  qr.drawCodewords(interleaveWithEcc(codewords, version, level));
  let best = 0;
  let bestPenalty = Infinity;
  for (let mask = 0; mask < 8; mask += 1) {
    qr.applyMask(mask);
    qr.drawFormatBits(level, mask);
    const penalty = qr.penalty();
    if (penalty < bestPenalty) {
      best = mask;
      bestPenalty = penalty;
    }
    qr.applyMask(mask); // XOR undoes it
  }
  qr.applyMask(best);
  qr.drawFormatBits(level, best);
  return qr.modules;
}

/**
 * The code drawn with half blocks, two module rows per line, black on white
 * whatever the terminal's own colors are, with a quiet zone around it.
 */
export function renderQrForTerminal(text: string, options: { level?: QrLevel; quiet?: number; indent?: string } = {}): string {
  const modules = encodeQr(text, options.level ?? "M");
  const quiet = options.quiet ?? 2;
  const size = modules.length + quiet * 2;
  const dark = (x: number, y: number) => {
    const mx = x - quiet;
    const my = y - quiet;
    return my >= 0 && my < modules.length && mx >= 0 && mx < modules.length && modules[my][mx];
  };
  const lines: string[] = [];
  for (let y = 0; y < size; y += 2) {
    let line = "";
    for (let x = 0; x < size; x += 1) {
      const top = dark(x, y);
      const bottom = y + 1 < size && dark(x, y + 1);
      line += top && bottom ? "█" : top ? "▀" : bottom ? "▄" : " ";
    }
    lines.push(`${options.indent ?? ""}\x1b[30;47m${line}\x1b[0m`);
  }
  return lines.join("\n");
}

function rawDataModules(version: number): number {
  let result = (16 * version + 128) * version + 64;
  if (version >= 2) {
    const align = Math.floor(version / 7) + 2;
    result -= (25 * align - 10) * align - 55;
    if (version >= 7) {
      result -= 36;
    }
  }
  return result;
}

function dataCodewords(version: number, level: QrLevel): number {
  return Math.floor(rawDataModules(version) / 8) - ECC_PER_BLOCK[level][version] * ECC_BLOCKS[level][version];
}

function interleaveWithEcc(data: number[], version: number, level: QrLevel): number[] {
  const blockCount = ECC_BLOCKS[level][version];
  const eccLength = ECC_PER_BLOCK[level][version];
  const rawCodewords = Math.floor(rawDataModules(version) / 8);
  const shortBlocks = blockCount - (rawCodewords % blockCount);
  const shortLength = Math.floor(rawCodewords / blockCount);
  const divisor = reedSolomonDivisor(eccLength);
  const blocks: number[][] = [];
  for (let i = 0, k = 0; i < blockCount; i += 1) {
    const length = shortLength - eccLength + (i < shortBlocks ? 0 : 1);
    const block = data.slice(k, k + length);
    k += length;
    const ecc = reedSolomonRemainder(block, divisor);
    if (i < shortBlocks) {
      block.push(0); // placeholder so every block has the same length
    }
    blocks.push(block.concat(ecc));
  }
  const result: number[] = [];
  for (let i = 0; i < blocks[0].length; i += 1) {
    blocks.forEach((block, j) => {
      if (i !== shortLength - eccLength || j >= shortBlocks) {
        result.push(block[i]);
      }
    });
  }
  return result;
}

function gfMultiply(x: number, y: number): number {
  let z = 0;
  for (let i = 7; i >= 0; i -= 1) {
    z = (z << 1) ^ ((z >>> 7) * 0x11d);
    z ^= ((y >>> i) & 1) * x;
  }
  return z & 0xff;
}

function reedSolomonDivisor(degree: number): number[] {
  const result = new Array<number>(degree).fill(0);
  result[degree - 1] = 1;
  let root = 1;
  for (let i = 0; i < degree; i += 1) {
    for (let j = 0; j < result.length; j += 1) {
      result[j] = gfMultiply(result[j], root);
      if (j + 1 < result.length) {
        result[j] ^= result[j + 1];
      }
    }
    root = gfMultiply(root, 0x02);
  }
  return result;
}

function reedSolomonRemainder(data: number[], divisor: number[]): number[] {
  const result = divisor.map(() => 0);
  for (const byte of data) {
    const factor = byte ^ (result.shift() as number);
    result.push(0);
    divisor.forEach((coef, i) => {
      result[i] ^= gfMultiply(coef, factor);
    });
  }
  return result;
}

class Matrix {
  readonly size: number;
  readonly modules: boolean[][];
  private readonly isFunction: boolean[][];

  constructor(readonly version: number) {
    this.size = version * 4 + 17;
    this.modules = Array.from({ length: this.size }, () => new Array<boolean>(this.size).fill(false));
    this.isFunction = Array.from({ length: this.size }, () => new Array<boolean>(this.size).fill(false));
  }

  private set(x: number, y: number, dark: boolean): void {
    this.modules[y][x] = dark;
    this.isFunction[y][x] = true;
  }

  drawFunctionPatterns(): void {
    for (let i = 0; i < this.size; i += 1) {
      this.set(6, i, i % 2 === 0);
      this.set(i, 6, i % 2 === 0);
    }
    this.drawFinder(3, 3);
    this.drawFinder(this.size - 4, 3);
    this.drawFinder(3, this.size - 4);
    const positions = this.alignmentPositions();
    const last = positions.length - 1;
    positions.forEach((x, i) => {
      positions.forEach((y, j) => {
        if (!((i === 0 && j === 0) || (i === 0 && j === last) || (i === last && j === 0))) {
          this.drawAlignment(x, y);
        }
      });
    });
    // Reserve the format areas (real bits are drawn per mask).
    this.drawFormatBits("M", 0);
    this.drawVersion();
  }

  private alignmentPositions(): number[] {
    if (this.version === 1) {
      return [];
    }
    const count = Math.floor(this.version / 7) + 2;
    const step = Math.ceil((this.version * 4 + 4) / (count * 2 - 2)) * 2;
    const result = [6];
    for (let pos = this.size - 7; result.length < count; pos -= step) {
      result.splice(1, 0, pos);
    }
    return result;
  }

  private drawFinder(cx: number, cy: number): void {
    for (let dy = -4; dy <= 4; dy += 1) {
      for (let dx = -4; dx <= 4; dx += 1) {
        const x = cx + dx;
        const y = cy + dy;
        if (x >= 0 && x < this.size && y >= 0 && y < this.size) {
          const dist = Math.max(Math.abs(dx), Math.abs(dy));
          this.set(x, y, dist !== 2 && dist !== 4);
        }
      }
    }
  }

  private drawAlignment(cx: number, cy: number): void {
    for (let dy = -2; dy <= 2; dy += 1) {
      for (let dx = -2; dx <= 2; dx += 1) {
        this.set(cx + dx, cy + dy, Math.max(Math.abs(dx), Math.abs(dy)) !== 1);
      }
    }
  }

  drawFormatBits(level: QrLevel, mask: number): void {
    const data = (LEVEL_FORMAT_BITS[level] << 3) | mask;
    let rem = data;
    for (let i = 0; i < 10; i += 1) {
      rem = (rem << 1) ^ ((rem >>> 9) * 0x537);
    }
    const bits = ((data << 10) | rem) ^ 0x5412;
    const bit = (i: number) => ((bits >>> i) & 1) !== 0;
    for (let i = 0; i <= 5; i += 1) {
      this.set(8, i, bit(i));
    }
    this.set(8, 7, bit(6));
    this.set(8, 8, bit(7));
    this.set(7, 8, bit(8));
    for (let i = 9; i < 15; i += 1) {
      this.set(14 - i, 8, bit(i));
    }
    for (let i = 0; i < 8; i += 1) {
      this.set(this.size - 1 - i, 8, bit(i));
    }
    for (let i = 8; i < 15; i += 1) {
      this.set(8, this.size - 15 + i, bit(i));
    }
    this.set(8, this.size - 8, true); // the dark module
  }

  private drawVersion(): void {
    if (this.version < 7) {
      return;
    }
    let rem = this.version;
    for (let i = 0; i < 12; i += 1) {
      rem = (rem << 1) ^ ((rem >>> 11) * 0x1f25);
    }
    const bits = (this.version << 12) | rem;
    for (let i = 0; i < 18; i += 1) {
      const dark = ((bits >>> i) & 1) !== 0;
      const a = this.size - 11 + (i % 3);
      const b = Math.floor(i / 3);
      this.set(a, b, dark);
      this.set(b, a, dark);
    }
  }

  drawCodewords(codewords: number[]): void {
    let i = 0;
    for (let right = this.size - 1; right >= 1; right -= 2) {
      if (right === 6) {
        right = 5;
      }
      for (let vert = 0; vert < this.size; vert += 1) {
        for (let j = 0; j < 2; j += 1) {
          const x = right - j;
          const upward = ((right + 1) & 2) === 0;
          const y = upward ? this.size - 1 - vert : vert;
          if (!this.isFunction[y][x] && i < codewords.length * 8) {
            this.modules[y][x] = ((codewords[i >>> 3] >>> (7 - (i & 7))) & 1) !== 0;
            i += 1;
          }
        }
      }
    }
  }

  applyMask(mask: number): void {
    for (let y = 0; y < this.size; y += 1) {
      for (let x = 0; x < this.size; x += 1) {
        if (!this.isFunction[y][x] && maskBit(mask, x, y)) {
          this.modules[y][x] = !this.modules[y][x];
        }
      }
    }
  }

  penalty(): number {
    const size = this.size;
    const m = this.modules;
    let result = 0;
    // Runs of five or more, and finder-like 1:1:3:1:1 patterns, in rows and columns.
    for (const vertical of [false, true]) {
      for (let a = 0; a < size; a += 1) {
        const line: boolean[] = [];
        for (let b = 0; b < size; b += 1) {
          line.push(vertical ? m[b][a] : m[a][b]);
        }
        let run = 1;
        for (let b = 1; b <= size; b += 1) {
          if (b < size && line[b] === line[b - 1]) {
            run += 1;
          } else {
            if (run >= 5) {
              result += run - 2;
            }
            run = 1;
          }
        }
        const padded = [...new Array<boolean>(4).fill(false), ...line, ...new Array<boolean>(4).fill(false)];
        for (let b = 0; b + 11 <= padded.length; b += 1) {
          const w = padded.slice(b, b + 11).map((dark) => (dark ? 1 : 0)).join("");
          if (w === "10111010000" || w === "00001011101") {
            result += 40;
          }
        }
      }
    }
    // 2x2 blocks of one color.
    for (let y = 0; y < size - 1; y += 1) {
      for (let x = 0; x < size - 1; x += 1) {
        const c = m[y][x];
        if (c === m[y][x + 1] && c === m[y + 1][x] && c === m[y + 1][x + 1]) {
          result += 3;
        }
      }
    }
    // Balance of dark and light.
    let dark = 0;
    for (const row of m) {
      for (const cell of row) {
        dark += cell ? 1 : 0;
      }
    }
    const total = size * size;
    result += (Math.ceil(Math.abs(dark * 20 - total * 10) / total) - 1) * 10;
    return result;
  }
}

export function maskBit(mask: number, x: number, y: number): boolean {
  switch (mask) {
    case 0: return (x + y) % 2 === 0;
    case 1: return y % 2 === 0;
    case 2: return x % 3 === 0;
    case 3: return (x + y) % 3 === 0;
    case 4: return (Math.floor(x / 3) + Math.floor(y / 2)) % 2 === 0;
    case 5: return ((x * y) % 2) + ((x * y) % 3) === 0;
    case 6: return (((x * y) % 2) + ((x * y) % 3)) % 2 === 0;
    default: return (((x + y) % 2) + ((x * y) % 3)) % 2 === 0;
  }
}
