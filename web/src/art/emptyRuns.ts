// The empty gallery's one moment, drawn by hand as Lottie data: a base line, three columns
// rising one after another, and the keystone settling on the arch they hold. The ink is
// the theme's, passed in, so the drawing matches either palette.

type RGB = [number, number, number];

function hex(color: string): RGB {
  const m = /^#?([0-9a-f]{6})/i.exec(color.trim());
  const n = m ? parseInt(m[1]!, 16) : 0xededf0;
  return [((n >> 16) & 255) / 255, ((n >> 8) & 255) / 255, (n & 255) / 255];
}

const ease = { i: { x: [0.22], y: [1] }, o: { x: [0.36], y: [0] } };
const tr = { ty: "tr", p: { a: 0, k: [0, 0] }, a: { a: 0, k: [0, 0] }, s: { a: 0, k: [100, 100] }, r: { a: 0, k: 0 }, o: { a: 0, k: 100 } };

function line(points: [number, number][], ink: RGB, width: number, draw: [number, number], erase: [number, number]) {
  return {
    ty: "gr",
    it: [
      { ty: "sh", ks: { a: 0, k: { i: points.map(() => [0, 0]), o: points.map(() => [0, 0]), v: points, c: false } } },
      { ty: "st", c: { a: 0, k: [...ink, 1] }, o: { a: 0, k: 100 }, w: { a: 0, k: width }, lc: 2, lj: 2 },
      {
        ty: "tm",
        s: { a: 1, k: [{ t: erase[0], s: [0], ...ease }, { t: erase[1], s: [100] }] },
        e: { a: 1, k: [{ t: draw[0], s: [0], ...ease }, { t: draw[1], s: [100] }] },
        o: { a: 0, k: 0 },
        m: 1,
      },
      tr,
    ],
  };
}

function arch(ink: RGB, draw: [number, number], erase: [number, number]) {
  // Two quarter curves from the outer columns up to the keystone.
  const left = { i: [[0, 0], [-14, 0]], o: [[0, -16], [0, 0]], v: [[46, 58], [72, 34]], c: false };
  const right = { i: [[0, 0], [14, 0]], o: [[0, -16], [0, 0]], v: [[114, 58], [88, 34]], c: false };
  return {
    ty: "gr",
    it: [
      { ty: "sh", ks: { a: 0, k: left } },
      { ty: "sh", ks: { a: 0, k: right } },
      { ty: "st", c: { a: 0, k: [...ink, 1] }, o: { a: 0, k: 60 }, w: { a: 0, k: 2.5 }, lc: 2, lj: 2 },
      {
        ty: "tm",
        s: { a: 1, k: [{ t: erase[0], s: [0], ...ease }, { t: erase[1], s: [100] }] },
        e: { a: 1, k: [{ t: draw[0], s: [0], ...ease }, { t: draw[1], s: [100] }] },
        o: { a: 0, k: 0 },
        m: 1,
      },
      tr,
    ],
  };
}

function keystone(ink: RGB) {
  return {
    ty: "gr",
    it: [
      { ty: "sh", ks: { a: 0, k: { i: [[0, 0], [0, 0], [0, 0], [0, 0]], o: [[0, 0], [0, 0], [0, 0], [0, 0]], v: [[72, 22], [88, 22], [84, 34], [76, 34]], c: true } } },
      { ty: "fl", c: { a: 0, k: [...ink, 1] }, o: { a: 0, k: 100 }, r: 1 },
      {
        ty: "tr",
        p: { a: 1, k: [{ t: 66, s: [0, -10], ...ease }, { t: 90, s: [0, 0] }] },
        a: { a: 0, k: [0, 0] },
        s: { a: 0, k: [100, 100] },
        r: { a: 0, k: 0 },
        o: { a: 1, k: [{ t: 66, s: [0], ...ease }, { t: 86, s: [100] }] },
      },
    ],
  };
}

export function emptyRunsLottie(inkColor: string): string {
  const ink = hex(inkColor);
  // It plays once and stays drawn, so nothing is erased within the clip.
  const erase: [number, number] = [1000, 1001];
  const shapes = [
    keystone(ink),
    arch(ink, [44, 78], erase),
    line([[46, 96], [46, 58]], ink, 3, [12, 42], erase),
    line([[80, 96], [80, 50]], ink, 3, [20, 50], erase),
    line([[114, 96], [114, 58]], ink, 3, [28, 58], erase),
    line([[30, 102], [130, 102]], ink, 2, [0, 24], erase),
  ];
  return JSON.stringify({
    v: "5.7.0",
    fr: 60,
    ip: 0,
    op: 110,
    w: 160,
    h: 120,
    nm: "empty runs",
    ddd: 0,
    assets: [],
    layers: [
      {
        ddd: 0,
        ind: 1,
        ty: 4,
        nm: "arch",
        sr: 1,
        ks: { o: { a: 0, k: 100 }, r: { a: 0, k: 0 }, p: { a: 0, k: [0, 0, 0] }, a: { a: 0, k: [0, 0, 0] }, s: { a: 0, k: [100, 100, 100] } },
        ao: 0,
        shapes,
        ip: 0,
        op: 110,
        st: 0,
        bm: 0,
      },
    ],
  });
}
