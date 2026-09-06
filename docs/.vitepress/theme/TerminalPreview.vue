<script setup lang="ts">
import { computed, onBeforeUnmount, ref } from "vue";

declare const __AUBE_VERSION__: string;

const version = __AUBE_VERSION__;
const packages = [
  "@vue/compiler-sfc@3.5.32",
  "vitepress@1.6.4",
  "react@18.3.1",
  "typescript@5.6.3",
  "vite@5.4.11",
  "esbuild@0.24.0",
  "rollup@4.28.0",
  "@types/node@22.10.1",
  "tailwindcss@3.4.15",
  "postcss@8.4.49",
  "zod@3.24.1",
  "hono@4.6.13",
  "vitest@2.1.8",
  "playwright@1.49.1",
  "astro@5.1.1",
  "three@0.171.0",
];
const spinners = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const elapsed = ref(0);
const running = ref(false);
const total = 319;
const duration = 2400;
let frame = 0;

const progress = computed(() => Math.min(1, elapsed.value / duration));
const done = computed(() => progress.value === 1);
const installed = computed(() => Math.floor(progress.value * total));
const progressBar = computed(() => {
  const filled = Math.floor(progress.value * 20);
  return "█".repeat(filled) + "░".repeat(20 - filled);
});
const packageRows = computed(() =>
  [0, 7, 13].map((offset) => ({
    name: packages[
      (Math.floor(elapsed.value / 110) + offset) % packages.length
    ],
    spinner:
      spinners[(Math.floor(elapsed.value / 80) + offset) % spinners.length],
  })),
);

function play() {
  cancelAnimationFrame(frame);
  if (window.matchMedia("(prefers-reduced-motion: reduce)").matches) {
    elapsed.value = duration;
    running.value = false;
    return;
  }

  elapsed.value = 0;
  running.value = true;
  const start = performance.now();
  function tick(now: number) {
    elapsed.value = now - start;
    if (!done.value) frame = requestAnimationFrame(tick);
    else running.value = false;
  }
  frame = requestAnimationFrame(tick);
}

onBeforeUnmount(() => cancelAnimationFrame(frame));
</script>

<template>
  <figure class="tui-preview">
    <div class="tui-window" aria-hidden="true">
      <div class="tui-titlebar">
        <span></span><span></span><span></span>
        <strong>aubr test</strong>
      </div>
      <div class="tui-output">
        <div>$ mise use aube</div>
        <div class="tui-muted">mise aube@{{ version }} ✓ installed</div>
        <div class="tui-muted">mise ./mise.toml tools: aube@{{ version }}</div>
        <div class="tui-command">$ aubr test</div>
        <div class="tui-heading">
          <span class="tui-accent">aube</span>
          <span class="tui-muted">{{ version }}</span>
          <span class="tui-muted">by jdx.dev</span>
        </div>
        <template v-if="!done">
          <div class="tui-progress">
            <span class="tui-accent">fetching</span>
            <span class="tui-blocks">{{ progressBar }}</span>
            <span>{{ installed }}/{{ total }} pkgs</span>
          </div>
          <div
            v-for="(pkg, index) in packageRows"
            :key="index"
            class="tui-package"
          >
            <span class="tui-accent">{{ pkg.spinner }}</span>
            <span>{{ pkg.name }}</span>
          </div>
        </template>
        <template v-else>
          <div class="tui-success">✓ installed {{ total }} packages</div>
          <div class="tui-success">✓ ran 100 tests successfully</div>
          <div>$ <span class="tui-accent">▍</span></div>
        </template>
      </div>
    </div>
    <figcaption>
      <span>
        Example: aubr test installs dependencies, then runs the tests. Package
        counts and output are illustrative.
      </span>
      <button type="button" :disabled="running" @click="play">
        {{ running ? "Playing…" : done ? "Replay preview" : "Play preview" }}
      </button>
    </figcaption>
    <span class="tui-status" role="status">
      {{
        done ? "Preview complete: dependencies installed and tests passed." : ""
      }}
    </span>
  </figure>
</template>

<style scoped>
.tui-preview {
  margin: 28px 0;
}

.tui-window {
  overflow: hidden;
  border: 1px solid #4a3c36;
  border-radius: 12px;
  background: linear-gradient(180deg, #28211e, #1c1715);
  color: #eee7df;
  box-shadow: 0 12px 32px -16px #0006;
  font-family: var(--aube-mono);
}

.tui-titlebar {
  display: flex;
  align-items: center;
  gap: 8px;
  padding: 12px 16px;
  border-bottom: 1px solid #4a3c36;
}

.tui-titlebar span {
  width: 9px;
  height: 9px;
  border-radius: 50%;
  background: #f59f75;
}

.tui-titlebar span:nth-child(2) {
  background: #e2b046;
}

.tui-titlebar span:nth-child(3) {
  background: #5ee0ba;
}

.tui-titlebar strong {
  margin-left: 8px;
  color: #b9ada4;
  font-size: 12px;
  font-weight: 400;
}

.tui-output {
  min-height: 284px;
  padding: 20px;
  font-size: 13px;
  line-height: 1.8;
  overflow-wrap: anywhere;
}

.tui-command {
  margin-top: 10px;
}

.tui-muted,
.tui-package {
  color: #b9ada4;
}

.tui-accent {
  color: #f9c4a1;
}

.tui-success {
  color: #8ddcb0;
}

.tui-heading,
.tui-progress,
.tui-package {
  display: flex;
  flex-wrap: wrap;
  align-items: center;
  gap: 0 10px;
}

.tui-blocks {
  color: #f59f75;
  white-space: nowrap;
}

figcaption {
  display: flex;
  flex-wrap: wrap;
  align-items: center;
  gap: 12px 20px;
  margin-top: 12px;
  color: var(--vp-c-text-2);
  font-size: 13px;
  line-height: 1.6;
}

figcaption > span {
  flex: 1 1 240px;
}

button {
  border: 1px solid var(--vp-c-divider);
  border-radius: 6px;
  padding: 6px 12px;
  color: var(--vp-c-text-1);
  background: var(--vp-c-bg-soft);
  font-weight: 500;
}

button:hover:not(:disabled) {
  border-color: var(--vp-c-brand-1);
}

button:focus-visible {
  outline: 2px solid var(--vp-c-brand-1);
  outline-offset: 3px;
}

button:disabled {
  opacity: 0.6;
  cursor: default;
}

.tui-status {
  position: absolute;
  width: 1px;
  height: 1px;
  overflow: hidden;
  clip-path: inset(50%);
  white-space: nowrap;
}

@media (max-width: 480px) {
  .tui-output {
    min-height: 340px;
    padding: 16px;
    font-size: 12px;
  }
}
</style>
