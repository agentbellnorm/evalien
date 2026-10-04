import assert from "node:assert/strict";
import { test } from "node:test";
import { readdirSync, readFileSync } from "node:fs";
import { dirname, join, relative, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const src = fileURLToPath(new URL("../src/", import.meta.url));

/** Each source file with the modules it imports, local ones as paths relative to src/. */
const files = readdirSync(src, { recursive: true, encoding: "utf8" })
  .filter((file) => file.endsWith(".mts"))
  .map((file) => {
    const specifiers = [...readFileSync(join(src, file), "utf8").matchAll(/^import[^;]*?from "([^"]+)"/gms)].map((m) => m[1]);
    return {
      file,
      local: specifiers.filter((s) => s.startsWith(".")).map((s) => relative(src, resolve(src, dirname(file), s))),
      external: specifiers.filter((s) => !s.startsWith(".")),
    };
  });

/** Top-level files are composition: entry points and what they share. Folders are modules. */
const isComposition = (file: string) => !file.includes("/") && file !== "contracts.mts";
const moduleOf = (file: string) => (file.includes("/") ? file.split("/")[0] : file);

test("the scan sees every module", () => {
  assert.deepEqual([...new Set(files.map(({ file }) => moduleOf(file)))].sort(), [
    "config.mts", "contracts.mts", "db", "evaluation", "harness", "inference", "lifecycle", "main.mts", "quality", "report.mts",
    "system-prompt.mts", "terminal", "trajectory",
  ]);
});

test("contracts depend on nothing", () => {
  const contracts = files.find(({ file }) => file === "contracts.mts")!;
  assert.deepEqual([...contracts.local, ...contracts.external], []);
});

test("modules know only the contracts and their own folder", () => {
  const violations = files
    .filter(({ file }) => !isComposition(file) && file !== "contracts.mts")
    .flatMap(({ file, local }) => local
      .filter((target) => target !== "contracts.mts" && moduleOf(target) !== moduleOf(file))
      .map((target) => `${file} imports ${target}`));
  assert.deepEqual(violations, []);
});

test("libraries stay inside the module that adapts them", () => {
  const owners: Record<string, string[]> = {
    "node:sqlite": ["db"],
    "node:readline": ["terminal"],
    "ai": ["inference"],
    "@ai-sdk/anthropic": ["inference"],
    "@ai-sdk/openai": ["inference"],
    "@ai-sdk/google": ["inference"],
  };
  const violations = files.flatMap(({ file, external }) => external
    .filter((lib) => owners[lib] && !owners[lib].includes(moduleOf(file)))
    .map((lib) => `${file} imports ${lib}`));
  assert.deepEqual(violations, []);
});
