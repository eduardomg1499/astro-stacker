import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";

const [html, css] = await Promise.all([
    readFile(new URL("../index.html", import.meta.url), "utf8"),
    readFile(new URL("../src/styles.css", import.meta.url), "utf8"),
]);

const planetarySection = html.match(
    /<div class="planetary-settings-section">([\s\S]*?)<!-- Tutorial Settings -->/,
)?.[1];

test("planetary settings keep labels and selects in the scoped responsive layout", () => {
    assert.ok(planetarySection, "planetary settings section must remain identifiable");

    for (const id of [
        "sel-gpu-mode",
        "sel-planetary-quality-policy",
        "sel-decode-policy",
    ]) {
        assert.match(
            planetarySection,
            new RegExp(`id="${id}"[^>]*class="[^"]*planetary-settings-select`),
            `${id} must not fall back to the global width:100% flex layout`,
        );
    }

    assert.match(css, /\.planetary-settings-row\s*\{[^}]*display:\s*grid;/s);
    assert.match(css, /grid-template-columns:\s*minmax\(0, 1fr\)\s+minmax\(190px, 46%\)/);
    assert.match(css, /@container\s*\(max-width:\s*390px\)/);
});

test("quality and decoder guidance is connected to its control", () => {
    assert.match(
        planetarySection,
        /id="sel-planetary-quality-policy"[^>]*aria-describedby="planetary-quality-policy-hint"/,
    );
    assert.match(
        planetarySection,
        /id="sel-decode-policy"[^>]*aria-describedby="planetary-decode-policy-hint"/,
    );
});
