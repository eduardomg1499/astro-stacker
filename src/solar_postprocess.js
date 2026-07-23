const LINEAR_POINTS = Object.freeze([[0, 0], [1, 1]]);

export const SOLAR_CURVE_PRESETS = Object.freeze({
  neutral: {
    label: "Neutral",
    enabled: false,
    invert: false,
    colorize: false,
    curvePoints: LINEAR_POINTS,
    shadowColor: "#0f0000",
    midtoneColor: "#b83300",
    highlightColor: "#fff05a",
    colorStrength: 0.9,
    highlightProtect: 0.65,
    filamentAmount: 0,
    filamentRadius: 1.15,
    noiseGuard: 0.65,
  },
  "ha-gold": {
    label: "H-alpha dorado",
    enabled: true,
    invert: false,
    colorize: true,
    curvePoints: [[0, 0], [0.12, 0.055], [0.38, 0.27], [0.72, 0.78], [1, 1]],
    shadowColor: "#170000",
    midtoneColor: "#c43e00",
    highlightColor: "#fff36a",
    colorStrength: 0.92,
    highlightProtect: 0.68,
    filamentAmount: 0.34,
    filamentRadius: 1.15,
    noiseGuard: 0.68,
  },
  "ha-inverted": {
    label: "H-alpha invertido",
    enabled: true,
    invert: true,
    colorize: true,
    curvePoints: [[0, 0], [0.16, 0.1], [0.48, 0.42], [0.8, 0.88], [1, 1]],
    shadowColor: "#170100",
    midtoneColor: "#d35408",
    highlightColor: "#fff0a6",
    colorStrength: 0.88,
    highlightProtect: 0.76,
    filamentAmount: 0.38,
    filamentRadius: 1.05,
    noiseGuard: 0.72,
  },
  prominence: {
    label: "Prominencias",
    enabled: true,
    invert: false,
    colorize: true,
    curvePoints: [[0, 0], [0.035, 0.11], [0.14, 0.29], [0.55, 0.68], [1, 1]],
    shadowColor: "#090000",
    midtoneColor: "#9e1600",
    highlightColor: "#ffd35a",
    colorStrength: 0.95,
    highlightProtect: 0.82,
    filamentAmount: 0.18,
    filamentRadius: 1.3,
    noiseGuard: 0.82,
  },
  filaments: {
    label: "Filamentos mono",
    enabled: true,
    invert: false,
    colorize: false,
    curvePoints: [[0, 0], [0.17, 0.1], [0.42, 0.34], [0.7, 0.78], [1, 1]],
    shadowColor: "#000000",
    midtoneColor: "#808080",
    highlightColor: "#ffffff",
    colorStrength: 0,
    highlightProtect: 0.8,
    filamentAmount: 0.62,
    filamentRadius: 0.95,
    noiseGuard: 0.78,
  },
});

export function normalizeSolarCurvePoints(points = LINEAR_POINTS) {
  const clean = (Array.isArray(points) ? points : [])
    .map((point) => [Number(point?.[0]), Number(point?.[1])])
    .filter(([x, y]) => Number.isFinite(x) && Number.isFinite(y))
    .map(([x, y]) => [Math.max(0, Math.min(1, x)), Math.max(0, Math.min(1, y))])
    .sort((left, right) => left[0] - right[0])
    .filter((point, index, all) => index === 0 || Math.abs(point[0] - all[index - 1][0]) >= 0.0001);
  if (!clean.length || clean[0][0] > 0.0001) clean.unshift([0, 0]);
  if (clean.at(-1)[0] < 0.9999) clean.push([1, 1]);
  return clean.length >= 2 ? clean : LINEAR_POINTS.map((point) => [...point]);
}

