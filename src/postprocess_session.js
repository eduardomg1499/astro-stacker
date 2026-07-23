const DEFAULT_HISTORY_LIMIT = 50;

function cloneValue(value) {
  if (value === undefined) return undefined;
  if (typeof structuredClone === "function") return structuredClone(value);
  return JSON.parse(JSON.stringify(value));
}

function stableValue(value) {
  if (Array.isArray(value)) return value.map(stableValue);
  if (value && typeof value === "object") {
    return Object.keys(value)
      .sort()
      .reduce((out, key) => {
        out[key] = stableValue(value[key]);
        return out;
      }, {});
  }
  return value;
}

function recipesEqual(left, right) {
  return JSON.stringify(stableValue(left)) === JSON.stringify(stableValue(right));
}

/**
 * Backend previews normally arrive as an absolute path, while mosaic previews
 * may use the explicit `file_path:` envelope. Keep the stored reference raw so
 * the UI can convert it with Tauri's asset protocol at display time.
 */
function unwrapPreviewReference(preview) {
  const value = String(preview || "");
  return value.startsWith("file_path:") ? value.slice("file_path:".length) : value;
}

function makeEntry(recipe, preview, label, timestamp = Date.now()) {
  return Object.freeze({
    recipe: cloneValue(recipe),
    preview: preview || "",
    label: String(label || "Ajuste"),
    timestamp,
  });
}

/**
 * History for a single immutable stacked result. The image itself always lives
 * in the Rust backend as 16-bit data; preview URLs are only display artifacts.
 */
export class PostProcessSession {
  constructor({ limit = DEFAULT_HISTORY_LIMIT } = {}) {
    this.limit = Math.max(2, Number(limit) || DEFAULT_HISTORY_LIMIT);
    this.generation = 0;
    this.source = "";
    this.entries = [];
    this.index = -1;
    this.pendingPreview = "";
  }

  beginResult({ generation, source = "", recipe = {}, preview = "", label = "Original" }) {
    this.generation = Number(generation) || 0;
    this.source = String(source || "");
    this.entries = [makeEntry(recipe, preview, label)];
    this.index = 0;
    this.pendingPreview = preview || "";
    return this.current();
  }

  clear() {
    this.generation = 0;
    this.source = "";
    this.entries = [];
    this.index = -1;
    this.pendingPreview = "";
  }

  setPreview(preview) {
    this.pendingPreview = String(preview || "");
  }

  commit(recipe, { label = "Ajuste", preview = this.pendingPreview } = {}) {
    if (this.index < 0) {
      return this.beginResult({ generation: this.generation, source: this.source, recipe, preview, label });
    }

    const current = this.entries[this.index];
    if (recipesEqual(current.recipe, recipe)) {
      if (preview && preview !== current.preview) {
        const replacement = makeEntry(current.recipe, preview, current.label, current.timestamp);
        this.entries.splice(this.index, 1, replacement);
      }
      return this.current();
    }

    this.entries.splice(this.index + 1);
    this.entries.push(makeEntry(recipe, preview, label));
    if (this.entries.length > this.limit) this.entries.shift();
    this.index = this.entries.length - 1;
    this.pendingPreview = preview || "";
    return this.current();
  }

  updateCurrentPreview(preview) {
    if (this.index < 0) return null;
    const current = this.entries[this.index];
    const replacement = makeEntry(current.recipe, preview, current.label, current.timestamp);
    this.entries.splice(this.index, 1, replacement);
    this.pendingPreview = preview || "";
    return this.current();
  }

  current() {
    if (this.index < 0 || !this.entries[this.index]) return null;
    return cloneValue(this.entries[this.index]);
  }

  undo() {
    if (!this.canUndo()) return null;
    this.index -= 1;
    return this.current();
  }

  redo() {
    if (!this.canRedo()) return null;
    this.index += 1;
    return this.current();
  }

  canUndo() {
    return this.index > 0;
  }

  canRedo() {
    return this.index >= 0 && this.index < this.entries.length - 1;
  }

  getCompareEntry(mode = "previous") {
    // At the original state there is no meaningful A/B comparison. Returning
    // the same entry used to enable A/B as "1/1" and made a missing preview look
    // like a broken previous version.
    if (this.index <= 0) return null;
    if (mode === "source") return cloneValue(this.entries[0]);
    return cloneValue(this.entries[this.index - 1]);
  }

  getState() {
    return {
      generation: this.generation,
      source: this.source,
      index: this.index,
      length: this.entries.length,
      canUndo: this.canUndo(),
      canRedo: this.canRedo(),
      canCompare: this.index > 0,
    };
  }
}

export { cloneValue, recipesEqual, unwrapPreviewReference };
