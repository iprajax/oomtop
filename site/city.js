/* oomtop site: the hero "memory city", built from data/snapshot.json (a recorded M5 Air).
 *
 * Each step of the spiral is one group (or the long tail of a kind, merged): its solid block is as tall as the
 * group's footprint, and it sits on top of everything before it, so the summit is the sum of all group footprints.
 * The translucent plane is the RAM ceiling (total memory, same scale). The well in the middle is swap: its
 * height is swap total, its water level is swap used. Amber blocks are reclaim candidates: click one and it sinks,
 * everything after it drops, and the headroom counter rises by oomtop's reclaim estimate.
 */
import * as THREE from "three";

const GiB = 1073741824;
const fmt = (b) => (window.oomtopFmt ? window.oomtopFmt(b) : (b / GiB).toFixed(1) + " GiB");
const KIND = {
  agent_session: "agent session", app: "app", build_daemon: "build daemon", model_server: "model server",
  sandbox: "sandbox", system: "system", other: "other"
};
const TAIL_BELOW = 100 * 1048576; // groups smaller than this are merged into one step per kind

const sceneEl = document.getElementById("scene");
const canvas = document.getElementById("city");
const tip = document.getElementById("tip");
const hudHeadroom = document.getElementById("hud-headroom");
const hudSum = document.getElementById("hud-sum");
const hudActions = document.getElementById("hud-actions");
const labelCeil = document.getElementById("label-ceiling");
const labelSwap = document.getElementById("label-swap");
const labelPeak = document.getElementById("label-peak");
const reduceMotion = window.matchMedia("(prefers-reduced-motion: reduce)");

const live = document.createElement("p");
live.className = "sr"; live.setAttribute("aria-live", "polite");
document.querySelector(".hud")?.appendChild(live);

function cssVar(name) {
  return getComputedStyle(document.documentElement).getPropertyValue(name).trim() || "#888";
}

function buildSteps(d) {
  const steps = [];
  const tail = new Map();
  for (const g of d.groups) {
    if (g.footprint >= TAIL_BELOW || g.reclaimable) steps.push({ ...g, count: 1 });
    else {
      const t = tail.get(g.kind) || { label: "", kind: g.kind, footprint: 0, count: 0, reclaimable: false, tail: true };
      t.footprint += g.footprint; t.count += 1; tail.set(g.kind, t);
    }
  }
  for (const t of [...tail.values()].filter((t) => t.footprint >= 8 * 1048576).sort((a, b) => b.footprint - a.footprint)) {
    t.label = `${t.count} smaller ${KIND[t.kind] || t.kind} group${t.count === 1 ? "" : "s"}`;
    steps.push(t);
  }
  return steps;
}

/* Outline edges of a step, top and bottom only: vertical corner lines sit on faces seen edge-on and shimmer. */
function horizontalEdges(geo) {
  const p = new THREE.EdgesGeometry(geo, 25).attributes.position.array, keep = [];
  for (let i = 0; i < p.length; i += 6) if (Math.abs(p[i + 1] - p[i + 4]) < 1e-4) keep.push(...p.slice(i, i + 6));
  const g = new THREE.BufferGeometry();
  g.setAttribute("position", new THREE.Float32BufferAttribute(keep, 3));
  return g;
}

/* Static fallback: the same stack as a 2D waterfall, drawn as SVG. */
function fallback(d, steps) {
  const fb = document.getElementById("scene-fallback");
  canvas.hidden = true; fb.hidden = false;
  document.querySelector(".hud-swap")?.remove(); // the 2D chart has no swap cylinder to explain
  const W = 720, H = 420, pad = 36, total = d.memory.total;
  const bw = (W - pad * 2) / steps.length;
  const y = (v) => H - pad - (v / total) * (H - pad * 2);
  let acc = 0, bars = "";
  steps.forEach((s, i) => {
    if (s.reclaimed) { // freed: a dashed outline where the bar stood
      const g0 = y(acc), g1 = y(acc + s.footprint);
      bars += `<rect x="${(pad + i * bw + 2).toFixed(1)}" y="${g1.toFixed(1)}" width="${(bw - 4).toFixed(1)}" height="${(g0 - g1).toFixed(1)}" rx="2" fill="none" stroke="var(--accent)" stroke-dasharray="3 3"><title>${s.label} · freed ≈${fmt(s.gain)}</title></rect>`;
      return;
    }
    const y0 = y(acc), y1 = y(acc + s.footprint); acc += s.footprint;
    const fill = s.reclaimable ? "var(--accent)" : "var(--kind-app)";
    bars += `<rect x="${(pad + i * bw + 2).toFixed(1)}" y="${y1.toFixed(1)}" width="${(bw - 4).toFixed(1)}" height="${Math.max(1, y0 - y1).toFixed(1)}" rx="2" fill="${fill}"><title>${s.label} · ${KIND[s.kind] || s.kind} · ${fmt(s.footprint)}</title></rect>`;
  });
  fb.innerHTML = `<svg viewBox="0 0 ${W} ${H}" role="img" aria-label="Memory stack of the recorded snapshot: group footprints stacked under the ${Math.round(total / GiB)} GiB RAM ceiling">
    <line x1="${pad}" x2="${W - pad}" y1="${y(total)}" y2="${y(total)}" stroke="var(--muted)" stroke-dasharray="4 4"/>
    <text x="${W - pad}" y="${y(total) - 8}" text-anchor="end" fill="var(--muted)" font-family="JetBrains Mono, monospace" font-size="15">RAM ceiling · ${Math.round(total / GiB)} GiB</text>
    <line x1="${pad}" x2="${W - pad}" y1="${H - pad}" y2="${H - pad}" stroke="var(--line-2)"/>
    ${bars}</svg>`;
}