export function evaluateSolarCurve(points, value) {
  const normalized = normalizeSolarCurvePoints(points);
  const x = Math.max(0, Math.min(1, Number(value) || 0));
  const intervals = normalized.slice(1).map((point, index) => (
    Math.max(0.000001, point[0] - normalized[index][0])
  ));
  const secants = intervals.map((interval, index) => (
    (normalized[index + 1][1] - normalized[index][1]) / interval
  ));
  const tangents = normalized.map((_, index) => {
    if (index === 0) return secants[0] ?? 1;
    if (index === normalized.length - 1) return secants.at(-1) ?? 1;
    const previous = secants[index - 1];
    const next = secants[index];
    if (previous * next <= 0) return 0;
    const previousInterval = intervals[index - 1];
    const nextInterval = intervals[index];
    const previousWeight = 2 * nextInterval + previousInterval;
    const nextWeight = nextInterval + 2 * previousInterval;
    return (previousWeight + nextWeight) / (previousWeight / previous + nextWeight / next);
  });
  for (let index = 0; index < normalized.length - 1; index += 1) {
    const left = normalized[index];
    const right = normalized[index + 1];
    if (x <= right[0]) {
      const span = Math.max(0.000001, right[0] - left[0]);
      const t = Math.max(0, Math.min(1, (x - left[0]) / span));
      const t2 = t * t;
      const t3 = t2 * t;
      const output = (2 * t3 - 3 * t2 + 1) * left[1]
        + (t3 - 2 * t2 + t) * span * tangents[index]
        + (-2 * t3 + 3 * t2) * right[1]
        + (t3 - t2) * span * tangents[index + 1];
      return Math.max(0, Math.min(1, Math.max(
        Math.min(left[1], right[1]),
        Math.min(Math.max(left[1], right[1]), output),
      )));
    }
  }
  return normalized.at(-1)?.[1] ?? x;
}

export function cloneSolarPreset(name) {
  const preset = SOLAR_CURVE_PRESETS[name] || SOLAR_CURVE_PRESETS.neutral;
  return {
    ...preset,
    curvePoints: normalizeSolarCurvePoints(preset.curvePoints),
  };
}

export class SolarCurveEditor {
  constructor(canvas, { onInput, onCommit } = {}) {
    this.canvas = canvas || null;
    this.onInput = onInput || (() => {});
    this.onCommit = onCommit || (() => {});
    this.points = normalizeSolarCurvePoints();
    this.histogram = [];
    this.activeIndex = -1;
    this.pointerId = null;
    this.resizeObserver = null;
    if (!this.canvas) return;

    this.canvas.addEventListener("contextmenu", (event) => {
      event.preventDefault();
      const index = this.#nearestIndex(event);
      if (index > 0 && index < this.points.length - 1) {
        this.points.splice(index, 1);
        this.draw();
        this.onInput(this.getPoints());
        this.onCommit(this.getPoints());
      }
    });
    this.canvas.addEventListener("pointerdown", (event) => this.#start(event));
    this.canvas.addEventListener("pointermove", (event) => this.#move(event));
    this.canvas.addEventListener("pointerup", (event) => this.#finish(event));
    this.canvas.addEventListener("pointercancel", (event) => this.#finish(event));
    if (typeof ResizeObserver !== "undefined") {
      this.resizeObserver = new ResizeObserver(() => this.draw());
      this.resizeObserver.observe(this.canvas);
    }
    this.draw();
  }

  destroy() {
    this.resizeObserver?.disconnect();
  }

  getPoints() {
    return this.points.map((point) => [...point]);
  }

  setPoints(points, { notify = false } = {}) {
    this.points = normalizeSolarCurvePoints(points);
    this.draw();
    if (notify) this.onInput(this.getPoints());
  }

  setHistogram(histogram) {
    this.histogram = Array.isArray(histogram) ? histogram.map(Number) : [];
    this.draw();
  }

  #geometry() {
    const bounds = this.canvas.getBoundingClientRect();
    const width = Math.max(260, Math.round(bounds.width || this.canvas.width || 360));
    const height = Math.max(150, Math.round(bounds.height || this.canvas.height || 190));
    const dpr = Math.min(2, globalThis.devicePixelRatio || 1);
    if (this.canvas.width !== Math.round(width * dpr) || this.canvas.height !== Math.round(height * dpr)) {
      this.canvas.width = Math.round(width * dpr);
      this.canvas.height = Math.round(height * dpr);
    }
    const context = this.canvas.getContext("2d");
    context.setTransform(dpr, 0, 0, dpr, 0, 0);
    return { bounds, context, width, height, pad: 16 };
  }

  #pointFromEvent(event) {
    const { bounds, width, height, pad } = this.#geometry();
    const x = Math.max(0, Math.min(1, (event.clientX - bounds.left - pad) / Math.max(1, width - pad * 2)));
    const y = Math.max(0, Math.min(1, 1 - (event.clientY - bounds.top - pad) / Math.max(1, height - pad * 2)));
    return [x, y];
  }

  #nearestIndex(event) {
    const { bounds, width, height, pad } = this.#geometry();
    const px = event.clientX - bounds.left;
    const py = event.clientY - bounds.top;
    let best = -1;
    let distance = 14;
    this.points.forEach((point, index) => {
      const x = pad + point[0] * (width - pad * 2);
      const y = pad + (1 - point[1]) * (height - pad * 2);
      const candidate = Math.hypot(px - x, py - y);
      if (candidate < distance) {
        best = index;
        distance = candidate;
      }
    });
    return best;
  }

  #start(event) {
    if (event.button !== 0) return;
    event.preventDefault();
    let index = this.#nearestIndex(event);
    if (index < 0 && this.points.length < 12) {
      const point = this.#pointFromEvent(event);
      this.points.push(point);
      this.points.sort((left, right) => left[0] - right[0]);
      index = this.points.indexOf(point);
    }
    if (index < 0) return;
    this.activeIndex = index;
    this.pointerId = event.pointerId;
    this.canvas.setPointerCapture?.(event.pointerId);
    this.#move(event);
  }

