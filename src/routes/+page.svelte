<script lang="ts">
  import { invoke } from "@tauri-apps/api/core";
  import { listen, type UnlistenFn } from "@tauri-apps/api/event";
  import { onMount, onDestroy } from "svelte";

  // ─── Constants ─────────────────────────────────────────────────────
  const N = 256;            // spectrum bin count (matches Rust payload)
  const M_RAYS = 600;       // sphere ray count
  const K_STARS = 220;      // background star count
  const TAU = Math.PI * 2;

  // Bin index → freq: bin_i ≈ i * 46.875 Hz.
  const BASS_RANGE: [number, number] = [1, 8];
  const MID_RANGE: [number, number] = [8, 40];
  const TREBLE_RANGE: [number, number] = [40, 128];

  // ─── Audio state ──────────────────────────────────────────────────
  type SpectrumFrame = { bins: number[]; waveform: number[]; rms: number };
  type AudioSession = { pid: number; exe: string; display: string; is_active: boolean };

  const bins = new Float32Array(N);
  const audio = { bass: 0, mid: 0, treble: 0, level: 0 };

  let bassMean = 0;
  let kickEnv = 0;
  let lastKickAt = 0;

  // ─── UI state ─────────────────────────────────────────────────────
  let canvasEl: HTMLCanvasElement;
  let running = $state(false);
  let error = $state<string | null>(null);
  let showHud = $state(true);
  let hudTimer: number | undefined;
  let unlisten: UnlistenFn | undefined;

  // Source picker: null = system-wide loopback; otherwise PID of a specific app.
  let sessions = $state<AudioSession[]>([]);
  let selectedPid = $state<number | null>(null);
  let refreshing = $state(false);
  let sourceOpen = $state(false);
  let sourceDdEl: HTMLDivElement | undefined;

  const LS_KEY = "wmp-modern.selectedPid";

  // ─── Now Playing (SMTC) ───────────────────────────────────────────
  type NowPlaying = {
    title: string;
    artist: string;
    album: string;
    status: string;
    position_ms: number;
    duration_ms: number;
    playback_rate: number;
    last_updated_unix_ms: number;
    source_app: string;
    thumbnail: string | null;
  };

  let nowPlaying = $state<NowPlaying | null>(null);
  // Local-clock snapshot of when we observed the last position, so the progress bar
  // can drift forward smoothly between polls without waiting for the next 1s tick.
  let npObservedAt = 0;
  let npPollTimer: number | undefined;
  let displayPos = $state(0);

  onMount(async () => {
    try { initGL(canvasEl); } catch (e) { error = String(e); return; }
    unlisten = await listen<SpectrumFrame>("audio-spectrum", onFrame);

    // Restore last source choice (if any) from localStorage.
    const stored = localStorage.getItem(LS_KEY);
    selectedPid = stored !== null && stored !== "" ? Number(stored) : null;

    await refreshSessions();
    await startWithSelected();

    // Start Now Playing polling.
    pollNowPlaying(); // immediate
    npPollTimer = window.setInterval(pollNowPlaying, 1000);

    bumpHud();
    window.addEventListener("mousemove", bumpHud);
    window.addEventListener("keydown", onKey);
    window.addEventListener("resize", resize);
    window.addEventListener("click", onWindowClick);
  });

  async function refreshSessions() {
    refreshing = true;
    try {
      sessions = await invoke<AudioSession[]>("list_audio_sessions");
    } catch (e) {
      error = String(e);
    } finally {
      refreshing = false;
    }
  }

  async function startWithSelected() {
    error = null;
    // If the restored PID isn't present anymore, fall back to system.
    if (selectedPid !== null && !sessions.some((s) => s.pid === selectedPid)) {
      selectedPid = null;
    }
    // Keep the SMTC card in sync with the audio source. Passing the exe name so
    // the backend can find the matching SMTC session via AUMID.
    const targetExe = selectedPid === null
      ? null
      : (sessions.find((s) => s.pid === selectedPid)?.exe ?? null);
    try { await invoke("set_smtc_target_exe", { exe: targetExe }); } catch {}
    try {
      await invoke("start_capture", { pid: selectedPid });
      running = true;
    } catch (e) {
      // If per-process capture fails (e.g. app exited between list and start), fall back.
      if (selectedPid !== null) {
        selectedPid = null;
        try {
          await invoke("start_capture", { pid: null });
          running = true;
          error = "Per-app capture failed, fell back to system audio.";
        } catch (e2) {
          error = String(e2);
        }
      } else {
        error = String(e);
      }
    }
  }

  async function selectSource(pid: number | null) {
    selectedPid = pid;
    localStorage.setItem(LS_KEY, pid === null ? "" : String(pid));
    sourceOpen = false;
    try { await invoke("stop_capture"); } catch {}
    running = false;
    await startWithSelected();
  }

  function onWindowClick(e: MouseEvent) {
    if (sourceOpen && sourceDdEl && !sourceDdEl.contains(e.target as Node)) {
      sourceOpen = false;
    }
  }

  type NowPlayingSnapshot = {
    current: NowPlaying | null;
    error: string | null;
    poll_age_ms: number;
  };

  async function pollNowPlaying() {
    try {
      const snap = await invoke<NowPlayingSnapshot>("get_now_playing");
      if (snap.current && (snap.current.title || snap.current.artist)) {
        nowPlaying = snap.current;
        npObservedAt = performance.now();
        displayPos = snap.current.position_ms;
      } else {
        nowPlaying = null;
      }
    } catch {
      nowPlaying = null;
    }
  }

  function fmtTime(ms: number): string {
    if (!isFinite(ms) || ms < 0) return "0:00";
    const s = Math.floor(ms / 1000);
    const m = Math.floor(s / 60);
    const r = s % 60;
    return `${m}:${r.toString().padStart(2, "0")}`;
  }

  function prettySource(aumid: string): string {
    // AUMIDs come in two flavors:
    //   - Desktop: "Spotify.exe", "chrome.exe", "discord.exe"
    //   - UWP/MSIX: "Microsoft.Edge_8wekyb3d8bbwe!App", "Spotify..._zpdnekdrzrea0!Spotify"
    // Strip the "!AppEntry" suffix, the ".exe" suffix, the "_publisherhash" segment,
    // then return the last dot-separated piece.
    if (!aumid) return "";
    let s = aumid.split("!")[0];
    if (s.toLowerCase().endsWith(".exe")) s = s.slice(0, -4);
    s = s.split("_")[0];                   // strip publisher hash on UWP names
    const parts = s.split(".");
    return parts[parts.length - 1] || s;
  }

  onDestroy(() => {
    unlisten?.();
    cancelAnimationFrame(rafId);
    if (npPollTimer) clearInterval(npPollTimer);
    window.removeEventListener("mousemove", bumpHud);
    window.removeEventListener("keydown", onKey);
    window.removeEventListener("resize", resize);
    window.removeEventListener("click", onWindowClick);
    if (hudTimer) clearTimeout(hudTimer);
  });

  function onFrame(e: { payload: SpectrumFrame }) {
    const p = e.payload;
    for (let i = 0; i < N; i++) {
      const v = Math.min(1, Math.sqrt(Math.max(0, p.bins[i] ?? 0)) * 3.5);
      bins[i] = bins[i] * 0.5 + v * 0.5;
    }
    audio.bass   = ema(audio.bass,   bandMean(p.bins, BASS_RANGE)   * 60, 0.78);
    audio.mid    = ema(audio.mid,    bandMean(p.bins, MID_RANGE)    * 80, 0.85);
    audio.treble = ema(audio.treble, bandMean(p.bins, TREBLE_RANGE) * 120, 0.85);
    audio.level  = ema(audio.level,  Math.min(1, p.rms * 3), 0.85);

    // Kick detector.
    bassMean = bassMean * 0.94 + audio.bass * 0.06;
    const excess = audio.bass - bassMean * 1.3;
    const now = performance.now();
    if (excess > 0.08 && now - lastKickAt > 180) {
      kickEnv = Math.min(1, kickEnv + excess * 4);
      lastKickAt = now;
    }
  }
  function bandMean(arr: number[], r: [number, number]): number {
    let s = 0;
    const end = Math.min(r[1], arr.length);
    for (let i = r[0]; i < end; i++) s += arr[i];
    return Math.sqrt(Math.max(0, s / Math.max(1, end - r[0])));
  }
  function ema(prev: number, next: number, alpha: number) {
    return prev * alpha + next * (1 - alpha);
  }

  function bumpHud() {
    showHud = true;
    if (hudTimer) clearTimeout(hudTimer);
    hudTimer = window.setTimeout(() => { showHud = false; }, 3000);
  }
  function onKey(e: KeyboardEvent) {
    if (e.key === "f" || e.key === "F") {
      if (document.fullscreenElement) document.exitFullscreen();
      else document.documentElement.requestFullscreen();
    }
  }
  async function start() {
    await startWithSelected();
  }
  async function stop() {
    try { await invoke("stop_capture"); } catch {}
    running = false;
  }

  // ─── WebGL setup ──────────────────────────────────────────────────
  const VS_LINE = `#version 300 es
  in vec3 aPos;
  in vec3 aColor;
  uniform mat4 uMVP;
  uniform float uPointSize;
  out vec3 vColor;
  void main() {
    gl_Position = uMVP * vec4(aPos, 1.0);
    vColor = aColor;
    gl_PointSize = uPointSize;
  }`;
  const FS_LINE = `#version 300 es
  precision mediump float;
  in vec3 vColor;
  uniform float uIntensity;
  out vec4 oColor;
  void main() { oColor = vec4(vColor * uIntensity, 1.0); }`;

  const VS_QUAD = `#version 300 es
  in vec2 aPos;
  void main() { gl_Position = vec4(aPos, 0.0, 1.0); }`;
  const FS_QUAD = `#version 300 es
  precision mediump float;
  uniform vec4 uColor;
  out vec4 oColor;
  void main() { oColor = uColor; }`;

  let gl: WebGL2RenderingContext;
  let lineProgram: WebGLProgram;
  let quadProgram: WebGLProgram;
  let uMVP: WebGLUniformLocation | null;
  let uIntensity: WebGLUniformLocation | null;
  let uPointSize: WebGLUniformLocation | null;
  let uQuadColor: WebGLUniformLocation | null;

  // Sphere ray data (pre-baked direction per ray; lengths streamed each frame).
  const rayDir = new Float32Array(M_RAYS * 3);     // unit direction per ray
  const rayP   = new Float32Array(M_RAYS * 2 * 3); // line start + end
  const rayC   = new Float32Array(M_RAYS * 2 * 3);
  const tipP   = new Float32Array(M_RAYS * 3);     // ray tip positions (for points)
  const tipC   = new Float32Array(M_RAYS * 3);

  // Starfield
  const starState = new Float32Array(K_STARS * 4); // x, y, z, hue
  const starP     = new Float32Array(K_STARS * 3);
  const starC     = new Float32Array(K_STARS * 3);

  let rayPVbo: WebGLBuffer, rayCVbo: WebGLBuffer;
  let tipPVbo: WebGLBuffer, tipCVbo: WebGLBuffer;
  let starPVbo: WebGLBuffer, starCVbo: WebGLBuffer;
  let quadVbo: WebGLBuffer;
  let lineVao: WebGLVertexArrayObject;
  let quadVao: WebGLVertexArrayObject;

  let rafId = 0;
  let startTime = 0;
  let lastTime = 0;

  function compile(gl: WebGL2RenderingContext, type: number, src: string) {
    const sh = gl.createShader(type)!;
    gl.shaderSource(sh, src.trim());
    gl.compileShader(sh);
    if (!gl.getShaderParameter(sh, gl.COMPILE_STATUS))
      throw new Error(`shader: ${gl.getShaderInfoLog(sh)}`);
    return sh;
  }
  function linkProgram(gl: WebGL2RenderingContext, vs: string, fs: string) {
    const p = gl.createProgram()!;
    gl.attachShader(p, compile(gl, gl.VERTEX_SHADER, vs));
    gl.attachShader(p, compile(gl, gl.FRAGMENT_SHADER, fs));
    gl.linkProgram(p);
    if (!gl.getProgramParameter(p, gl.LINK_STATUS))
      throw new Error(`link: ${gl.getProgramInfoLog(p)}`);
    return p;
  }

  function initGL(canvas: HTMLCanvasElement) {
    const ctx = canvas.getContext("webgl2", { antialias: true, alpha: false });
    if (!ctx) throw new Error("WebGL2 not available");
    gl = ctx;

    lineProgram = linkProgram(gl, VS_LINE, FS_LINE);
    quadProgram = linkProgram(gl, VS_QUAD, FS_QUAD);
    uMVP        = gl.getUniformLocation(lineProgram, "uMVP");
    uIntensity  = gl.getUniformLocation(lineProgram, "uIntensity");
    uPointSize  = gl.getUniformLocation(lineProgram, "uPointSize");
    uQuadColor  = gl.getUniformLocation(quadProgram, "uColor");

    rayPVbo  = gl.createBuffer()!;
    rayCVbo  = gl.createBuffer()!;
    tipPVbo  = gl.createBuffer()!;
    tipCVbo  = gl.createBuffer()!;
    starPVbo = gl.createBuffer()!;
    starCVbo = gl.createBuffer()!;

    lineVao = gl.createVertexArray()!;
    quadVao = gl.createVertexArray()!;

    // Trail-fade quad.
    gl.bindVertexArray(quadVao);
    quadVbo = gl.createBuffer()!;
    gl.bindBuffer(gl.ARRAY_BUFFER, quadVbo);
    gl.bufferData(gl.ARRAY_BUFFER, new Float32Array([-1, -1, 3, -1, -1, 3]), gl.STATIC_DRAW);
    const aQuadPos = gl.getAttribLocation(quadProgram, "aPos");
    gl.enableVertexAttribArray(aQuadPos);
    gl.vertexAttribPointer(aQuadPos, 2, gl.FLOAT, false, 0, 0);
    gl.bindVertexArray(null);

    // Pre-bake ray directions on a Fibonacci sphere (uniform-ish distribution).
    const golden = Math.PI * (3 - Math.sqrt(5));
    for (let i = 0; i < M_RAYS; i++) {
      const y = 1 - (i / (M_RAYS - 1)) * 2;
      const r = Math.sqrt(1 - y * y);
      const theta = golden * i;
      rayDir[i * 3 + 0] = Math.cos(theta) * r;
      rayDir[i * 3 + 1] = y;
      rayDir[i * 3 + 2] = Math.sin(theta) * r;
    }

    // Seed stars.
    for (let i = 0; i < K_STARS; i++) {
      starState[i * 4 + 0] = (Math.random() - 0.5) * 8;
      starState[i * 4 + 1] = (Math.random() - 0.5) * 5;
      starState[i * 4 + 2] = -Math.random() * 20 - 1;
      starState[i * 4 + 3] = Math.random();
    }

    startTime = performance.now();
    resize();
    rafId = requestAnimationFrame(render);
  }

  function resize() {
    const dpr = Math.min(window.devicePixelRatio || 1, 2);
    const w = Math.floor(window.innerWidth * dpr);
    const h = Math.floor(window.innerHeight * dpr);
    if (canvasEl.width !== w || canvasEl.height !== h) {
      canvasEl.width = w;
      canvasEl.height = h;
    }
    gl.viewport(0, 0, w, h);
  }

  // ─── Matrix helpers (column-major mat4) ──────────────────────────
  const mvp   = new Float32Array(16);
  const view  = new Float32Array(16);
  const proj  = new Float32Array(16);
  const model = new Float32Array(16);
  const tmp   = new Float32Array(16);

  function mat4Identity(o: Float32Array) {
    o.fill(0); o[0] = 1; o[5] = 1; o[10] = 1; o[15] = 1;
  }
  function mat4Perspective(o: Float32Array, fovY: number, aspect: number, near: number, far: number) {
    const f = 1 / Math.tan(fovY / 2);
    o.fill(0);
    o[0] = f / aspect; o[5] = f;
    o[10] = (far + near) / (near - far);
    o[11] = -1;
    o[14] = (2 * far * near) / (near - far);
  }
  function mat4LookAt(o: Float32Array, ex: number, ey: number, ez: number, cx: number, cy: number, cz: number) {
    let fx = cx - ex, fy = cy - ey, fz = cz - ez;
    const fl = Math.hypot(fx, fy, fz); fx /= fl; fy /= fl; fz /= fl;
    let sx = fy * 0 - fz * 1, sy = fz * 0 - fx * 0, sz = fx * 1 - fy * 0;
    const sl = Math.hypot(sx, sy, sz); sx /= sl; sy /= sl; sz /= sl;
    const ux = sy * fz - sz * fy, uy = sz * fx - sx * fz, uz = sx * fy - sy * fx;
    o[0] = sx;  o[4] = sy;  o[8] = sz;  o[12] = -(sx * ex + sy * ey + sz * ez);
    o[1] = ux;  o[5] = uy;  o[9] = uz;  o[13] = -(ux * ex + uy * ey + uz * ez);
    o[2] = -fx; o[6] = -fy; o[10] = -fz;o[14] = (fx * ex + fy * ey + fz * ez);
    o[3] = 0;   o[7] = 0;   o[11] = 0;  o[15] = 1;
  }
  function mat4Mul(o: Float32Array, a: Float32Array, b: Float32Array) {
    for (let i = 0; i < 4; i++) {
      const ai0 = a[i], ai1 = a[i + 4], ai2 = a[i + 8], ai3 = a[i + 12];
      o[i]      = ai0 * b[0]  + ai1 * b[1]  + ai2 * b[2]  + ai3 * b[3];
      o[i + 4]  = ai0 * b[4]  + ai1 * b[5]  + ai2 * b[6]  + ai3 * b[7];
      o[i + 8]  = ai0 * b[8]  + ai1 * b[9]  + ai2 * b[10] + ai3 * b[11];
      o[i + 12] = ai0 * b[12] + ai1 * b[13] + ai2 * b[14] + ai3 * b[15];
    }
  }
  function mat4RotateY(o: Float32Array, m: Float32Array, a: number) {
    const c = Math.cos(a), s = Math.sin(a);
    mat4Identity(tmp);
    tmp[0] = c; tmp[2] = -s; tmp[8] = s; tmp[10] = c;
    mat4Mul(o, m, tmp);
  }

  // Pastel palette. t in [0,1).
  function pal(t: number, out: Float32Array, offset: number) {
    const TT = TAU * t;
    out[offset]     = 0.55 + 0.30 * Math.cos(TT);
    out[offset + 1] = 0.55 + 0.30 * Math.cos(TT + 2.094);
    out[offset + 2] = 0.55 + 0.30 * Math.cos(TT + 4.188);
  }

  // ─── Per-frame geometry ──────────────────────────────────────────
  function buildSphere(t: number, camY: number) {
    // Spectrum-driven spike ball: each ray's length is one frequency bin.
    // Color depends on direction (gives a real 3D feel: front rays brighter than back).
    // Slow rotation about Y axis so we see different sides over time.
    const ROT_Y = t * 0.18;
    const cosY = Math.cos(ROT_Y), sinY = Math.sin(ROT_Y);

    const innerR = 0.15 + audio.bass * 0.08;
    const baseLen = 1.05 + kickEnv * 0.45;    // ball "breathes" with bass kicks
    const lenScale = 1.3;
    const hueDrift = t * 0.012;

    for (let i = 0; i < M_RAYS; i++) {
      // Rotate direction about Y.
      const dx0 = rayDir[i * 3 + 0];
      const dy0 = rayDir[i * 3 + 1];
      const dz0 = rayDir[i * 3 + 2];
      const dx =  cosY * dx0 + sinY * dz0;
      const dy =  dy0;
      const dz = -sinY * dx0 + cosY * dz0;

      const binIdx = i % N;
      const mag = bins[binIdx];
      const len = innerR + 0.08 + mag * lenScale * baseLen;

      const inX = dx * innerR, inY = dy * innerR, inZ = dz * innerR;
      const outX = dx * len,   outY = dy * len,   outZ = dz * len;

      const b6 = i * 6;
      rayP[b6 + 0] = inX;  rayP[b6 + 1] = inY;  rayP[b6 + 2] = inZ;
      rayP[b6 + 3] = outX; rayP[b6 + 4] = outY; rayP[b6 + 5] = outZ;

      // Front-back fade: rays pointing toward camera (+Z direction) are brighter.
      // Camera is roughly at (0, camY, +2.5) looking at origin.
      const front = 0.5 + 0.5 * dz; // 0..1
      const facing = 0.35 + 0.65 * front;

      // Hue from index + slow time drift.
      const huePos = (i / M_RAYS) + hueDrift;
      pal(huePos, rayC, b6 + 3);          // tip color
      rayC[b6 + 3] *= facing;
      rayC[b6 + 4] *= facing;
      rayC[b6 + 5] *= facing;
      // Root: dim version of same hue.
      rayC[b6 + 0] = rayC[b6 + 3] * 0.18;
      rayC[b6 + 1] = rayC[b6 + 4] * 0.18;
      rayC[b6 + 2] = rayC[b6 + 5] * 0.18;

      // Tip point — same position as the ray tip, brighter.
      tipP[i * 3 + 0] = outX;
      tipP[i * 3 + 1] = outY;
      tipP[i * 3 + 2] = outZ;
      tipC[i * 3 + 0] = Math.min(1, rayC[b6 + 3] * 1.4);
      tipC[i * 3 + 1] = Math.min(1, rayC[b6 + 4] * 1.4);
      tipC[i * 3 + 2] = Math.min(1, rayC[b6 + 5] * 1.4);
    }
  }

  function buildStars(dt: number, t: number) {
    const baseSpeed = 0.35;
    const speed = baseSpeed + kickEnv * 3.0;
    for (let i = 0; i < K_STARS; i++) {
      const idx = i * 4;
      starState[idx + 2] += dt * speed;
      if (starState[idx + 2] > 1.5) {
        starState[idx + 0] = (Math.random() - 0.5) * 8;
        starState[idx + 1] = (Math.random() - 0.5) * 5;
        starState[idx + 2] = -18 - Math.random() * 4;
        starState[idx + 3] = Math.random();
      }
      const x = starState[idx + 0];
      const y = starState[idx + 1];
      const z = starState[idx + 2];
      const hue = starState[idx + 3];

      starP[i * 3 + 0] = x;
      starP[i * 3 + 1] = y;
      starP[i * 3 + 2] = z;

      const proximity = Math.max(0, Math.min(1, (z + 16) / 16));
      const fade = proximity * proximity * 0.45; // stars stay dim (background only)
      pal(hue + t * 0.005, starC, i * 3);
      starC[i * 3 + 0] *= fade;
      starC[i * 3 + 1] *= fade;
      starC[i * 3 + 2] *= fade;
    }
  }

  function bindLineRing(posVbo: WebGLBuffer, colVbo: WebGLBuffer, posData: Float32Array, colData: Float32Array) {
    const aPos = gl.getAttribLocation(lineProgram, "aPos");
    const aCol = gl.getAttribLocation(lineProgram, "aColor");
    gl.bindBuffer(gl.ARRAY_BUFFER, posVbo);
    gl.bufferData(gl.ARRAY_BUFFER, posData, gl.DYNAMIC_DRAW);
    gl.enableVertexAttribArray(aPos);
    gl.vertexAttribPointer(aPos, 3, gl.FLOAT, false, 0, 0);
    gl.bindBuffer(gl.ARRAY_BUFFER, colVbo);
    gl.bufferData(gl.ARRAY_BUFFER, colData, gl.DYNAMIC_DRAW);
    gl.enableVertexAttribArray(aCol);
    gl.vertexAttribPointer(aCol, 3, gl.FLOAT, false, 0, 0);
  }

  function computeMVP(t: number): number {
    const aspect = canvasEl.width / Math.max(1, canvasEl.height);
    mat4Perspective(proj, (Math.PI / 180) * 55, aspect, 0.1, 100);
    // Orbital camera around the spiky ball.
    const orbit = t * 0.06;
    const tilt = 0.30 + Math.sin(t * 0.05) * 0.08;
    const dist = 2.5 - kickEnv * 0.5;
    const ex = Math.sin(orbit) * dist * Math.cos(tilt);
    const ez = Math.cos(orbit) * dist * Math.cos(tilt);
    const ey = Math.sin(tilt) * dist;
    mat4LookAt(view, ex, ey, ez, 0, 0, 0);
    mat4Identity(model);
    mat4Mul(mvp, proj, view);
    return ey;
  }

  function render() {
    rafId = requestAnimationFrame(render);
    const now = performance.now();
    const t = (now - startTime) / 1000;
    const dt = lastTime === 0 ? 0.016 : Math.min(0.05, (now - lastTime) / 1000);
    lastTime = now;

    // Kick envelope decay.
    kickEnv *= Math.pow(0.04, dt);

    // Smooth Now Playing position interpolation between polls.
    if (nowPlaying && nowPlaying.status === "playing") {
      const elapsed = now - npObservedAt;
      const extrapolated = nowPlaying.position_ms + elapsed * nowPlaying.playback_rate;
      displayPos = Math.min(nowPlaying.duration_ms || extrapolated, extrapolated);
    }

    // Trail fade.
    gl.bindVertexArray(quadVao);
    gl.useProgram(quadProgram);
    gl.enable(gl.BLEND);
    gl.blendFunc(gl.SRC_ALPHA, gl.ONE_MINUS_SRC_ALPHA);
    const fadeA = 0.10 + audio.level * 0.05;
    gl.uniform4f(uQuadColor!, 0.012, 0.012, 0.025, fadeA);
    gl.drawArrays(gl.TRIANGLES, 0, 3);

    // Geometry.
    const camY = computeMVP(t);
    buildStars(dt, t);
    buildSphere(t, camY);

    gl.bindVertexArray(lineVao);
    gl.useProgram(lineProgram);
    gl.uniformMatrix4fv(uMVP!, false, mvp);
    gl.blendFunc(gl.SRC_ALPHA, gl.ONE);

    // Stars (back).
    gl.uniform1f(uPointSize!, 1.8);
    gl.uniform1f(uIntensity!, 0.7);
    bindLineRing(starPVbo, starCVbo, starP, starC);
    gl.drawArrays(gl.POINTS, 0, K_STARS);

    // Spike ball: rays.
    gl.uniform1f(uIntensity!, 0.85 + audio.treble * 0.5 + kickEnv * 0.4);
    bindLineRing(rayPVbo, rayCVbo, rayP, rayC);
    gl.drawArrays(gl.LINES, 0, M_RAYS * 2);

    // Spike ball: tip points (glowing dots at each ray's end).
    gl.uniform1f(uPointSize!, 2.5 + kickEnv * 2.0);
    gl.uniform1f(uIntensity!, 1.1 + kickEnv * 0.6);
    bindLineRing(tipPVbo, tipCVbo, tipP, tipC);
    gl.drawArrays(gl.POINTS, 0, M_RAYS);
  }
