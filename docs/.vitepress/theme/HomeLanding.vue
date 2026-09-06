<script setup lang="ts">
import { computed, onBeforeUnmount, ref } from "vue";
import benchmarkResults from "../../../benchmarks/results.json";

declare const __AUBE_VERSION__: string;

const aubeVersion = __AUBE_VERSION__;
const installCommand = "mise use -g aube";
const copyStatus = ref("");
let copyTimer: ReturnType<typeof setTimeout> | undefined;
const warmValues = benchmarkResults.rows.find(
  (row) => row.key === "gvs-warm",
)?.values;
const tools = ["aube", "bun", "pnpm", "npm"] as const;
const measurements = computed(() => {
  const maximum = Math.max(...tools.map((tool) => warmValues?.[tool] ?? 0), 1);
  return tools.flatMap((tool) => {
    const value = warmValues?.[tool];
    const seconds =
      typeof value === "number"
        ? value / (benchmarkResults.unit === "ms" ? 1000 : 1)
        : undefined;
    if (typeof seconds !== "number" || seconds <= 0) return [];
    return [{ tool, seconds, width: `${((value ?? 0) / maximum) * 100}%` }];
  });
});

async function copyInstallCommand() {
  clearTimeout(copyTimer);
  try {
    if (!navigator.clipboard) throw new Error("Clipboard unavailable");
    await navigator.clipboard.writeText(installCommand);
    copyStatus.value = "Copied to clipboard";
  } catch {
    copyStatus.value = "Select the command and copy it manually";
  }
  copyTimer = setTimeout(() => {
    copyStatus.value = "";
  }, 4000);
}

onBeforeUnmount(() => clearTimeout(copyTimer));
</script>