  #move(event) {
    if (this.activeIndex < 0 || event.pointerId !== this.pointerId) return;
    event.preventDefault();
    const [rawX, y] = this.#pointFromEvent(event);
    const last = this.points.length - 1;
    const previousX = this.points[this.activeIndex - 1]?.[0] ?? 0;
    const nextX = this.points[this.activeIndex + 1]?.[0] ?? 1;
    const x = this.activeIndex === 0 ? 0
      : this.activeIndex === last ? 1
        : Math.max(previousX + 0.012, Math.min(nextX - 0.012, rawX));
    this.points[this.activeIndex] = [x, y];
    this.draw();
    this.onInput(this.getPoints());
  }

  #finish(event) {
    if (this.activeIndex < 0 || event.pointerId !== this.pointerId) return;
    this.canvas.releasePointerCapture?.(event.pointerId);
    this.activeIndex = -1;
    this.pointerId = null;
    this.onCommit(this.getPoints());
  }

  draw() {
    if (!this.canvas) return;
    const { context, width, height, pad } = this.#geometry();
    context.clearRect(0, 0, width, height);
    context.fillStyle = "#020617";
    context.fillRect(0, 0, width, height);

    context.strokeStyle = "rgba(148, 163, 184, .12)";
    context.lineWidth = 1;
    for (let index = 0; index <= 4; index += 1) {
      const x = pad + (width - pad * 2) * index / 4;
      const y = pad + (height - pad * 2) * index / 4;
      context.beginPath();
      context.moveTo(x, pad);
      context.lineTo(x, height - pad);
      context.stroke();
      context.beginPath();
      context.moveTo(pad, y);
      context.lineTo(width - pad, y);
      context.stroke();
    }

    if (this.histogram.length) {
      const maximum = Math.max(1, ...this.histogram);
      context.beginPath();
      this.histogram.forEach((count, index) => {
        const x = pad + index / Math.max(1, this.histogram.length - 1) * (width - pad * 2);
        const normalized = Math.log1p(Math.max(0, count)) / Math.log1p(maximum);
        const y = height - pad - normalized * (height - pad * 2) * 0.72;
        if (index === 0) context.moveTo(x, y); else context.lineTo(x, y);
      });
      context.lineTo(width - pad, height - pad);
      context.lineTo(pad, height - pad);
      context.closePath();
      context.fillStyle = "rgba(148, 163, 184, .12)";
      context.fill();
    }

    const samples = Math.max(128, Math.round(width));
    context.beginPath();
    for (let index = 0; index <= samples; index += 1) {
      const input = index / samples;
      const output = evaluateSolarCurve(this.points, input);
      const x = pad + input * (width - pad * 2);
      const y = pad + (1 - output) * (height - pad * 2);
      if (index === 0) context.moveTo(x, y); else context.lineTo(x, y);
    }
    context.strokeStyle = "#f59e0b";
    context.lineWidth = 2.2;
    context.stroke();

    this.points.forEach((point, index) => {
      const x = pad + point[0] * (width - pad * 2);
      const y = pad + (1 - point[1]) * (height - pad * 2);
      context.beginPath();
      context.arc(x, y, index === this.activeIndex ? 6 : 4.5, 0, Math.PI * 2);
      context.fillStyle = index === this.activeIndex ? "#fef3c7" : "#0f172a";
      context.fill();
      context.strokeStyle = "#fbbf24";
      context.lineWidth = 2;
      context.stroke();
    });
  }
}
