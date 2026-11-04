// Fails if a translation is missing or a component has text written directly in it.
import { readdirSync, readFileSync, statSync } from "node:fs";
import { join } from "node:path";

const root = new URL("../", import.meta.url).pathname;
const en = JSON.parse(readFileSync(join(root, "src/i18n/en.json"), "utf8"));
const es = JSON.parse(readFileSync(join(root, "src/i18n/es.json"), "utf8"));
const problems = [];

const flatten = (value, prefix = "") =>
  Object.entries(value).flatMap(([key, child]) =>
    typeof child === "object" ? flatten(child, `${prefix}${key}.`) : [`${prefix}${key}`],
  );
const enKeys = new Set(flatten(en));
const esKeys = new Set(flatten(es));
for (const key of enKeys) if (!esKeys.has(key)) problems.push(`missing in es.json: ${key}`);
for (const key of esKeys) if (!enKeys.has(key)) problems.push(`missing in en.json: ${key}`);

// Every reason the backend can return needs a sentence.
const core = readFileSync(join(root, "crates/keel-core/src/lib.rs"), "utf8");
const reasonEnum = core.slice(core.indexOf("pub enum Reason"));
for (const [, variant] of reasonEnum.matchAll(/^\s{4}([A-Z]\w+)\b/gm)) {
  const code = variant[0].toLowerCase() + variant.slice(1);
  if (!enKeys.has(`reasons.${code}`)) problems.push(`no translation for reason: ${code}`);
}
const stateEnum = core.slice(core.indexOf("pub enum AttemptState"), core.indexOf("impl AttemptState"));
for (const [, variant] of stateEnum.matchAll(/^\s{4}([A-Z]\w+),/gm)) {
  const code = variant[0].toLowerCase() + variant.slice(1);
  if (!enKeys.has(`states.${code}`)) problems.push(`no translation for state: ${code}`);
}

const files = (dir) =>
  readdirSync(dir).flatMap((name) => {
    const path = join(dir, name);
    return statSync(path).isDirectory() ? files(path) : path.endsWith(".tsx") && !path.endsWith(".test.tsx") ? [path] : [];
  });
const letters = /[A-Za-zÁÉÍÓÚáéíóúÑñ]{2,}/;
for (const file of files(join(root, "src"))) {
  const lines = readFileSync(file, "utf8").split("\n");
  lines.forEach((line, index) => {
    const where = `${file.replace(root, "")}:${index + 1}`;
    if (line.trim().startsWith("//") || line.trim().startsWith("*")) return;
    for (const [, attr, text] of line.matchAll(/\b(title|placeholder|label|aria-label|alt)="([^"]*)"/g)) {
      if (letters.test(text)) problems.push(`${where}: ${attr}="${text}"`);
    }
    for (const [, text] of line.matchAll(/>\s*([^<>{}]+?)\s*<\//g)) {
      if (letters.test(text) && !text.includes("=>")) problems.push(`${where}: text "${text}"`);
    }
    const alone = line.trim();
    if (/^[A-ZÁÉÍÓÚÑ][a-záéíóúñ]+(\s+[a-záéíóúñA-Z]+)*[.:]?$/.test(alone)) problems.push(`${where}: text "${alone}"`);
  });
}

if (problems.length) {
  console.error(problems.join("\n"));
  console.error(`\n${problems.length} i18n problem(s)`);
  process.exit(1);
}
console.log(`i18n ok: ${enKeys.size} keys in both languages`);
