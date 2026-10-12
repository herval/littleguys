// A photo as a guy's picture: a small square image in a data: URL (see
// guyPicture.ts for where it's kept). The roster carries it to every window, so
// it stays small.

/** Big enough for the largest face (96pt) on a Retina screen, small enough to ride along in the roster. */
export const AVATAR_MAX_CHARS = 24_000;
const SIZES = [160, 128, 96];

/** Center-crops and shrinks an image file into a data URL that fits `AVATAR_MAX_CHARS`. */
export async function photoToAvatar(file: Blob): Promise<string> {
  if (!file.type.startsWith("image/")) throw new Error("That isn't an image.");
  const img = await load(file);
  const side = Math.min(img.naturalWidth, img.naturalHeight);
  if (!side) throw new Error("Couldn't read that image.");
  const square = { x: (img.naturalWidth - side) / 2, y: (img.naturalHeight - side) / 2, side };
  for (const size of SIZES) {
    const px = Math.min(size, side);
    const canvas = shrink(img, square, px);
    // WebKit can't encode WebP; PNG keeps transparency (and is tiny for drawings); JPEG fits photos.
    const webp = canvas.toDataURL("image/webp", 0.85);
    if (webp.startsWith("data:image/webp") && webp.length <= AVATAR_MAX_CHARS) return webp;
    const png = canvas.toDataURL("image/png");
    if (png.length <= AVATAR_MAX_CHARS) return png;
    const jpeg = onWhite(canvas).toDataURL("image/jpeg", 0.85);
    if (jpeg.length <= AVATAR_MAX_CHARS) return jpeg;
  }
  throw new Error("Couldn't make that image small enough. Try another one.");
}

async function load(file: Blob): Promise<HTMLImageElement> {
  const url = URL.createObjectURL(file);
  try {
    const img = new Image();
    img.src = url;
    await img.decode();
    return img;
  } catch {
    throw new Error("Couldn't read that image.");
  } finally {
    URL.revokeObjectURL(url);
  }
}

/** Halves the crop until it's close to `px`, then draws it at `px`: one big jump looks jagged. */
function shrink(img: HTMLImageElement, crop: { x: number; y: number; side: number }, px: number): HTMLCanvasElement {
  let source: CanvasImageSource = img;
  let { x, y, side } = crop;
  while (side / 2 >= px * 2) {
    const half = canvas(Math.round(side / 2));
    const ctx = half.getContext("2d")!;
    ctx.imageSmoothingQuality = "high";
    ctx.drawImage(source, x, y, side, side, 0, 0, half.width, half.width);
    source = half;
    x = y = 0;
    side = half.width;
  }
  const out = canvas(px);
  const ctx = out.getContext("2d")!;
  ctx.imageSmoothingQuality = "high";
  ctx.drawImage(source, x, y, side, side, 0, 0, px, px);
  return out;
}

/** JPEG has no transparency; without this, see-through pixels turn black. */
function onWhite(src: HTMLCanvasElement): HTMLCanvasElement {
  const out = canvas(src.width);
  const ctx = out.getContext("2d")!;
  ctx.fillStyle = "#fff";
  ctx.fillRect(0, 0, out.width, out.height);
  ctx.drawImage(src, 0, 0);
  return out;
}

function canvas(side: number): HTMLCanvasElement {
  const c = document.createElement("canvas");
  c.width = c.height = side;
  return c;
}
