#!/usr/bin/env node
/**
 * Render an animated SVG in assets/ to a GIF via headless Chrome and ffmpeg.
 *
 * Prerequisites:
 *   - Google Chrome
 *   - ffmpeg
 *   - puppeteer-core (for example, `npm i --prefix /tmp puppeteer-core`)
 *
 *   DEMO_NAME=riff-workflow WIDTH=900 HEIGHT=520 DURATION_MS=18000 node assets/export-gif.mjs
 */
import { spawn } from "node:child_process";
import { readFile, mkdir, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const assetDir = dirname(fileURLToPath(import.meta.url));
const demoName = process.env.DEMO_NAME || "riff-demo";
const svgPath = join(assetDir, `${demoName}.svg`);
const outGif = join(assetDir, `${demoName}.gif`);
const chrome =
  process.env.CHROME_PATH ||
  "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome";
const moduleRoot = process.env.PUPPETEER_MODULE || "/tmp/node_modules/puppeteer-core";
const puppeteer = (
  await import(pathToFileURL(join(moduleRoot, "lib/esm/puppeteer/puppeteer-core.js")).href)
).default;

const DURATION_MS = Number(process.env.DURATION_MS || 16000);
const FPS = Number(process.env.FPS || 10);
const FRAME_COUNT = Math.round((DURATION_MS / 1000) * FPS);
const WIDTH = Number(process.env.WIDTH || 780);
const HEIGHT = Number(process.env.HEIGHT || 440);

const svg = await readFile(svgPath, "utf8");
const frameDir = join(tmpdir(), `${demoName}-frames-${process.pid}`);
await rm(frameDir, { recursive: true, force: true });
await mkdir(frameDir, { recursive: true });

const browser = await puppeteer.launch({
  executablePath: chrome,
  headless: true,
  args: [
    "--no-sandbox",
    "--disable-dev-shm-usage",
    `--window-size=${WIDTH},${HEIGHT}`,
    "--hide-scrollbars",
  ],
});

try {
  const page = await browser.newPage();
  await page.setViewport({ width: WIDTH, height: HEIGHT, deviceScaleFactor: 1 });
  await page.setContent(
    `<!doctype html>
<html>
<head>
<meta charset="utf-8" />
<style>
  html, body { margin: 0; width: ${WIDTH}px; height: ${HEIGHT}px; background: #03080b; overflow: hidden; }
  svg { display: block; }
</style>
</head>
<body>${svg}</body>
</html>`,
    { waitUntil: "load" },
  );
  await new Promise((resolve) => setTimeout(resolve, 250));
  await page.evaluate(() => {
    const animation = document.querySelector("svg");
    animation.pauseAnimations();
    animation.setCurrentTime(0);
  });
  console.log(`Capturing ${FRAME_COUNT} frames at ${FPS} fps...`);

  for (let index = 0; index < FRAME_COUNT; index += 1) {
    await page.evaluate((seconds) => {
      document.querySelector("svg").setCurrentTime(seconds);
    }, index / FPS);
    await page.evaluate(() => new Promise((resolve) => requestAnimationFrame(resolve)));
    await page.screenshot({
      path: join(frameDir, `frame-${String(index).padStart(4, "0")}.png`),
    });
    if (index % 20 === 0) console.log(`  frame ${index}/${FRAME_COUNT}`);
  }
} finally {
  await browser.close();
}

console.log("Encoding GIF...");
await new Promise((resolve, reject) => {
  const ffmpeg = spawn(
    "ffmpeg",
    [
      "-y",
      "-framerate",
      String(FPS),
      "-i",
      join(frameDir, "frame-%04d.png"),
      "-vf",
      `fps=${FPS},scale=${WIDTH}:-1:flags=lanczos,split[s0][s1];[s0]palettegen=max_colors=160:stats_mode=diff[p];[s1][p]paletteuse=dither=bayer:bayer_scale=3`,
      outGif,
    ],
    { stdio: "inherit" },
  );
  ffmpeg.on("exit", (code) =>
    code === 0 ? resolve() : reject(new Error(`ffmpeg exited ${code}`)),
  );
});

await rm(frameDir, { recursive: true, force: true });
console.log(`Wrote ${outGif}`);
