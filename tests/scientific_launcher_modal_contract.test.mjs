import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";

const html = readFileSync(new URL("../index.html", import.meta.url), "utf8");
const styles = readFileSync(new URL("../src/styles.css", import.meta.url), "utf8");
const main = readFileSync(new URL("../src/main.js", import.meta.url), "utf8");

test("the three scientific launchers share one visible beta contract", () => {
  for (const id of ["btn-deepsky-mode", "btn-milkyway-mode", "btn-deepsky-editor"]) {
    assert.match(
      html,
      new RegExp(`id=["']${id}["'][^>]*class=["'][^"']*beta-feature-button`),
      `${id} must expose the shared beta ribbon`,
    );
  }
  assert.match(styles, /\.beta-feature-button::after\s*\{[\s\S]*?content:\s*["']BETA["']/);
});

test("the FITS sequence launcher is hidden without removing its reusable code", () => {
  assert.match(html, /id="btn-analyze-fits"[^>]*\shidden(?:\s|>)/);
  assert.match(main, /const btnAnalyzeFits = \$\("#btn-analyze-fits"\)/);
  assert.match(main, /btnAnalyzeFits\.addEventListener\("click"/);
});

test("deep-sky backdrop clicks cannot dismiss work and both title marks optically center their icons", () => {
  assert.doesNotMatch(main, /if \(e\.target === modal\) closeWizard\(\)/);
  assert.match(main, /btn-deepsky-close"\)\?\.addEventListener\("click", closeWizard\)/);
  assert.match(main, /e\.key === "Escape"[^\n]*closeWizard\(\)/);
  assert.match(html, /class="donation-heart-mark ds-wizard-title-mark"/);
  assert.match(styles, /\.mw-title-mark \.zas-icon\s*\{[\s\S]*?margin:\s*0;/);
  assert.match(styles, /\.ds-progress-brand \.zas-icon\s*\{[\s\S]*?margin:\s*0;/);
  assert.match(html, /\.ds-wizard-title-mark \.zas-icon\s*\{[\s\S]*?margin:0;/);
});
