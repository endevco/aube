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
const warm = byKey['ci-warm']
if (!warm) throw new Error("results.json missing row with key='ci-warm'")

const warmRatio = (tool) => warm[tool] / warm.aube
const maxRatio = (tool) => Math.max(...results.rows.map((r) => r.values[tool] / r.values.aube))

const warmPnpm = Math.round(warmRatio('pnpm'))
const warmBun = Math.round(warmRatio('bun'))
const maxPnpm = Math.round(maxRatio('pnpm'))
const maxBun = Math.round(maxRatio('bun'))

const paragraph = `**[Fast installs](https://aube.en.dev/benchmarks).** Warm CI is about ${warmPnpm}x faster than pnpm and ${warmBun}x faster than Bun in the current benchmarks. Across the fixture set, aube runs up to ~${maxPnpm}x faster than pnpm and up to ${maxBun}x faster than Bun.`

const START = '<!-- BENCH_RATIOS:START -->'
const END = '<!-- BENCH_RATIOS:END -->'
const readmePath = `${repo}/README.md`
const readme = readFileSync(readmePath, 'utf8')

const startIdx = readme.indexOf(START)
const endIdx = readme.indexOf(END, startIdx)
if (startIdx === -1 || endIdx === -1) {
  throw new Error(`README.md is missing ${START} ... ${END} markers`)
}

writeFileSync(readmePath, readme.slice(0, startIdx) + `${START}\n${paragraph}\n${END}` + readme.slice(endIdx + END.length))
console.log(`bench ratios: warm pnpm=${warmPnpm}x bun=${warmBun}x / max pnpm=${maxPnpm}x bun=${maxBun}x`)
