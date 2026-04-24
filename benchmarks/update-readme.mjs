#!/usr/bin/env node
// Regenerate the "Fast installs" ratio paragraph in README.md from
// benchmarks/results.json. Invoked at the tail of `mise run bench:bump`
// so bumping benchmark data keeps the landing-page ratios in sync.

import { readFileSync, writeFileSync } from 'node:fs'
import { dirname, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'

const repo = resolve(dirname(fileURLToPath(import.meta.url)), '..')
const results = JSON.parse(readFileSync(`${repo}/benchmarks/results.json`, 'utf8'))

const byKey = Object.fromEntries(results.rows.map((r) => [r.key, r.values]))

function warmRatio(tool) {
  const v = byKey['ci-warm']
  return v[tool] / v.aube
}

function maxRatio(tool) {
  return Math.max(...results.rows.map((r) => r.values[tool] / r.values.aube))
}

const warmPnpm = Math.round(warmRatio('pnpm'))
const warmBun = Math.round(warmRatio('bun'))
const maxPnpm = Math.round(maxRatio('pnpm'))
const maxBun = Math.round(maxRatio('bun'))

const paragraph = `**[Fast installs](https://aube.en.dev/benchmarks).** Warm CI is about ${warmPnpm}x faster than pnpm and ${warmBun}x faster than Bun in the current benchmarks. Across the fixture set, aube runs up to ~${maxPnpm}x faster than pnpm and up to ${maxBun}x faster than Bun.`

const START = '<!-- BENCH_RATIOS:START -->'
const END = '<!-- BENCH_RATIOS:END -->'
const readmePath = `${repo}/README.md`
const readme = readFileSync(readmePath, 'utf8')
const re = new RegExp(`${START}[\\s\\S]*?${END}`)
if (!re.test(readme)) {
  console.error(`README.md is missing ${START} ... ${END} markers`)
  process.exit(1)
}

writeFileSync(readmePath, readme.replace(re, `${START}\n${paragraph}\n${END}`))
console.log(`bench ratios: warm pnpm=${warmPnpm}x bun=${warmBun}x / max pnpm=${maxPnpm}x bun=${maxBun}x`)