</script>

<canvas bind:this={canvasEl}></canvas>

{#if nowPlaying && (nowPlaying.title || nowPlaying.artist)}
  <div class="np" class:hidden={!showHud}>
    {#if nowPlaying.thumbnail}
      <img class="np-art" src={nowPlaying.thumbnail} alt="" />
    {:else}
      <div class="np-art np-art-placeholder">♪</div>
    {/if}
    <div class="np-text">
      <div class="np-title" title={nowPlaying.title}>{nowPlaying.title || "Unknown"}</div>
      <div class="np-artist" title={nowPlaying.artist}>
        {nowPlaying.artist}{nowPlaying.album ? ` — ${nowPlaying.album}` : ""}
      </div>
      {#if nowPlaying.duration_ms > 0}
        <div class="np-progress-row">
          <span class="np-time">{fmtTime(displayPos)}</span>
          <div class="np-progress">
            <div class="np-progress-fill" style="width: {Math.min(100, (displayPos / nowPlaying.duration_ms) * 100)}%"></div>
          </div>
          <span class="np-time">{fmtTime(nowPlaying.duration_ms)}</span>
        </div>
      {/if}
      <div class="np-status">
        {nowPlaying.status === "playing" ? "▶" : nowPlaying.status === "paused" ? "⏸" : "■"}
        <span class="np-source">{prettySource(nowPlaying.source_app)}</span>
      </div>
    </div>
  </div>
{/if}

<div class="hud" class:hidden={!showHud}>
  <div class="title">wmp-modern</div>

  <div class="source" bind:this={sourceDdEl}>
    <span class="label-text">Source</span>
    <div class="source-dd">
      <button class="source-trigger" onclick={(e) => { e.stopPropagation(); sourceOpen = !sourceOpen; }}>
        <span class="source-current">{
          selectedPid === null
            ? "System (all audio)"
            : (sessions.find((s) => s.pid === selectedPid)?.display ?? "System (all audio)")
        }</span>
        <span class="chevron" class:open={sourceOpen}>▾</span>
      </button>
      {#if sourceOpen}
        <div class="source-list">
          <button
            class="source-opt"
            class:active={selectedPid === null}
            onclick={() => selectSource(null)}>
            System (all audio)
          </button>
          {#each sessions as s (s.pid)}
            <button
              class="source-opt"
              class:active={selectedPid === s.pid}
              class:idle={!s.is_active}
              onclick={() => selectSource(s.pid)}>
              {s.display}{s.is_active ? "" : " (idle)"}
            </button>
          {/each}
        </div>
      {/if}
    </div>
    <button class="refresh" onclick={refreshSessions} disabled={refreshing} title="Refresh sources">
      {refreshing ? "…" : "↻"}
    </button>
  </div>

  <div class="controls">
    {#if running}
      <button onclick={stop}>Stop</button>
    {:else}
      <button onclick={start}>Start</button>
    {/if}
    <span class="status">{running ? "● capturing" : "○ idle"}</span>
  </div>
  {#if error}<p class="err">{error}</p>{/if}
  <p class="hint">Press <kbd>F</kbd> for fullscreen.</p>
</div>

<style>
  /* color-scheme: dark tells the OS/browser to render native form controls (e.g. the
     opened <option> list rendered by the OS) using dark default colors. Without this,
     opened dropdowns appear as light text on white because the OS picks light theme. */
  :global(html) { color-scheme: dark; margin: 0; padding: 0; height: 100%; overflow: hidden; background: #000; }
  :global(body) { margin: 0; padding: 0; height: 100%; overflow: hidden; background: #000; font-family: ui-sans-serif, system-ui, sans-serif; color: #eee; }

  canvas { display: block; position: fixed; inset: 0; width: 100vw; height: 100vh; }

  .hud {
    position: fixed; top: 1rem; right: 1rem; padding: 0.8rem 1rem;
    background: rgba(0,0,0,0.45); backdrop-filter: blur(8px);
    border: 1px solid rgba(255,255,255,0.08); border-radius: 8px;
    transition: opacity 0.4s ease; user-select: none;
  }
  .hud.hidden { opacity: 0; pointer-events: none; }
  .title { font-size: 0.9rem; font-weight: 500; margin-bottom: 0.4rem; }
  .source { display: flex; gap: 0.4rem; align-items: center; margin-bottom: 0.5rem; flex-wrap: nowrap; }
  .label-text { font-size: 0.75rem; color: #888; }
  /* Custom dropdown — replaces native <select> because WebView2's option-list
     rendering doesn't reliably respect color-scheme: dark. */
  .source-dd { position: relative; flex: 1; min-width: 180px; max-width: 240px; }
  .source-trigger {
    width: 100%;
    display: flex; justify-content: space-between; align-items: center; gap: 0.4rem;
    background: rgba(255,255,255,0.08); color: #eee; font: inherit; font-size: 0.8rem;
    border: 1px solid rgba(255,255,255,0.15); border-radius: 5px;
    padding: 0.25rem 0.5rem; cursor: pointer; text-align: left;
  }
  .source-trigger:hover { background: rgba(255,255,255,0.14); }
  .source-current { overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
  .chevron { font-size: 0.7rem; color: #aaa; transition: transform 0.15s ease; }
  .chevron.open { transform: rotate(180deg); }
  .source-list {
    position: absolute; top: calc(100% + 4px); left: 0; right: 0;
    background: rgba(18, 18, 22, 0.96); backdrop-filter: blur(10px);
    border: 1px solid rgba(255,255,255,0.12); border-radius: 6px;
    padding: 0.2rem; z-index: 10;
    max-height: 320px; overflow-y: auto;
    box-shadow: 0 8px 24px rgba(0,0,0,0.5);
  }
  .source-opt {
    display: block; width: 100%; text-align: left;
    background: transparent; color: #eee; font: inherit; font-size: 0.8rem;
    border: none; border-radius: 4px; padding: 0.35rem 0.55rem; cursor: pointer;
    overflow: hidden; text-overflow: ellipsis; white-space: nowrap;
  }
  .source-opt:hover { background: rgba(255,255,255,0.10); }
  .source-opt.active { background: rgba(120, 160, 220, 0.22); color: #fff; }
  .source-opt.idle { color: #888; }
  .refresh { padding: 0.25rem 0.55rem; font-size: 0.8rem; }
  .controls { display: flex; gap: 0.6rem; align-items: center; }
  button {
    padding: 0.35rem 0.8rem; font: inherit; font-size: 0.85rem;
    background: rgba(255,255,255,0.08); color: #eee;
    border: 1px solid rgba(255,255,255,0.15); border-radius: 5px; cursor: pointer;
  }
  button:hover { background: rgba(255,255,255,0.14); }
  .status { font-size: 0.8rem; color: #aaa; }
  .err { color: #f88; font-size: 0.8rem; margin: 0.4rem 0 0; }
  .hint { font-size: 0.7rem; color: #777; margin: 0.5rem 0 0; }
  kbd {
    background: rgba(255,255,255,0.1); padding: 0.05rem 0.3rem;
    border-radius: 3px; font-family: ui-monospace, monospace;
  }

  /* ─── Now Playing card ────────────────────────────────────────── */
  .np {
    position: fixed; bottom: 1rem; left: 1rem;
    display: flex; gap: 0.8rem; align-items: center;
    padding: 0.7rem 0.9rem;
    background: rgba(0,0,0,0.55); backdrop-filter: blur(10px);
    border: 1px solid rgba(255,255,255,0.08); border-radius: 10px;
    max-width: min(440px, 50vw);
    transition: opacity 0.4s ease;
    user-select: none;
  }
  .np.hidden { opacity: 0; pointer-events: none; }
  .np-art {
    width: 64px; height: 64px; border-radius: 6px; flex-shrink: 0;
    background: rgba(255,255,255,0.05); object-fit: cover;
  }
  .np-art-placeholder {
    display: flex; align-items: center; justify-content: center;
    color: rgba(255,255,255,0.4); font-size: 1.8rem;
  }
  .np-text { display: flex; flex-direction: column; gap: 0.2rem; min-width: 0; flex: 1; }
  .np-title {
    font-size: 0.95rem; font-weight: 500; color: #eee;
    overflow: hidden; text-overflow: ellipsis; white-space: nowrap;
  }
  .np-artist {
    font-size: 0.8rem; color: #aaa;
    overflow: hidden; text-overflow: ellipsis; white-space: nowrap;
  }
  .np-progress-row {
    display: flex; align-items: center; gap: 0.5rem; margin-top: 0.15rem;
  }
  .np-time {
    font-size: 0.7rem; color: #888; font-variant-numeric: tabular-nums;
    min-width: 2.5rem; text-align: right;
  }
  .np-time:first-child { text-align: right; }
  .np-progress {
    flex: 1; height: 3px; background: rgba(255,255,255,0.08); border-radius: 2px;
    overflow: hidden;
  }
  .np-progress-fill {
    height: 100%; background: linear-gradient(90deg, #888, #ccc);
    transition: width 0.1s linear;
  }
  .np-status { font-size: 0.7rem; color: #777; display: flex; gap: 0.4rem; align-items: center; margin-top: 0.1rem; }
  .np-source { color: #666; }
</style>
