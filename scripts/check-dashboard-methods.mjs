#!/usr/bin/env node
//
// Catch `this.someMethod()` calls in the dashboard that no class defines.
//
// The dashboard has no build step and no test harness, so nothing else notices:
// `node --check` validates syntax and says nothing about whether a method
// exists, and the failure only appears when a user clicks the thing. A filter
// button calling `this.load()` when the method is named `refresh()` shipped
// exactly this way.
//
// Deliberately simple: it collects every method name defined anywhere in the
// file and checks every `this.NAME(` call site against that set plus Preact's
// own members. Cross-class calls would be a false negative; a typo — the case
// that actually happens — is caught.

import { readFileSync } from 'node:fs';

const FILE = process.argv[2] ?? 'src/dashboard/assets/app.js';
const source = readFileSync(FILE, 'utf8');

// Members Preact's Component provides, plus fields assigned in constructors.
const INHERITED = new Set([
  'setState',
  'forceUpdate',
  'render',
  'props',
  'state',
  'context',
  'base',
  'componentDidMount',
  'componentWillUnmount',
  'componentDidUpdate',
  'shouldComponentUpdate',
  'componentDidCatch',
]);

// `  name(args) {` or `  async name(args) {` at a class-body indent.
const defined = new Set(
  [...source.matchAll(/^\s{2}(?:async\s+)?([A-Za-z_$][\w$]*)\s*\([^)]*\)\s*\{/gm)].map(
    (m) => m[1],
  ),
);

// Fields assigned as `this.name = ...`, which are callable if they hold a function.
for (const m of source.matchAll(/\bthis\.([A-Za-z_$][\w$]*)\s*=/g)) {
  defined.add(m[1]);
}

const missing = new Map();
for (const m of source.matchAll(/\bthis\.([A-Za-z_$][\w$]*)\s*\(/g)) {
  const name = m[1];
  if (defined.has(name) || INHERITED.has(name)) continue;
  const line = source.slice(0, m.index).split('\n').length;
  if (!missing.has(name)) missing.set(name, line);
}

if (missing.size === 0) {
  console.log(`${FILE}: every this.<method>() call resolves`);
  process.exit(0);
}

for (const [name, line] of missing) {
  console.error(`${FILE}:${line}: this.${name}() is called but never defined`);
}
process.exit(1);