<template>
  <div class="aube-home">
    <section class="aube-hero" aria-labelledby="aube-hero-title">
      <div class="aube-hero-copy">
        <a
          class="aube-release"
          :href="`https://github.com/jdx/aube/releases/tag/v${aubeVersion}`"
        >
          v{{ aubeVersion }} <span aria-hidden="true">·</span> Release notes
          <span aria-hidden="true">↗</span>
        </a>
        <p class="aube-eyebrow">A package manager for Node.js</p>
        <h1 id="aube-hero-title">
          Run your project.<br /><em>We’ll handle the install.</em>
        </h1>
        <p class="aube-lede">
          Run a script. aube installs what’s missing, reuses what’s there, and
          keeps your existing lockfile. Written in Rust, built for the everyday
          loop.
        </p>
        <div class="aube-actions">
          <a class="aube-button" href="/getting-started"
            >Get started <span aria-hidden="true">→</span></a
          >
          <a class="aube-text-link" href="/guide"
            >Explore the docs <span aria-hidden="true">→</span></a
          >
        </div>
        <div class="aube-install-command">
          <span aria-hidden="true">$</span>
          <code>{{ installCommand }}</code>
          <button
            type="button"
            aria-label="Copy install command"
            @click="copyInstallCommand"
          >
            Copy
          </button>
        </div>
        <div class="aube-install-note">
          <a href="/installation">Other installation methods</a>
          <span role="status">{{ copyStatus }}</span>
        </div>
      </div>
      <div class="aube-terminal-wrap">
        <div
          class="aube-terminal"
          aria-label="Example of running a project script with aube"
        >
          <div class="aube-terminal-bar">
            <span>~/your-project</span><span>terminal</span>
          </div>
          <div class="aube-terminal-body">
            <p><span class="aube-prompt">$</span> aubr build</p>
            <p class="aube-terminal-muted">
              Dependencies changed. Installing first…
            </p>
            <p class="aube-terminal-success">✓ Dependencies ready</p>
            <p class="aube-terminal-muted">$ vite build</p>
            <p class="aube-terminal-success">✓ Build complete</p>
            <div class="aube-terminal-divider"></div>
            <p><span class="aube-prompt">$</span> aubr build</p>
            <p class="aube-terminal-muted">
              Dependencies unchanged. Straight to your script.
            </p>
            <p class="aube-terminal-muted">$ vite build</p>
            <p class="aube-terminal-success">✓ Build complete</p>
          </div>
        </div>
        <p class="aube-terminal-caption">
          Illustrative output. Same command, one less step to remember.
        </p>
      </div>
    </section>

    <section class="aube-migration" aria-label="Migration guides">
      <p>Your project. Your lockfile.</p>
      <div>
        <a href="/pnpm-users">pnpm <span aria-hidden="true">↗</span></a>
        <a href="/npm-users">npm <span aria-hidden="true">↗</span></a>
        <a href="/yarn-users">Yarn <span aria-hidden="true">↗</span></a>
        <a href="/bun-users">Bun <span aria-hidden="true">↗</span></a>
      </div>
    </section>

    <section class="aube-workflow" aria-labelledby="aube-workflow-title">
      <div class="aube-section-heading">
        <p class="aube-eyebrow">Less setup, more doing</p>
        <h2 id="aube-workflow-title">Start with the command you need.</h2>
      </div>
      <div class="aube-command-grid">
        <a href="/package-manager/scripts#scripts"
          ><span>01 / Project scripts</span><code>aubr build</code>
          <p>Run a package script, with an install check first.</p></a
        >
        <a href="/package-manager/scripts#local-binaries"
          ><span>02 / Installed tools</span><code>aube exec vitest</code>
          <p>Use project binaries and the project’s Node version.</p></a
        >
        <a href="/package-manager/scripts#one-off-binaries"
          ><span>03 / One-off tools</span><code>aubx cowsay hi</code>
          <p>Use a local binary or fetch the tool when you need it.</p></a
        >
      </div>
    </section>

    <section class="aube-principles" aria-label="How aube manages dependencies">
      <article>
        <span class="aube-eyebrow">Familiar files</span>
        <h2>Keep the lockfile.<br />Try a different workflow.</h2>
        <p>
          aube reads and writes supported pnpm, npm, Yarn, and Bun lockfiles in
          place. Try it locally, review the diff, and run your tests before
          switching the team.
        </p>
        <a class="aube-text-link" href="/package-manager/lockfiles"
          >Check format compatibility <span aria-hidden="true">→</span></a
        >
      </article>
      <article>
        <span class="aube-eyebrow">Shared storage</span>
        <h2>Same dependencies.<br />Less duplicated work.</h2>
        <p>
          Package files live in a content-addressable store. The global virtual
          store also shares package directory trees across local projects and
          worktrees.
        </p>
        <a class="aube-text-link" href="/package-manager/global-virtual-store"
          >Understand the store <span aria-hidden="true">→</span></a
        >
      </article>
      <article>
        <span class="aube-eyebrow">Explicit build policy</span>
        <h2>Know what runs<br />during an install.</h2>
        <p>
          Dependency scripts need project approval or built-in trust. Explicit
          denies win. Optional build jails restrict approved scripts, with
          enforcement that depends on your OS.
        </p>
        <a class="aube-text-link" href="/package-manager/lifecycle-scripts"
          >Review dependency builds <span aria-hidden="true">→</span></a
        >
      </article>
      <article>
        <span class="aube-eyebrow">Checks during resolution</span>
        <h2>Look beyond<br />the version number.</h2>
        <p>
          aube checks publishing evidence, release age, and known malicious
          packages when selecting versions. Each check has documented defaults
          and exceptions.
        </p>
        <a class="aube-text-link" href="/security"
          >Read the security model <span aria-hidden="true">→</span></a
        >
      </article>
    </section>

    <section class="aube-benchmark" aria-labelledby="aube-benchmark-title">
      <div>
        <p class="aube-eyebrow">Measured, with context</p>
        <h2 id="aube-benchmark-title">
          A shorter wait<br />for a fresh install.
        </h2>
        <p>
          Warm cache, committed lockfile, no <code>node_modules</code>. These
          are recorded results for the same fixture using each tool’s default
          install model.
        </p>
        <a class="aube-text-link" href="/benchmarks"
          >All scenarios and methodology <span aria-hidden="true">→</span></a
        >
      </div>
      <div class="aube-benchmark-results">
        <p class="aube-chart-label">Warm install · seconds · lower is better</p>
        <div
          v-for="row in measurements"
          :key="row.tool"
          class="aube-chart-row"
          :class="{ 'is-aube': row.tool === 'aube' }"
        >
          <span>{{ row.tool }}</span>
          <div class="aube-chart-track" aria-hidden="true">
            <div :style="{ width: row.width }"></div>
          </div>
          <span>{{ row.seconds.toFixed(2) }} s</span>
        </div>
        <p class="aube-chart-note">
          aube’s global virtual store is enabled; pnpm’s is at its default of
          off.
        </p>
      </div>
    </section>
  </div>
</template>
