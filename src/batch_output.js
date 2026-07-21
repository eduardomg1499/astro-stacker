export const BATCH_OUTPUT_POLICY_SOURCE_ADJACENT = "sourceAdjacent";
export const BATCH_OUTPUT_POLICY_SINGLE_DIRECTORY = "singleDirectory";

export function normalizeBatchOutputSettings(policy, directory) {
    const normalizedDirectory = typeof directory === "string" ? directory.trim() : "";
    if (policy === BATCH_OUTPUT_POLICY_SINGLE_DIRECTORY && normalizedDirectory) {
        return {
            policy: BATCH_OUTPUT_POLICY_SINGLE_DIRECTORY,
            directory: normalizedDirectory
        };
    }
    return {
        policy: BATCH_OUTPUT_POLICY_SOURCE_ADJACENT,
        directory: normalizedDirectory
    };
}

export function buildBatchOutputLookup(plan, expectedFiles = []) {
    if (!plan || plan.schemaVersion !== 1 || typeof plan.animationFolder !== "string" || !plan.animationFolder) {
        throw new Error("El backend devolvió un plan de salida del lote inválido.");
    }
    if (!Array.isArray(plan.entries)) {
        throw new Error("El plan de salida no contiene destinos por vídeo.");
    }
    const lookup = new Map();
    for (const entry of plan.entries) {
        if (!entry || typeof entry.sourcePath !== "string" || typeof entry.outputFolder !== "string") {
            throw new Error("El plan de salida contiene una entrada inválida.");
        }
        if (lookup.has(entry.sourcePath)) {
            throw new Error(`El plan de salida repite la fuente: ${entry.sourcePath}`);
        }
        lookup.set(entry.sourcePath, entry.outputFolder);
    }
    for (const file of expectedFiles) {
        if (!lookup.has(file)) {
            throw new Error(`El plan de salida no contiene la fuente: ${file}`);
        }
    }
    return lookup;
}

function cloneBatchContractValue(value) {
    if (Array.isArray(value)) return value.map(cloneBatchContractValue);
    if (value && typeof value === "object") {
        return Object.fromEntries(
            Object.entries(value).map(([key, nested]) => [key, cloneBatchContractValue(nested)])
        );
    }
    return value;
}

function deepFreezeBatchContract(value) {
    if (!value || typeof value !== "object" || Object.isFrozen(value)) return value;
    Object.values(value).forEach(deepFreezeBatchContract);
    return Object.freeze(value);
}

/**
 * Capture one immutable recipe before a batch starts.  The processing loop
 * must only read this snapshot: live sliders can still repaint while Rust is
 * busy and must never change the recipe half-way through an animation.
 */
export function freezeBatchProcessingContract(value) {
    return deepFreezeBatchContract(cloneBatchContractValue(value));
}

function firstNonEmptyString(...values) {
    return values.find(value => typeof value === "string" && value.trim().length > 0)?.trim() || "";
}

/**
 * Keep the Tauri response boundary explicit and backwards-compatible with
 * both Rust snake_case and JS camelCase.  A production batch result is valid
 * only when the prepared PNG and its untouched RGB16 master were published.
 */
export function normalizeBatchEntryResult(result) {
    const value = typeof result === "string" ? { path: result } : result;
    if (!value || typeof value !== "object") {
        throw new Error("El backend no devolvió un resultado batch válido.");
    }
    const preparedPath = firstNonEmptyString(
        value.path,
        value.preparedPath,
        value.prepared_path,
        value.outputPath,
        value.output_path
    );
    const linearMasterPath = firstNonEmptyString(
        value.masterPath,
        value.master_path,
        value.linearMasterPath,
        value.linear_master_path
    );
    const preview = firstNonEmptyString(
        value.previewBase64,
        value.preview_base64,
        preparedPath
    );
    if (!preparedPath) {
        throw new Error("El backend terminó la entrada, pero no publicó la imagen preparada.");
    }
    if (!linearMasterPath) {
        throw new Error("El backend publicó la imagen preparada sin su máster lineal RGB16.");
    }
    return Object.freeze({ preparedPath, linearMasterPath, preview });
}

export function formatBatchOutputError(value) {
    let error = value;
    if (typeof error === "string") {
        try {
            error = JSON.parse(error);
        } catch (_) {
            return error;
        }
    }
    if (error && typeof error === "object") {
        const message = typeof error.message === "string" ? error.message : "";
        const path = typeof error.path === "string" ? error.path : "";
        if (message && path && !message.includes(path)) return `${message}\n${path}`;
        if (message) return message;
        try {
            return JSON.stringify(error);
        } catch (_) {}
    }
    return String(value ?? "Error desconocido al preparar la salida del lote.");
}