async function main() {
  const d = await (window.oomtopSnapshot || fetch("data/snapshot.json").then((r) => r.json()));
  const steps = buildSteps(d);
  const total = d.memory.total;
  let reclaimedGain = 0, flat = false; // flat: no WebGL, the 2D fallback chart is shown instead

  const sumAll = () => steps.reduce((s, x) => s + (x.reclaimed ? 0 : x.footprint), 0);
  const counter = { headroom: d.headroom.bytes, sum: sumAll() };
  const shown = { headroom: counter.headroom, sum: counter.sum };
  function paintHud() {
    hudHeadroom.textContent = fmt(shown.headroom);
    hudSum.textContent = fmt(shown.sum);
    hudHeadroom.classList.toggle("up", reclaimedGain > 0);
  }
  paintHud();

  // HUD buttons: the keyboard / screen-reader path to the same reclaim story
  const buttons = new Map();
  for (const s of steps.filter((x) => x.reclaimable)) {
    const b = document.createElement("button");
    b.type = "button"; b.setAttribute("aria-pressed", "false");
    b.textContent = `Reclaim ${s.label} ≈${fmt(s.gain)}`;
    b.addEventListener("click", () => (s.reclaimed ? restore(s) : reclaim(s)));
    b.addEventListener("mouseenter", () => highlight(s));
    b.addEventListener("mouseleave", () => highlight(null));
    hudActions.appendChild(b); buttons.set(s, b);
  }
  const reset = document.createElement("button");
  reset.type = "button"; reset.className = "reset"; reset.textContent = "Reset"; reset.hidden = true;
  reset.addEventListener("click", () => steps.filter((s) => s.reclaimed).forEach(restore));
  hudActions.appendChild(reset);

  // start the GPU work after first paint and the load event, so the copy and CTA are interactive first
  await new Promise((r) => (document.readyState === "complete" ? r() : window.addEventListener("load", r, { once: true })));
  await new Promise((r) => (window.requestIdleCallback ? requestIdleCallback(r, { timeout: 1200 }) : setTimeout(r, 200)));

  /* ── renderer / scene (one context; no separate WebGL probe) ── */
  let renderer;
  try {
    if (!window.WebGLRenderingContext) throw new Error("no WebGL");
    // probe on a throwaway canvas (released at once), so a missing WebGL means a quiet fallback, not console errors
    const probe = document.createElement("canvas").getContext("webgl2") || document.createElement("canvas").getContext("webgl");
    if (!probe) throw new Error("no WebGL");
    probe.getExtension("WEBGL_lose_context")?.loseContext();
    renderer = new THREE.WebGLRenderer({ canvas, antialias: true, alpha: true, powerPreference: "high-performance" });
  } catch (e) { flat = true; fallback(d, steps); return; }
  // software rasterizers (no GPU): keep it light
  let software = false;
  try {
    const gl = renderer.getContext();
    const ext = gl.getExtension("WEBGL_debug_renderer_info");
    software = /swiftshader|llvmpipe|software|basic render/i.test(ext ? gl.getParameter(ext.UNMASKED_RENDERER_WEBGL) : "");
  } catch {}
  renderer.setPixelRatio(software ? 1 : Math.min(window.devicePixelRatio || 1, 2));
  renderer.outputColorSpace = THREE.SRGBColorSpace;
  renderer.toneMapping = THREE.ACESFilmicToneMapping;
  renderer.toneMappingExposure = 1.05;
  renderer.shadowMap.enabled = !software;
  renderer.shadowMap.type = THREE.PCFShadowMap; // PCFSoft dithers its penumbra, which shows as noise in the gaps between steps

  const scene = new THREE.Scene();
  scene.fog = new THREE.Fog(0x0d0c0b, 34, 78);
  const camera = new THREE.PerspectiveCamera(34, 1, 0.1, 200);

  const S = 0.55; // scene units per GiB
  const u = (bytes) => (bytes / GiB) * S;
  const ceilY = u(total);
  const lookY = ceilY * 0.36;

  const hemi = new THREE.HemisphereLight(0xfff2e0, 0x1a1612, 1.15);
  scene.add(hemi);
  const key = new THREE.DirectionalLight(0xfff0dc, 2.2);
  key.position.set(-14, 26, 12);
  key.castShadow = true;
  key.shadow.mapSize.set(2048, 2048);
  Object.assign(key.shadow.camera, { left: -14, right: 14, top: 14, bottom: -14, near: 1, far: 70 });
  key.shadow.bias = -0.0004;
  key.shadow.normalBias = 0.045; // no acne stripes on faces at grazing light angles
  key.shadow.radius = 3;
  scene.add(key);
  const fill = new THREE.DirectionalLight(0xfff4e6, 0.5); // follows the camera so faces toward the viewer never go black
  scene.add(fill);
  const rim = new THREE.DirectionalLight(0xffc477, 0.6);
  rim.position.set(16, 8, -14);
  scene.add(rim);

  // ground: a soft disc that fades into the background, with faint rings
  const groundTex = (() => {
    const c = document.createElement("canvas"); c.width = c.height = 512;
    const g = c.getContext("2d");
    const grd = g.createRadialGradient(256, 256, 0, 256, 256, 256);
    grd.addColorStop(0, "rgba(255,255,255,1)"); grd.addColorStop(0.55, "rgba(255,255,255,0.75)"); grd.addColorStop(1, "rgba(255,255,255,0)");
    g.fillStyle = grd; g.fillRect(0, 0, 512, 512);
    const t = new THREE.CanvasTexture(c); t.colorSpace = THREE.SRGBColorSpace; return t;
  })();
  const groundMat = new THREE.MeshStandardMaterial({ color: 0x15130f, roughness: 0.95, metalness: 0, alphaMap: groundTex, transparent: true });
  const ground = new THREE.Mesh(new THREE.CircleGeometry(30, 96), groundMat);
  ground.rotation.x = -Math.PI / 2; ground.receiveShadow = true;
  scene.add(ground);
  const ringMat = new THREE.LineBasicMaterial({ color: 0x3a3631, transparent: true, opacity: 0.5 });
  for (const r of [4, 8, 12, 16]) {
    const pts = []; for (let i = 0; i <= 128; i++) { const a = (i / 128) * Math.PI * 2; pts.push(new THREE.Vector3(Math.cos(a) * r, 0.01, Math.sin(a) * r)); }
    const ring = new THREE.Line(new THREE.BufferGeometry().setFromPoints(pts), ringMat);
    ring.material = ringMat.clone(); ring.material.opacity = 0.45 * (1 - r / 20);
    ring.userData.ring = true;
    scene.add(ring);
  }

  // RAM ceiling: a translucent glass lid over the stack (same scale), with a polar grid and a crisp rim
  const ceilR = 11;
  const ceilMat = new THREE.MeshBasicMaterial({ color: 0xede9e3, transparent: true, opacity: 0.035, side: THREE.DoubleSide, depthWrite: false });
  const ceiling = new THREE.Mesh(new THREE.CircleGeometry(ceilR, 128), ceilMat);
  ceiling.rotation.x = -Math.PI / 2; ceiling.position.y = ceilY;
  scene.add(ceiling);
  const grid = new THREE.PolarGridHelper(ceilR, 16, 5, 128, 0xffffff, 0xffffff);
  grid.position.y = ceilY; grid.material.transparent = true; grid.material.opacity = 0.07; grid.material.depthWrite = false;
  scene.add(grid);
  const circle = (r, n = 128) => { const pts = []; for (let i = 0; i < n; i++) { const a = (i / n) * Math.PI * 2; pts.push(new THREE.Vector3(Math.cos(a) * r, 0, Math.sin(a) * r)); } return new THREE.BufferGeometry().setFromPoints(pts); };
  const ceilEdge = new THREE.LineLoop(circle(ceilR), new THREE.LineBasicMaterial({ color: 0xede9e3, transparent: true, opacity: 0.32 }));
  ceilEdge.position.y = ceilY;
  scene.add(ceilEdge);

  // swap well beside the stack: glass cylinder (swap total) with water (swap used)
  const wellR = 1.5, wellH = u(d.memory.swap_total), waterH = u(d.memory.swap_used);
  const START_ANGLE = -3.3, WELL_OFF = -1.6; // initial camera angle; the well sits to the right of the stack
  const wellPos = new THREE.Vector3(Math.cos(START_ANGLE + WELL_OFF) * 11.8, 0, Math.sin(START_ANGLE + WELL_OFF) * 11.8);
  const glassMat = new THREE.MeshStandardMaterial({ color: 0xede9e3, transparent: true, opacity: 0.07, roughness: 0.1, metalness: 0, depthWrite: false });
  const well = new THREE.Mesh(new THREE.CylinderGeometry(wellR, wellR, wellH, 64, 1, true), glassMat);
  well.position.set(wellPos.x, wellH / 2, wellPos.z); scene.add(well);
  const rimPts = []; for (let i = 0; i <= 96; i++) { const a = (i / 96) * Math.PI * 2; rimPts.push(new THREE.Vector3(Math.cos(a) * wellR, 0, Math.sin(a) * wellR)); }
  const rimGeo = new THREE.BufferGeometry().setFromPoints(rimPts);
  const wellRim = new THREE.Line(rimGeo, new THREE.LineBasicMaterial({ color: 0xede9e3, transparent: true, opacity: 0.5 }));
  wellRim.position.set(wellPos.x, wellH, wellPos.z); scene.add(wellRim);
  const wellBase = new THREE.Line(rimGeo, new THREE.LineBasicMaterial({ color: 0xede9e3, transparent: true, opacity: 0.25 }));
  wellBase.position.set(wellPos.x, 0.01, wellPos.z); scene.add(wellBase);
  const ribPts = [];
  for (let i = 0; i < 28; i++) { const a = (i / 28) * Math.PI * 2, x = Math.cos(a) * wellR, z = Math.sin(a) * wellR; ribPts.push(new THREE.Vector3(x, 0, z), new THREE.Vector3(x, wellH, z)); }
  const wellRibs = new THREE.LineSegments(new THREE.BufferGeometry().setFromPoints(ribPts), new THREE.LineBasicMaterial({ color: 0xede9e3, transparent: true, opacity: 0.12, depthWrite: false }));
  wellRibs.position.set(wellPos.x, 0, wellPos.z); scene.add(wellRibs);
  const waterMat = new THREE.MeshStandardMaterial({ color: 0x8f877c, transparent: true, opacity: 0.6, roughness: 0.15, metalness: 0.1 });
  const water = new THREE.Mesh(new THREE.CylinderGeometry(wellR * 0.96, wellR * 0.96, waterH, 64), waterMat);
  water.position.set(wellPos.x, waterH / 2, wellPos.z); water.castShadow = true; scene.add(water);
  const surfMat = new THREE.MeshStandardMaterial({ color: 0xcfc9c0, transparent: true, opacity: 0.55, roughness: 0.05, metalness: 0.2, side: THREE.DoubleSide });
  const surface = new THREE.Mesh(new THREE.CircleGeometry(wellR * 0.96, 64), surfMat);
  surface.rotation.x = -Math.PI / 2; surface.position.set(wellPos.x, waterH + 0.005, wellPos.z); scene.add(surface);

  // the spiral staircase: one annular-sector step per group, unit height, scaled in y
  const N = steps.length;
  const r1 = 2.0, r2 = 8.6, gap = 0, // no slit between steps: a 1-3 px gap aliases into dashes; shared faces stay inside the solid
   span = (Math.PI * 2) / N;
  const a0 = Math.PI * 0.95 + 0.5; // the first (largest) steps, the reclaim candidates, face the camera
  const solids = [];
  const PO = { polygonOffset: true, polygonOffsetFactor: 1, polygonOffsetUnits: 1 }; // faces sit behind their edge lines
  const plinthMat = new THREE.MeshStandardMaterial({ roughness: 0.85, metalness: 0.02, ...PO });
  steps.forEach((s, i) => {
    const from = a0 - i * span + gap, to = a0 - (i + 1) * span - gap;
    const shape = new THREE.Shape();
    shape.absarc(0, 0, r2, from, to, true);
    shape.absarc(0, 0, r1, to, from, false);
    shape.closePath();
    const geo = new THREE.ExtrudeGeometry(shape, { depth: 1, bevelEnabled: false, curveSegments: 24 });
    geo.rotateX(-Math.PI / 2);
    const edges = horizontalEdges(geo);
    const band = new THREE.Mesh(geo, new THREE.MeshStandardMaterial({ roughness: 0.55, metalness: 0.05, ...PO }));
    band.castShadow = true; band.receiveShadow = false; // shadow-map edges on the thin side faces read as dashes
    const bandEdge = new THREE.LineSegments(edges, new THREE.LineBasicMaterial({ transparent: true, opacity: 0.6 }));
    band.add(bandEdge);
    const plinth = new THREE.Mesh(geo, plinthMat);
    plinth.castShadow = true; plinth.receiveShadow = true;
    band.scale.y = 0.001; plinth.scale.y = 0.001;
    scene.add(plinth); scene.add(band);
    band.userData.step = s; plinth.userData.step = s;
    const mid = (from + to) / 2;
    s.anchor = new THREE.Vector3(Math.cos(mid) * (r1 + r2) / 2, 0, -Math.sin(mid) * (r1 + r2) / 2);
    s.mesh = band; s.edge = bandEdge; s.shaft = plinth;
    s.outer = new THREE.Vector3(Math.cos(mid) * (r2 + 0.2), 0, -Math.sin(mid) * (r2 + 0.2));
    if (s.reclaimable) {
      // where the block was, once reclaimed: a faint outline, so the freed space stays visible
      s.ghost = new THREE.LineSegments(edges, new THREE.LineBasicMaterial({ transparent: true, opacity: 0, depthWrite: false }));
      s.ghost.scale.y = u(s.footprint); s.ghost.visible = false; scene.add(s.ghost);
      s.tag = document.createElement("div");
      s.tag.className = "scene-label scene-label-step"; s.tag.hidden = true; s.tag.setAttribute("aria-hidden", "true");
      sceneEl.appendChild(s.tag);
    }
    s.cur = { h: 0, b: 0 }; s.tgt = { h: 0, b: 0 };
    solids.push(band, plinth);
  });

  function layout(instant) {
    let acc = 0;
    for (const s of steps) {
      s.tgt.b = acc;
      s.tgt.h = s.reclaimed ? 0 : u(s.footprint);
      acc += s.tgt.h;
      if (instant) { s.cur.b = s.tgt.b; s.cur.h = s.tgt.h; }
    }
    counter.sum = sumAll();
    counter.headroom = d.headroom.bytes + reclaimedGain;
    if (instant) { shown.sum = counter.sum; shown.headroom = counter.headroom; }
  }
  function apply() {
    for (const s of steps) {
      s.mesh.scale.y = Math.max(0.001, s.cur.h); s.mesh.position.y = s.cur.b;
      s.mesh.visible = s.cur.h > 0.004;
      s.shaft.scale.y = Math.max(0.001, s.cur.b); s.shaft.visible = s.cur.b > 0.004;
      if (s.ghost) {
        const gone = s.reclaimed ? Math.max(0, 1 - s.cur.h / u(s.footprint)) : 0; // 0 = standing, 1 = fully reclaimed
        s.ghost.position.y = s.cur.b; s.ghost.material.opacity = 0.55 * gone; s.ghost.visible = gone > 0.02;
      }
    }
  }

  let hovered = null, hl = null;
  function paintColors() {
    const kinds = {
      agent_session: cssVar("--kind-agent"), app: cssVar("--kind-app"), system: cssVar("--kind-system"),
      other: cssVar("--kind-other"), build_daemon: cssVar("--kind-app"), model_server: cssVar("--kind-agent"), sandbox: cssVar("--kind-other")
    };
    const accent = new THREE.Color(cssVar("--accent"));
    const line = new THREE.Color(cssVar("--line-2"));
    const bg = new THREE.Color(cssVar("--scene-bg"));
    const light = document.documentElement.dataset.theme === "light" ||
      (!document.documentElement.dataset.theme && window.matchMedia("(prefers-color-scheme: light)").matches);
    scene.fog.color.copy(bg);
    groundMat.color.set(cssVar("--scene-ground"));
    const ink = new THREE.Color(cssVar("--text"));
    for (const m of [ceilMat, glassMat]) m.color.copy(ink);
    ceilMat.opacity = light ? 0.05 : 0.035;
    grid.material.color.copy(ink); grid.material.opacity = light ? 0.12 : 0.07;
    ceilEdge.material.color.copy(ink); wellRim.material.color.copy(ink); wellBase.material.color.copy(ink); wellRibs.material.color.copy(ink);
    wellRibs.material.opacity = light ? 0.2 : 0.12;
    waterMat.color.set(light ? "#b9b1a5" : "#8f877c");
    surfMat.color.set(light ? "#ece6dc" : "#cfc9c0");
    hemi.intensity = light ? 1.5 : 1.0;
    hemi.groundColor.set(light ? "#d8d2c8" : "#1a1612");
    key.intensity = light ? 1.9 : 2.3;
    plinthMat.color.set(cssVar("--plinth"));
    scene.traverse((o) => { if (o.userData.ring) o.material.color.copy(line); });
    for (const s of steps) {
      const isHot = s === hovered || s === hl;
      if (s.reclaimable) {
        s.mesh.material.color.copy(accent);
        s.mesh.material.emissive.copy(accent).multiplyScalar(isHot ? 0.5 : 0.22);
        s.edge.material.color.copy(accent).offsetHSL(0, 0, light ? -0.3 : 0.15);
        s.edge.material.opacity = 0.95;
        s.ghost.material.color.copy(accent).offsetHSL(0, 0, light ? -0.25 : 0);
      } else {
        s.mesh.material.color.set(kinds[s.kind] || kinds.other);
        s.mesh.material.emissive.set(0x000000);
        if (isHot) s.mesh.material.emissive.copy(ink).multiplyScalar(light ? 0.05 : 0.15);
        s.edge.material.color.copy(light ? new THREE.Color("#ffffff") : bg);
        s.edge.material.opacity = light ? 0.9 : 0.6;
      }
    }
    requestRender();
  }

  /* ── camera / orbit ── */
  let angle = START_ANGLE, sway = 0, tilt = 0.0, dragging = false, dragFrom = null, moved = 0, lastInteract = -1e9;
  let viewW = 1, viewH = 1, narrow = false;
  const og = document.documentElement.classList.contains("og"); // social-card render mode (og.png)
  function placeCamera() {
    const dist = narrow ? 47 : og ? 44 : 50;
    const h = 19 + tilt * 10;
    camera.position.set(Math.cos(angle) * dist, h, Math.sin(angle) * dist);
    camera.lookAt(0, lookY, 0);
    fill.position.copy(camera.position);
  }
  function resize() {
    const r = sceneEl.getBoundingClientRect();
    viewW = Math.max(1, r.width); viewH = Math.max(1, r.height);
    narrow = window.matchMedia("(max-width: 900px)").matches;
    renderer.setSize(viewW, viewH, false);
    camera.aspect = viewW / viewH;
    // desktop: push the scene right of the copy and up (clear of the HUD); the ?og card has no HUD, so only right
    if (!narrow) camera.setViewOffset(viewW, viewH, -viewW * (og ? 0.2 : 0.19), viewH * (og ? 0.03 : 0.15), viewW, viewH);
    else camera.clearViewOffset();
    camera.updateProjectionMatrix();
    placeCamera();
    requestRender();
  }

  /* ── picking ── */
  const ray = new THREE.Raycaster();
  const ndc = new THREE.Vector2();
  function pick(clientX, clientY) {
    const r = canvas.getBoundingClientRect();
    ndc.set(((clientX - r.left) / r.width) * 2 - 1, -((clientY - r.top) / r.height) * 2 + 1);
    ray.setFromCamera(ndc, camera);
    // the first surface hit wins; a plinth (the stack below a step) is not that group, so it shows nothing
    const hit = ray.intersectObjects(solids.filter((m) => m.visible), false)[0];
    return hit && hit.object === hit.object.userData.step.mesh ? hit.object.userData.step : null;
  }
  function showTip(s, clientX, clientY) {
    if (!s) { tip.hidden = true; return; }
    const r = sceneEl.getBoundingClientRect();
    const kind = KIND[s.kind] || s.kind;
    let html = `<b></b><span class="tk"></span>`;
    if (s.reclaimable) html += `<div class="ta"></div>`;
    tip.innerHTML = html;
    tip.querySelector("b").textContent = s.label;
    tip.querySelector(".tk").textContent = `${kind} · ${fmt(s.footprint)} footprint`;
    if (s.reclaimable) tip.querySelector(".ta").textContent = s.reclaimed ? "reclaimed · click to restore" : `reclaim ≈${fmt(s.gain)} · click to reclaim`;
    tip.hidden = false;
    let x = clientX - r.left, y = clientY - r.top;
    const tw = tip.offsetWidth, th = tip.offsetHeight;
    if (x + tw + 24 > r.width) x -= tw + 28;
    if (y + th + 24 > r.height) y -= th + 28;
    tip.style.left = x + "px"; tip.style.top = y + "px";
  }
  function highlight(s) { hl = s; paintColors(); }

  canvas.addEventListener("pointerdown", (e) => {
    dragging = true; moved = 0; dragFrom = { x: e.clientX, y: e.clientY, angle, tilt };
    lastInteract = performance.now();
  });
  window.addEventListener("pointerup", (e) => {
    if (!dragging) return;
    dragging = false; canvas.classList.remove("dragging");
    if (moved < 6 && e.target === canvas) {
      const s = pick(e.clientX, e.clientY);
      if (s && s.reclaimable) s.reclaimed ? restore(s) : reclaim(s);
      showTip(s, e.clientX, e.clientY);
    }
  });
  canvas.addEventListener("pointermove", (e) => {
    if (dragging && dragFrom) {
      const dx = e.clientX - dragFrom.x, dy = e.clientY - dragFrom.y;
      moved = Math.max(moved, Math.hypot(dx, dy));
      if (moved > 6 && e.pointerType === "mouse") {
        canvas.classList.add("dragging");
        angle = dragFrom.angle - dx * 0.006;
        tilt = Math.max(-0.4, Math.min(0.6, dragFrom.tilt + dy * 0.003));
        tip.hidden = true;
        lastInteract = performance.now();
        requestRender();
        return;
      }
    }
    if (e.pointerType !== "mouse") return;
    const s = pick(e.clientX, e.clientY);
    if (s !== hovered) { hovered = s; paintColors(); }
    canvas.classList.toggle("pointing", !!(s && s.reclaimable));
    showTip(s, e.clientX, e.clientY);
  });
  canvas.addEventListener("pointerleave", () => {
    if (hovered) { hovered = null; paintColors(); }
    tip.hidden = true; canvas.classList.remove("pointing");
  });

  function update() {
    if (!flat) { layout(reduceMotion.matches); kick(); return; }
    shown.headroom = counter.headroom = d.headroom.bytes + reclaimedGain;
    shown.sum = counter.sum = sumAll();
    paintHud(); fallback(d, steps);
  }
  function reclaim(s) {
    if (s.reclaimed) return;
    s.reclaimed = true; reclaimedGain += s.gain;
    update();
    const b = buttons.get(s);
    if (b) { b.setAttribute("aria-pressed", "true"); b.textContent = `✓ ${s.label} reclaimed · undo`; }
    reset.hidden = false;
    live.textContent = `Reclaimed ${s.label}: headroom rises by about ${fmt(s.gain)} to ${fmt(d.headroom.bytes + reclaimedGain)}.`;
  }
  function restore(s) {
    if (!s.reclaimed) return;
    s.reclaimed = false; reclaimedGain -= s.gain;
    update();
    const b = buttons.get(s);
    if (b) { b.setAttribute("aria-pressed", "false"); b.textContent = `Reclaim ${s.label} ≈${fmt(s.gain)}`; }
    reset.hidden = !steps.some((x) => x.reclaimed);
    live.textContent = `${s.label} restored. Headroom ${fmt(d.headroom.bytes + reclaimedGain)}.`;
  }

  /* ── labels pinned to 3D points ── */
  const v3 = new THREE.Vector3();
  function pinLabel(el, x, y, z, center) {
    v3.set(x, y, z).project(camera);
    if (v3.z > 1) { el.hidden = true; return; }
    el.hidden = false;
    const half = el.offsetWidth / 2 + 8;
    el.style.left = Math.max(half, Math.min(viewW - half, (v3.x + 1) / 2 * viewW)) + "px";
    el.style.top = ((1 - v3.y) / 2 * viewH - (center ? 0 : 6)) + "px";
  }

  /* ── loop: runs only while visible, and only while something moves ── */
  let onScreen = true, raf = 0, last = performance.now(), needsFrame = true, intro = reduceMotion.matches ? 1 : 0, introStart = 0;
  function requestRender() { needsFrame = true; kick(); }
  function kick() { if (!raf && onScreen && !document.hidden) { last = performance.now(); raf = requestAnimationFrame(frame); } }
  function frame(now) {
    raf = 0;
    const dt = Math.min(0.05, (now - last) / 1000); last = now;
    let busy = false;
    if (!reduceMotion.matches) {
      if (!dragging && now - lastInteract > 2500) {
        // idle: a slow sway around the composed view (not a full orbit, so labels never swing over the copy)
        sway += dt * 0.16;
        const target = START_ANGLE + Math.sin(sway) * 0.32;
        angle += (target - angle) * (1 - Math.exp(-dt * 0.8));
        tilt += (0 - tilt) * (1 - Math.exp(-dt * 0.8));
        busy = true;
      }
      if (intro < 1) { introStart ||= now; intro = Math.min(1, (now - introStart) / 1800); busy = true; }
    }
    const k = reduceMotion.matches ? 1 : 1 - Math.exp(-dt * 5);
    const ease = 1 - Math.pow(1 - intro, 3);
    for (const s of steps) {
      for (const p of ["h", "b"]) {
        const target = s.tgt[p] * ease;
        const diff = target - s.cur[p];
        if (Math.abs(diff) > 0.0005) { s.cur[p] += diff * (intro < 1 ? 1 : k); busy = true; } else s.cur[p] = target;
      }
    }
    for (const key2 of ["headroom", "sum"]) {
      const diff = counter[key2] - shown[key2];
      if (Math.abs(diff) > 1e6) { shown[key2] += diff * k; busy = true; } else shown[key2] = counter[key2];
    }
    paintHud();
    apply();
    placeCamera();
    renderer.render(scene, camera);
    // the ceiling label rides the lid's rim, a little to the left of the point nearest the camera
    pinLabel(labelCeil, Math.cos(angle + 0.55) * ceilR, ceilY, Math.sin(angle + 0.55) * ceilR);
    pinLabel(labelSwap, wellPos.x, wellH + 0.25, wellPos.z);
    const top = steps[steps.length - 1];
    pinLabel(labelPeak, top.anchor.x, top.cur.b + top.cur.h + 0.25, top.anchor.z);
    labelPeak.textContent = "Σ " + fmt(shown.sum);
    let prevTag = null;
    for (const s of steps) {
      if (!s.tag) continue;
      // standing: name + size on the block's face; reclaimed: "freed" on top of its ghost outline
      const y = s.reclaimed ? s.cur.b + u(s.footprint) : s.cur.b + s.cur.h * 0.5;
      const text = s.reclaimed ? `freed ≈${fmt(s.gain)}` : `${s.label} · ${fmt(s.footprint)}`;
      if (s.tag.textContent !== text) s.tag.textContent = text;
      s.tag.classList.toggle("is-freed", !!s.reclaimed);
      // hide while the block faces away from the viewer (after a drag) or before the intro has built it
      const facing = (s.outer.x * camera.position.x + s.outer.z * camera.position.z) > 0;
      if (intro < 0.6 || !facing) { s.tag.hidden = true; continue; }
      pinLabel(s.tag, s.outer.x, y, s.outer.z, true);
      // keep tags from touching: lift a tag above the previous one when their boxes overlap
      if (prevTag) {
        const ax = parseFloat(s.tag.style.left), ay = parseFloat(s.tag.style.top);
        const bx = parseFloat(prevTag.style.left), by = parseFloat(prevTag.style.top);
        const w = (s.tag.offsetWidth + prevTag.offsetWidth) / 2 + 6, hh = (s.tag.offsetHeight + prevTag.offsetHeight) / 2 + 4;
        if (Math.abs(ax - bx) < w && Math.abs(ay - by) < hh) s.tag.style.top = (by - hh) + "px";
      }
      prevTag = s.tag;
    }
    needsFrame = false;
    if (busy || needsFrame) kick();
  }

  new IntersectionObserver((entries) => {
    onScreen = entries[0].isIntersecting;
    if (onScreen) kick();
  }, { threshold: 0.01 }).observe(sceneEl);
  document.addEventListener("visibilitychange", () => { if (!document.hidden) kick(); });
  reduceMotion.addEventListener?.("change", () => { layout(true); requestRender(); });
  window.addEventListener("oomtop-theme", paintColors);
  window.matchMedia("(prefers-color-scheme: light)").addEventListener("change", paintColors);
  new ResizeObserver(resize).observe(sceneEl);

  layout(false);
  resize();
  paintColors();
  sceneEl.classList.add("ready");
  window.__oomtopCity = { steps, reclaim, restore, renderer };
}

main().catch((err) => {
  console.warn("oomtop: 3D scene unavailable", err);
  window.oomtopSnapshot?.then((d) => fallback(d, buildSteps(d))).catch(() => {});
});
