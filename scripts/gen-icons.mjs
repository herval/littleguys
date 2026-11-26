// Generate the desktop icons and Ghost Wave's menu-bar templates with pnpm icons.
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { Resvg } from "@resvg/resvg-js";
import tauri from "@tauri-apps/cli";
import { signatureTemplate, trayFrames, uiMascots } from "../assets/branding/ghost-wave-poses.mjs";

const root = path.join(import.meta.dirname, "..");
const brandDir = path.join(root, "assets", "branding");
const iconDir = path.join(root, "src-tauri", "icons");
const trayDir = path.join(root, "src-tauri", "assets", "tray");
const uiDir = path.join(root, "src", "ui");
fs.mkdirSync(iconDir, { recursive: true });
fs.mkdirSync(trayDir, { recursive: true });

function png(svg, width) {
  return new Resvg(svg, { fitTo: { mode: "width", value: width } }).render().asPng();
}

// Tauri emits the ICNS representations in hash-map order. Each chunk starts
// with its four-byte type and big-endian length, including its own header.
function orderedIcns(file) {
  const bytes = fs.readFileSync(file);
  const chunks = [];
  for (let offset = 8; offset < bytes.length;) {
    const length = bytes.readUInt32BE(offset + 4);
    if (length < 8 || offset + length > bytes.length) throw new Error("Invalid ICNS chunk");
    chunks.push(bytes.subarray(offset, offset + length));
    offset += length;
  }
  return Buffer.concat([bytes.subarray(0, 8), ...chunks.sort(Buffer.compare)]);
}

// The original artwork fills its square. Give the plate the same optical inset
// as the other icons in the Dock before Tauri builds the desktop containers.
const original = fs.readFileSync(path.join(brandDir, "ghost-wave.png")).toString("base64");
const dock = `<svg xmlns="http://www.w3.org/2000/svg" xmlns:xlink="http://www.w3.org/1999/xlink" width="1024" height="1024">
  <image x="96" y="96" width="832" height="832" xlink:href="data:image/png;base64,${original}"/>
</svg>`;
const temp = fs.mkdtempSync(path.join(os.tmpdir(), "littleguys-icons-"));
try {
  const master = path.join(temp, "app-icon.png");
  fs.writeFileSync(master, png(dock, 1024));
  await tauri.run(["icon", master, "--output", temp], "pnpm icons");
  fs.writeFileSync(path.join(iconDir, "icon.icns"), orderedIcns(path.join(temp, "icon.icns")));
  // Tauri also exports mobile and AppX assets. These are the desktop bundle inputs.
  for (const file of ["app-icon.png", "32x32.png", "128x128.png", "128x128@2x.png", "icon.png", "icon.ico"]) {
    fs.copyFileSync(path.join(temp, file), path.join(iconDir, file));
  }
} finally {
  fs.rmSync(temp, { recursive: true, force: true });
}

for (const [name, svg] of Object.entries(trayFrames)) {
  fs.writeFileSync(path.join(trayDir, `${name}.png`), png(svg, 44));
}

const posesDir = path.join(brandDir, "menu-bar");
fs.mkdirSync(posesDir, { recursive: true });
const keyPoses = {
  idle: "idle-0",
  working: "work-1",
  "new-reply": "news-0",
  "needs-you": "needs-1",
  offline: "off-1",
  connecting: "setup-1",
};
for (const [state, frame] of Object.entries(keyPoses)) {
  fs.writeFileSync(path.join(posesDir, `${state}.svg`), trayFrames[frame]);
}
fs.writeFileSync(path.join(brandDir, "ghost-wave-template.svg"), signatureTemplate);
fs.writeFileSync(path.join(brandDir, "ghost-wave-template.png"), png(signatureTemplate, 22));
fs.writeFileSync(path.join(brandDir, "ghost-wave-template@2x.png"), png(signatureTemplate, 44));
for (const [name, svg] of Object.entries(uiMascots)) {
  fs.writeFileSync(path.join(uiDir, name), svg);
}
console.log(`Wrote the desktop icons, menu-bar poses and ${Object.keys(trayFrames).length} tray frames.`);
