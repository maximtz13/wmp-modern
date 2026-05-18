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

  // Visualizer preset picker.
  type PresetName = "spike" | "canyon" | "tunnel" | "wave" | "bars";
  const PRESETS: PresetName[] = ["spike", "canyon", "tunnel", "wave", "bars"];
  const PRESET_LABELS: Record<PresetName, string> = {
    spike: "Spike",
    canyon: "Canyon",
    tunnel: "Tunnel",
    wave: "Wave",
    bars: "Bars",
  };
  const LS_PRESET = "wmp-modern.preset";
  let preset = $state<PresetName>("spike");

  function setPreset(p: PresetName) {
    preset = p;
    try { localStorage.setItem(LS_PRESET, p); } catch {}
  }

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
  // Track-change auto-show: when the title/artist changes, pop the Now Playing card
  // up for NP_FLASH_MS even if the HUD is otherwise hidden.
  let lastTrackKey = "";
  let npShownUntil = 0;
  let npFlashActive = $state(false); // toggled in the render loop so reactivity fires
  const NP_FLASH_MS = 5000;

  onMount(async () => {
    try { initGL(canvasEl); } catch (e) { error = String(e); return; }
    unlisten = await listen<SpectrumFrame>("audio-spectrum", onFrame);

    // Restore last source choice (if any) from localStorage.
    const stored = localStorage.getItem(LS_KEY);
    selectedPid = stored !== null && stored !== "" ? Number(stored) : null;

    // Restore last preset choice.
    const storedPreset = localStorage.getItem(LS_PRESET) as PresetName | null;
    if (storedPreset && PRESETS.includes(storedPreset)) {
      preset = storedPreset;
    }

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
        const newKey = `${snap.current.title}\x1f${snap.current.artist}`;
        // Trigger flash only on real changes, and only after we've established a baseline
        // — otherwise the very first poll after launch would always flash.
        if (lastTrackKey !== "" && newKey !== lastTrackKey) {
          npShownUntil = performance.now() + NP_FLASH_MS;
        }
        lastTrackKey = newKey;
        nowPlaying = snap.current;
        npObservedAt = performance.now();
        displayPos = snap.current.position_ms;
      } else {
        nowPlaying = null;
        lastTrackKey = "";
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

  // Mousemove fires many times per second; throttle to avoid spamming Svelte
  // reactivity (which can cause perceptible stutter in the visualizer).
  let lastBumpAt = 0;
  function bumpHud() {
    const now = performance.now();
    if (now - lastBumpAt < 120 && showHud) {
      // Still active and timer already set — just reset the timeout, don't toggle state.
      if (hudTimer) clearTimeout(hudTimer);
      hudTimer = window.setTimeout(() => { showHud = false; }, 3000);
      return;
    }
    lastBumpAt = now;
    if (!showHud) showHud = true;
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

  // Canyon Chase preset: scrolling 3D wireframe terrain. Heightmap of past
  // spectrum frames, new row written at the far end (horizon) and shifted
  // toward the camera each tick — classic synthwave forward-flythrough.
  const CANYON_COLS = 96;
  const CANYON_ROWS = 72;          // more rows so the deeper canyon stays detailed
  const CANYON_WIDTH = 16.0;       // broader U
  const CANYON_DEPTH = 25.0;       // much longer into the distance
  const CANYON_HEIGHT = 0.95;
  const CANYON_WALL_HEIGHT = 5.5;
  const canyonHeightmap = new Float32Array(CANYON_COLS * CANYON_ROWS);
  const canyonBaseline = new Float32Array(CANYON_COLS);
  // Sharper U: exponent 5.0 → walls hug the outer ~15% with a nearly flat floor.
  for (let c = 0; c < CANYON_COLS; c++) {
    const u = c / (CANYON_COLS - 1);
    const dist = Math.abs(u - 0.5) * 2;
    canyonBaseline[c] = Math.pow(dist, 5.0) * CANYON_WALL_HEIGHT;
  }
  // Geometry: horizontal lines (cols-1 per row × rows) + vertical lines (rows-1 per col × cols).
  const CANYON_HORZ_SEG = (CANYON_COLS - 1) * CANYON_ROWS;
  const CANYON_VERT_SEG = (CANYON_ROWS - 1) * CANYON_COLS;
  const CANYON_TOTAL_SEG = CANYON_HORZ_SEG + CANYON_VERT_SEG;
  const canyonP = new Float32Array(CANYON_TOTAL_SEG * 2 * 3);
  const canyonC = new Float32Array(CANYON_TOTAL_SEG * 2 * 3);
  let canyonScrollAccumulator = 0;

  // Wave preset: compact object, lines stacked in Y, orbited by the camera.
  const WAVE_N = 256;
  const WAVE_CLONES = 30;              // more clones — stack extends further down
  const WAVE_WIDTH = 3.6;              // narrower
  const WAVE_HEIGHT = 0.28;
  const WAVE_LINE_SPACING_Y = 0.075;
  const WAVE_TOTAL_VERTS = WAVE_CLONES * WAVE_N;
  const waveAllP = new Float32Array(WAVE_TOTAL_VERTS * 3);
  const waveAllC = new Float32Array(WAVE_TOTAL_VERTS * 3);
  const waveLine = new Float32Array(WAVE_N);
  const waveLineTmp = new Float32Array(WAVE_N);

  // Tunnel preset: same scrolling-heightmap idea as the canyon, but the spectrum
  // wraps angularly around each ring instead of laying flat across an X-axis.
  // - "Rows" are rings, spaced along Z.
  // - "Columns" are vertices around each ring's circumference (an angle).
  // - The heightmap cell at (ring, angle) is a radial displacement: bigger = the
  //   ring bulges outward at that angle.
  // Each frame the heightmap shifts one row toward the camera and we write the
  // current spectrum into the far ring, exactly like the canyon. Result: spectrum
  // history flying toward the viewer in tunnel form.
  const TUNNEL_RINGS = 90;        // halved along with depth so accumulated brightness stays sane
  const TUNNEL_RING_N = 64;
  const TUNNEL_DEPTH = 8.0;       // half again — rings spawn even closer
  const TUNNEL_BASE_R = 1.10;
  const TUNNEL_AMP = 0.18;        // half — gentler beat indentation
  const TUNNEL_MIN_R = 0.30;      // clamp to keep the core visible even on strong beats
  const TUNNEL_TOTAL_VERTS = TUNNEL_RINGS * TUNNEL_RING_N;
  const tunnelHeightmap = new Float32Array(TUNNEL_RINGS * TUNNEL_RING_N);
  const tunnelRowBuf = new Float32Array(TUNNEL_RING_N);
  const tunnelRowTmp = new Float32Array(TUNNEL_RING_N);
  // One big buffer for ALL rings — uploaded once per frame, drawn as 28 separate
  // LINE_LOOPs (offsets into the same buffer) + a single POINTS draw for the glow.
  const tunnelAllP = new Float32Array(TUNNEL_TOTAL_VERTS * 3);
  const tunnelAllC = new Float32Array(TUNNEL_TOTAL_VERTS * 3);
  let tunnelScrollAccumulator = 0;

  // Bars preset: 64 radial 3D bars, mirrored above/below the plane.
  const BARS_N = 64;
  // Each bar = 2 line segments (positive Y half + negative Y half) = 4 vertices total.
  const barsP = new Float32Array(BARS_N * 4 * 3);
  const barsC = new Float32Array(BARS_N * 4 * 3);
  // Halo ring (the circle the bars stand on) — line loop of HALO_N verts.
  const HALO_N = 96;
  const haloP = new Float32Array(HALO_N * 3);
  const haloC = new Float32Array(HALO_N * 3);

  let rayPVbo: WebGLBuffer, rayCVbo: WebGLBuffer;
  let tipPVbo: WebGLBuffer, tipCVbo: WebGLBuffer;
  let starPVbo: WebGLBuffer, starCVbo: WebGLBuffer;
  let canyonPVbo: WebGLBuffer, canyonCVbo: WebGLBuffer;
  let tunnelRingPVbo: WebGLBuffer, tunnelRingCVbo: WebGLBuffer;
  let wavePVbo: WebGLBuffer, waveCVbo: WebGLBuffer;
  let barsPVbo: WebGLBuffer, barsCVbo: WebGLBuffer;
  let haloPVbo: WebGLBuffer, haloCVbo: WebGLBuffer;
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
    canyonPVbo = gl.createBuffer()!;
    canyonCVbo = gl.createBuffer()!;
    tunnelRingPVbo   = gl.createBuffer()!;
    tunnelRingCVbo   = gl.createBuffer()!;
    wavePVbo = gl.createBuffer()!;
    waveCVbo = gl.createBuffer()!;

    barsPVbo   = gl.createBuffer()!;
    barsCVbo   = gl.createBuffer()!;
    haloPVbo   = gl.createBuffer()!;
    haloCVbo   = gl.createBuffer()!;

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

    const innerR = 0.15 + audio.bass * 0.02;
    const baseLen = 1.05 + kickEnv * 0.06;    // very gentle bass breathing
    const lenScale = 0.85;                     // shorter rays overall
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

  // Map a canyon column index to a spectrum bin index using log-spaced bins, so the
  // perceptual frequency distribution looks balanced across the terrain instead of
  // all the energy bunching up on one side.
  function canyonColToBin(col: number): number {
    const u = col / (CANYON_COLS - 1);
    const lo = 1, hi = 200;
    return Math.min(N - 1, Math.max(1, Math.round(Math.pow(hi / lo, u) * lo)));
  }

  // Same log-spaced mapping for the tunnel, but wraps angularly: vertex 0 starts
  // at the same bin as vertex TUNNEL_RING_N (the loop closes) — so we go halfway
  // around the spectrum and mirror, otherwise the seam would be visible.
  function tunnelVertToBin(v: number): number {
    // Map [0, 1) angular position to [0, 1] frequency by folding at 0.5.
    const u = v / TUNNEL_RING_N;
    const folded = u < 0.5 ? u * 2 : (1 - u) * 2;  // 0..1..0 (continuous at the seam)
    const lo = 1, hi = 180;
    return Math.min(N - 1, Math.max(1, Math.round(Math.pow(hi / lo, folded) * lo)));
  }

  function advanceTunnelHeightmap(dt: number) {
    // Doubled scroll rate.
    tunnelScrollAccumulator += dt * 28 + kickEnv * 12 * dt;
    while (tunnelScrollAccumulator >= 1) {
      tunnelScrollAccumulator -= 1;
      // Shift rings FORWARD (toward the camera): ring[r] = ring[r+1].
      for (let r = 0; r < TUNNEL_RINGS - 1; r++) {
        const dst = r * TUNNEL_RING_N;
        const src = (r + 1) * TUNNEL_RING_N;
        for (let v = 0; v < TUNNEL_RING_N; v++) {
          tunnelHeightmap[dst + v] = tunnelHeightmap[src + v];
        }
      }
      // Write new spectrum into the FAR ring. Two passes of 1-2-1 blur, but
      // circular at the seam (wrap around) so the ring is smooth all the way
      // around its circumference.
      // Beat amplifier — strong, then we lightly diffuse longitudinally so the
      // peaks spread to neighbors without flattening completely.
      const beatBoost = 2.0 + kickEnv * 3.5;
      for (let v = 0; v < TUNNEL_RING_N; v++) {
        const b = bins[tunnelVertToBin(v)];
        tunnelRowBuf[v] = Math.sqrt(Math.max(0, b)) * beatBoost;
      }
      // Only ONE circular smoothing pass so distinct peaks remain visible
      // around the ring — multiple small prominent mounds rather than one big blob.
      for (let v = 0; v < TUNNEL_RING_N; v++) {
        const vm = (v - 1 + TUNNEL_RING_N) % TUNNEL_RING_N;
        const vp = (v + 1) % TUNNEL_RING_N;
        tunnelRowTmp[v] = (tunnelRowBuf[vm] + 2 * tunnelRowBuf[v] + tunnelRowBuf[vp]) * 0.25;
      }
      for (let v = 0; v < TUNNEL_RING_N; v++) tunnelRowBuf[v] = tunnelRowTmp[v];
      // Cross-fade with the previous-far-ring for vertical continuity.
      const farRow = (TUNNEL_RINGS - 1) * TUNNEL_RING_N;
      for (let v = 0; v < TUNNEL_RING_N; v++) {
        const prev = tunnelHeightmap[farRow + v];
        tunnelHeightmap[farRow + v] = prev * 0.30 + tunnelRowBuf[v] * 0.70;
      }

      // Longitudinal blur — VERY light, just enough that each beat's bump
      // visibly spans 3-5 adjacent rings while still preserving most of the
      // peak amplitude per ring (was 0.25/0.50/0.25; now 0.10/0.80/0.10).
      for (let v = 0; v < TUNNEL_RING_N; v++) {
        let prev = tunnelHeightmap[v];
        for (let r = 1; r < TUNNEL_RINGS - 1; r++) {
          const idx = r * TUNNEL_RING_N + v;
          const curr = tunnelHeightmap[idx];
          const next = tunnelHeightmap[idx + TUNNEL_RING_N];
          tunnelHeightmap[idx] = prev * 0.10 + curr * 0.80 + next * 0.10;
          prev = curr;
        }
      }
    }
  }

  // ─── Wave preset: 2D spectrum line ──────────────────────────────
  // Single horizontal line. Y at each X is driven by a spectrum bin (log-spaced).
  // The whole line scales vertically with bass kicks → it "rises" on every beat.
  function waveColToBin(c: number): number {
    const u = c / (WAVE_N - 1);
    const lo = 1, hi = 200;
    return Math.min(N - 1, Math.max(1, Math.round(Math.pow(hi / lo, u) * lo)));
  }

  function buildWave(t: number) {
    const beatBoost = 1.0 + kickEnv * 2.5;
    const hueDrift = t * 0.04;

    // Compute the one current spectrum line (smoothed).
    for (let i = 0; i < WAVE_N; i++) {
      const b = bins[waveColToBin(i)];
      waveLine[i] = Math.sqrt(Math.max(0, b)) * 1.6 * beatBoost;
    }
    for (let pass = 0; pass < 2; pass++) {
      for (let i = 0; i < WAVE_N; i++) {
        const im = Math.max(0, i - 1);
        const ip = Math.min(WAVE_N - 1, i + 1);
        waveLineTmp[i] = (waveLine[im] + 2 * waveLine[i] + waveLine[ip]) * 0.25;
      }
      for (let i = 0; i < WAVE_N; i++) waveLine[i] = waveLineTmp[i];
    }

    // Clone the current line into WAVE_CLONES stacked copies, centered on Y=0,
    // all at Z=0 (no depth stagger).
    const halfSpanY = (WAVE_CLONES - 1) * WAVE_LINE_SPACING_Y * 0.5;
    for (let r = 0; r < WAVE_CLONES; r++) {
      const baseY = halfSpanY - r * WAVE_LINE_SPACING_Y;
      const baseZ = 0;
      const ageT = r / (WAVE_CLONES - 1);
      const fade = 1 - ageT;
      const fadeQ = fade * fade * 0.85 + 0.15; // small floor so all clones remain dimly visible

      // Each line is one solid color — slowly drifts over time, subtle offset
      // per clone so adjacent lines are slightly different shades.
      const cloneU = r / (WAVE_CLONES - 1);
      const lineHue = hueDrift + cloneU * 0.10;
      for (let i = 0; i < WAVE_N; i++) {
        const u = i / (WAVE_N - 1);
        const x = (u - 0.5) * WAVE_WIDTH;
        const h = waveLine[i] * WAVE_HEIGHT;
        const idx = (r * WAVE_N + i) * 3;
        waveAllP[idx + 0] = x;
        waveAllP[idx + 1] = baseY + h;
        waveAllP[idx + 2] = baseZ;
        pal(lineHue, waveAllC, idx);
        const peakBoost = 0.55 + h * 0.85;
        const brightness = fadeQ * peakBoost;
        waveAllC[idx + 0] *= brightness;
        waveAllC[idx + 1] *= brightness;
        waveAllC[idx + 2] *= brightness;
      }
    }
  }

  function buildTunnel(t: number, dt: number) {
    advanceTunnelHeightmap(dt);
    const hueBase = t * 0.04;
    const zStep = TUNNEL_DEPTH / (TUNNEL_RINGS - 1);
    const subRow = tunnelScrollAccumulator;
    const zOffset = subRow * zStep; // positive: rings slide toward camera

    for (let r = 0; r < TUNNEL_RINGS; r++) {
      const z = -r * zStep + zOffset;
      // Reverse depth fade: rings closest to camera (low r) are dim; rings farthest
      // (high r, the spawn point) are bright. So rings DRAIN brightness as they
      // scroll toward you. Close still visible (>30%), not invisible.
      const proximityT = r / (TUNNEL_RINGS - 1);
      const depthFade = 0.35 + proximityT * 0.65;
      const ringHueShift = r * 0.025;
      for (let v = 0; v < TUNNEL_RING_N; v++) {
        const u = v / TUNNEL_RING_N;
        const angle = u * Math.PI * 2;
        const height = tunnelHeightmap[r * TUNNEL_RING_N + v];
        // Beats push INWARD (indent the surface). Clamp so radius can't collapse to 0.
        const radius = Math.max(TUNNEL_MIN_R, TUNNEL_BASE_R - height * TUNNEL_AMP);
        const idx = (r * TUNNEL_RING_N + v) * 3;
        tunnelAllP[idx + 0] = Math.cos(angle) * radius;
        tunnelAllP[idx + 1] = Math.sin(angle) * radius;
        tunnelAllP[idx + 2] = z;

        // Rainbow around the ring + slow time drift + ring-depth hue shift.
        // Uniform brightness across the ring — no peak-driven boost.
        const brightness = depthFade * 0.50;
        pal(u + hueBase + ringHueShift, tunnelAllC, idx);
        tunnelAllC[idx + 0] *= brightness;
        tunnelAllC[idx + 1] *= brightness;
        tunnelAllC[idx + 2] *= brightness;
      }
    }
  }

  function advanceCanyonHeightmap(dt: number) {
    // Faster scroll for a snappier flythrough sensation.
    canyonScrollAccumulator += dt * 28 + kickEnv * 6 * dt;
    while (canyonScrollAccumulator >= 1) {
      canyonScrollAccumulator -= 1;
      for (let r = 0; r < CANYON_ROWS - 1; r++) {
        const dst = r * CANYON_COLS;
        const src = (r + 1) * CANYON_COLS;
        for (let c = 0; c < CANYON_COLS; c++) {
          canyonHeightmap[dst + c] = canyonHeightmap[src + c];
        }
      }
      // Write new spectrum row. Boost the input gain and use only 2 smoothing passes
      // so peaks stay tall and dramatic instead of being smoothed flat.
      const farRow = (CANYON_ROWS - 1) * CANYON_COLS;
      let buf: Float32Array = new Float32Array(CANYON_COLS);
      let tmp: Float32Array = new Float32Array(CANYON_COLS);
      for (let c = 0; c < CANYON_COLS; c++) {
        const b = bins[canyonColToBin(c)];
        buf[c] = Math.sqrt(Math.max(0, b)) * 2.8; // boosted from 1.8
      }
      for (let pass = 0; pass < 2; pass++) {
        for (let c = 0; c < CANYON_COLS; c++) {
          const cm = Math.max(0, c - 1);
          const cp = Math.min(CANYON_COLS - 1, c + 1);
          tmp[c] = (buf[cm] + 2 * buf[c] + buf[cp]) * 0.25;
        }
        [buf, tmp] = [tmp, buf];
      }
      // Cross-fade lightly with previous-far-row for vertical continuity.
      for (let c = 0; c < CANYON_COLS; c++) {
        const prev = canyonHeightmap[farRow + c];
        canyonHeightmap[farRow + c] = prev * 0.30 + buf[c] * 0.70;
      }
    }
  }

  // Helper: write a vertex (position + color) into the canyon buffers at given segment offset.
  function setCanyonVertex(segIdx: number, vertInSeg: 0 | 1, x: number, y: number, z: number, hueT: number, brightness: number) {
    const i = (segIdx * 2 + vertInSeg) * 3;
    canyonP[i + 0] = x;
    canyonP[i + 1] = y;
    canyonP[i + 2] = z;
    pal(hueT, canyonC, i);
    canyonC[i + 0] *= brightness;
    canyonC[i + 1] *= brightness;
    canyonC[i + 2] *= brightness;
  }

  function buildCanyon(t: number, dt: number) {
    advanceCanyonHeightmap(dt);
    const hueBase = t * 0.04;

    const xStep = CANYON_WIDTH / (CANYON_COLS - 1);
    const zStep = CANYON_DEPTH / (CANYON_ROWS - 1);

    // Sub-row interpolation: as the accumulator grows from 0→1, every row
    // smoothly slides toward the camera (+Z direction). When it wraps to 0,
    // the discrete shift has already moved every row's data one slot forward,
    // so the apparent position is continuous.
    const subRow = canyonScrollAccumulator;
    const zOffset = subRow * zStep;

    let seg = 0;

    // Horizontal lines (col → col+1 within each row).
    for (let r = 0; r < CANYON_ROWS; r++) {
      const z = -r * zStep + zOffset;
      const depthT = 1 - r / (CANYON_ROWS - 1);
      // Quadratic depth falloff so far rows fade nearly to black at the horizon.
      const depthFade = 0.02 + depthT * depthT * 0.98;
      for (let c = 0; c < CANYON_COLS - 1; c++) {
        const h0 = canyonHeightmap[r * CANYON_COLS + c] * CANYON_HEIGHT + canyonBaseline[c];
        const h1 = canyonHeightmap[r * CANYON_COLS + (c + 1)] * CANYON_HEIGHT + canyonBaseline[c + 1];
        const x0 = (c / (CANYON_COLS - 1) - 0.5) * CANYON_WIDTH;
        const x1 = ((c + 1) / (CANYON_COLS - 1) - 0.5) * CANYON_WIDTH;
        const b0 = depthFade * (0.5 + h0 * 1.0);
        const b1 = depthFade * (0.5 + h1 * 1.0);
        setCanyonVertex(seg, 0, x0, h0, z, hueBase + h0 * 0.2, b0);
        setCanyonVertex(seg, 1, x1, h1, z, hueBase + h1 * 0.2, b1);
        seg++;
      }
    }

    // Vertical lines (row → row+1 within each col).
    for (let c = 0; c < CANYON_COLS; c++) {
      const x = (c / (CANYON_COLS - 1) - 0.5) * CANYON_WIDTH;
      for (let r = 0; r < CANYON_ROWS - 1; r++) {
        const z0 = -r * zStep + zOffset;
        const z1 = -(r + 1) * zStep + zOffset;
        const depthT0 = 1 - r / (CANYON_ROWS - 1);
        const depthT1 = 1 - (r + 1) / (CANYON_ROWS - 1);
        const fade0 = 0.02 + depthT0 * depthT0 * 0.98;
        const fade1 = 0.02 + depthT1 * depthT1 * 0.98;
        const h0 = canyonHeightmap[r * CANYON_COLS + c] * CANYON_HEIGHT + canyonBaseline[c];
        const h1 = canyonHeightmap[(r + 1) * CANYON_COLS + c] * CANYON_HEIGHT + canyonBaseline[c];
        const b0 = fade0 * (0.5 + h0 * 1.0);
        const b1 = fade1 * (0.5 + h1 * 1.0);
        setCanyonVertex(seg, 0, x, h0, z0, hueBase + h0 * 0.2, b0);
        setCanyonVertex(seg, 1, x, h1, z1, hueBase + h1 * 0.2, b1);
        seg++;
      }
    }
  }

  // ─── Tunnel preset: 3D forward-flight ─────────────────────────────
  function advanceTunnel(dt: number) {
    const baseSpeed = 1.8;
    const speed = baseSpeed + kickEnv * 5.0;
    for (let i = 0; i < TUNNEL_RING_COUNT; i++) {
      tunnelRingZ[i] += dt * speed;
      if (tunnelRingZ[i] > 1.5) {
        // Recycle to the far end.
        tunnelRingZ[i] -= TUNNEL_RING_COUNT * TUNNEL_RING_SPACING;
        tunnelRingPhase[i] = Math.random() * Math.PI * 2;
        tunnelRingHue[i] = Math.random();
      }
    }
  }

  function buildTunnelRing(idx: number, t: number) {
    const z = tunnelRingZ[idx];
    const phase = tunnelRingPhase[idx];
    const hueBase = tunnelRingHue[idx];

    // Slightly smaller base radius so rings fit comfortably in the FOV.
    const baseR = 1.05;
    const waveScale = 0.10;
    const hueDrift = t * 0.04;

    // Depth fade — higher floor so even far rings are visible.
    const camZ = 1.5;
    const tunnelEnd = -TUNNEL_RING_COUNT * TUNNEL_RING_SPACING;
    const depthT = Math.max(0, Math.min(1, (z - tunnelEnd) / (camZ - tunnelEnd)));
    const depthFade = 0.30 + depthT * 0.70;

    for (let i = 0; i < TUNNEL_RING_N; i++) {
      const u = i / TUNNEL_RING_N;
      const angle = u * Math.PI * 2 + phase + t * 0.10;
      const w = wave.length > 0 ? (wave[Math.floor(u * wave.length)] || 0) : 0;
      const wobble = Math.sin(angle * 6 + t * 0.5) * 0.045;
      const r = baseR + w * waveScale + wobble + audio.bass * 0.05;
      tunnelRingP[i * 3 + 0] = Math.cos(angle) * r;
      tunnelRingP[i * 3 + 1] = Math.sin(angle) * r;
      tunnelRingP[i * 3 + 2] = z;
      pal(u * 0.10 + hueBase + hueDrift, tunnelRingC, i * 3);
      tunnelRingC[i * 3 + 0] *= depthFade;
      tunnelRingC[i * 3 + 1] *= depthFade;
      tunnelRingC[i * 3 + 2] *= depthFade;
    }
  }

  function buildBars(t: number) {
    const innerR = 0.85 + kickEnv * 0.10;
    const heightScale = 1.3;
    const spinT = t * 0.10;
    const hueDrift = t * 0.012;

    for (let i = 0; i < BARS_N; i++) {
      const u = i / BARS_N;
      const angle = u * Math.PI * 2 + spinT;
      // Sample bins to fill BARS_N entries: take every 4th bin.
      const binIdx = (i * 4) % N;
      const h = bins[binIdx] * heightScale;

      const cx = Math.cos(angle), cz = Math.sin(angle);
      const bx = cx * innerR, bz = cz * innerR;

      const base = i * 12; // 4 verts * 3 components

      // Top-half bar: from (bx, 0, bz) to (bx, +h, bz)
      barsP[base + 0] = bx;  barsP[base + 1] = 0;  barsP[base + 2] = bz;
      barsP[base + 3] = bx;  barsP[base + 4] = h;  barsP[base + 5] = bz;
      // Bottom-half bar: from (bx, 0, bz) to (bx, -h, bz)
      barsP[base + 6] = bx;  barsP[base + 7] = 0;  barsP[base + 8] = bz;
      barsP[base + 9] = bx;  barsP[base + 10] = -h; barsP[base + 11] = bz;

      // Color: bright at the tip, dim at the base. Hue varies with angle.
      const cBase = base; // 4 verts colored independently
      pal(u + hueDrift, barsC, cBase + 0);     // base of top
      pal(u + hueDrift, barsC, cBase + 3);     // tip of top (full)
      pal(u + hueDrift, barsC, cBase + 6);     // base of bottom
      pal(u + hueDrift, barsC, cBase + 9);     // tip of bottom (full)
      // Dim the base ends (vert 0 and vert 2)
      barsC[cBase + 0] *= 0.20; barsC[cBase + 1] *= 0.20; barsC[cBase + 2] *= 0.20;
      barsC[cBase + 6] *= 0.20; barsC[cBase + 7] *= 0.20; barsC[cBase + 8] *= 0.20;
    }

    // Halo ring (line loop around the base of all bars)
    for (let i = 0; i < HALO_N; i++) {
      const u = i / HALO_N;
      const a = u * Math.PI * 2 + spinT * 0.5;
      haloP[i * 3 + 0] = Math.cos(a) * innerR;
      haloP[i * 3 + 1] = 0;
      haloP[i * 3 + 2] = Math.sin(a) * innerR;
      pal(u + hueDrift + 0.5, haloC, i * 3);
      // Dim the halo so it's a subtle frame, not a focal point.
      haloC[i * 3 + 0] *= 0.45;
      haloC[i * 3 + 1] *= 0.45;
      haloC[i * 3 + 2] *= 0.45;
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

    // Canyon and tunnel have fundamentally different camera setups than the
    // rotating presets, so handle them up front.
    if (preset === "canyon") {
      // Faster lateral sweep (~52 sec period). Camera drifts side-to-side, looking
      // diagonally so the canyon angles across the screen.
      const period = t * 0.12;
      const camX = Math.sin(period) * 4.5;
      const camY = 4.5;
      const camZ = 2.0;
      const lookX = -Math.sin(period) * 2.0;
      mat4LookAt(view, camX, camY, camZ, lookX, -0.30, -12.0);
      mat4Identity(model);
      mat4Mul(mvp, proj, view);
      return camY;
    }
    if (preset === "wave") {
      // Subtle orbit around the wave (~±30°) — since the stack is now flat at z=0,
      // a wide orbit would show it edge-on and lose all the detail.
      const swing = Math.sin(t * 0.10) * (Math.PI / 6);
      const tilt = 0.18 + Math.sin(t * 0.05) * 0.06;
      const dist = 3.6;
      const ex = Math.sin(swing) * dist * Math.cos(tilt);
      const ez = Math.cos(swing) * dist * Math.cos(tilt);
      const ey = Math.sin(tilt) * dist;
      mat4LookAt(view, ex, ey, ez, 0, 0, 0);
      mat4Identity(model);
      mat4Mul(mvp, proj, view);
      return ey;
    }
    if (preset === "tunnel") {
      // Camera positioned inside the tunnel looking down -Z.
      mat4LookAt(view, 0, 0, -3.0, 0, 0, -20);
      mat4Identity(model);
      mat4Mul(mvp, proj, view);
      return 0;
    }

    // Orbiting presets (spike, bars).
    let dist: number, tiltBase: number, orbitRate: number, tiltWobbleRate: number, tiltWobbleAmp: number;
    switch (preset) {
      case "bars":
        dist = 3.0 - kickEnv * 0.4;
        tiltBase = 0.45;
        orbitRate = 0.07;
        tiltWobbleRate = 0.05;
        tiltWobbleAmp = 0.05;
        break;
      case "spike":
      default:
        dist = 2.5 - kickEnv * 0.20;
        tiltBase = 0.30;
        orbitRate = 0.06;
        tiltWobbleRate = 0.05;
        tiltWobbleAmp = 0.08;
        break;
    }

    const orbit = t * orbitRate;
    const tilt = tiltBase + Math.sin(t * tiltWobbleRate) * tiltWobbleAmp;
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

    // Now Playing flash visibility — toggle only when crossing the threshold so we
    // don't trigger Svelte reactivity 60 times a second.
    const flashActive = now < npShownUntil;
    if (flashActive !== npFlashActive) npFlashActive = flashActive;

    // Smooth Now Playing position interpolation between polls.
    if (nowPlaying && nowPlaying.status === "playing") {
      const elapsed = now - npObservedAt;
      const extrapolated = nowPlaying.position_ms + elapsed * nowPlaying.playback_rate;
      displayPos = Math.min(nowPlaying.duration_ms || extrapolated, extrapolated);
    }

    // Trail fade — color drifts slowly through hues so the background isn't static.
    gl.bindVertexArray(quadVao);
    gl.useProgram(quadProgram);
    gl.enable(gl.BLEND);
    gl.blendFunc(gl.SRC_ALPHA, gl.ONE_MINUS_SRC_ALPHA);
    const fadeA = 0.10 + audio.level * 0.05;
    const bgPhase = t * 0.05;
    const bgR = 0.012 + 0.014 * (0.5 + 0.5 * Math.cos(bgPhase));
    const bgG = 0.012 + 0.014 * (0.5 + 0.5 * Math.cos(bgPhase + 2.094));
    const bgB = 0.025 + 0.014 * (0.5 + 0.5 * Math.cos(bgPhase + 4.188));
    gl.uniform4f(uQuadColor!, bgR, bgG, bgB, fadeA);
    gl.drawArrays(gl.TRIANGLES, 0, 3);

    // Geometry.
    const camY = computeMVP(t);
    buildStars(dt, t);

    gl.bindVertexArray(lineVao);
    gl.useProgram(lineProgram);
    gl.uniformMatrix4fv(uMVP!, false, mvp);
    gl.blendFunc(gl.SRC_ALPHA, gl.ONE);

    // Background stars are always drawn — they give depth context to every preset.
    gl.uniform1f(uPointSize!, 1.8);
    gl.uniform1f(uIntensity!, 0.7);
    bindLineRing(starPVbo, starCVbo, starP, starC);
    gl.drawArrays(gl.POINTS, 0, K_STARS);

    // Per-preset geometry + draws.
    if (preset === "spike") {
      buildSphere(t, camY);
      gl.uniform1f(uIntensity!, 0.85 + audio.treble * 0.5 + kickEnv * 0.4);
      bindLineRing(rayPVbo, rayCVbo, rayP, rayC);
      gl.drawArrays(gl.LINES, 0, M_RAYS * 2);
      gl.uniform1f(uPointSize!, 2.5 + kickEnv * 2.0);
      gl.uniform1f(uIntensity!, 1.1 + kickEnv * 0.6);
      bindLineRing(tipPVbo, tipCVbo, tipP, tipC);
      gl.drawArrays(gl.POINTS, 0, M_RAYS);
    } else if (preset === "canyon") {
      buildCanyon(t, dt);
      gl.uniform1f(uPointSize!, 1.0);
      gl.uniform1f(uIntensity!, 0.95 + audio.level * 0.30);
      bindLineRing(canyonPVbo, canyonCVbo, canyonP, canyonC);
      gl.drawArrays(gl.LINES, 0, CANYON_TOTAL_SEG * 2);
    } else if (preset === "wave") {
      buildWave(t);
      bindLineRing(wavePVbo, waveCVbo, waveAllP, waveAllC);
      gl.uniform1f(uIntensity!, 1.5 + audio.level * 0.3);
      for (let r = 0; r < WAVE_CLONES; r++) {
        gl.drawArrays(gl.LINE_STRIP, r * WAVE_N, WAVE_N);
      }
    } else if (preset === "tunnel") {
      // Heightmap-based tunnel (mirrors the canyon's mechanism). All ring geometry
      // is computed CPU-side into one big buffer, uploaded once, then drawn as
      // TUNNEL_RINGS separate LINE_LOOPs plus one POINTS draw for the full glow.
      buildTunnel(t, dt);
      bindLineRing(tunnelRingPVbo, tunnelRingCVbo, tunnelAllP, tunnelAllC);
      // Rings, drawn far-to-near for correct additive layering.
      gl.uniform1f(uIntensity!, 1.5 + audio.level * 0.3);
      for (let r = TUNNEL_RINGS - 1; r >= 0; r--) {
        gl.drawArrays(gl.LINE_LOOP, r * TUNNEL_RING_N, TUNNEL_RING_N);
      }
      // Glow points across every ring, one draw call.
      gl.uniform1f(uPointSize!, 2.5 + kickEnv * 1.5);
      gl.uniform1f(uIntensity!, 1.8 + kickEnv * 0.4);
      gl.drawArrays(gl.POINTS, 0, TUNNEL_TOTAL_VERTS);
    } else if (preset === "bars") {
      buildBars(t);
      // Halo (subtle base ring).
      gl.uniform1f(uIntensity!, 0.45 + audio.level * 0.25);
      bindLineRing(haloPVbo, haloCVbo, haloP, haloC);
      gl.drawArrays(gl.LINE_LOOP, 0, HALO_N);
      // Bars themselves.
      gl.uniform1f(uIntensity!, 0.9 + audio.mid * 0.5 + kickEnv * 0.4);
      bindLineRing(barsPVbo, barsCVbo, barsP, barsC);
      gl.drawArrays(gl.LINES, 0, BARS_N * 4);
    }
  }
</script>

<canvas bind:this={canvasEl}></canvas>

{#if nowPlaying && (nowPlaying.title || nowPlaying.artist)}
  <div class="np" class:hidden={!showHud && !npFlashActive}>
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

  <div class="preset-row">
    <span class="label-text">Preset</span>
    <div class="preset-group">
      {#each PRESETS as p}
        <button
          class="preset-btn"
          class:active={preset === p}
          onclick={() => setPreset(p)}>
          {PRESET_LABELS[p]}
        </button>
      {/each}
    </div>
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

  canvas {
    display: block; position: fixed; inset: 0; width: 100vw; height: 100vh;
    /* Force the OS default cursor — works around an occasional WebView2 quirk where
       the cursor sprite lingers on the canvas after a fast mouse-leave. */
    cursor: default;
  }

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

  .preset-row { display: flex; gap: 0.4rem; align-items: center; margin-bottom: 0.5rem; }
  .preset-group { display: flex; gap: 0.2rem; flex: 1; }
  .preset-btn {
    flex: 1;
    background: rgba(255,255,255,0.06); color: #ccc; font: inherit; font-size: 0.78rem;
    border: 1px solid rgba(255,255,255,0.10); border-radius: 4px;
    padding: 0.25rem 0.3rem; cursor: pointer;
  }
  .preset-btn:hover { background: rgba(255,255,255,0.12); color: #fff; }
  .preset-btn.active {
    background: rgba(120, 160, 220, 0.22); color: #fff;
    border-color: rgba(120, 160, 220, 0.45);
  }
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
