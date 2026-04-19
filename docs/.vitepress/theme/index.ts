import DefaultTheme from "vitepress/theme";
import type { Theme } from "vitepress";
import { h } from "vue";
import AubeSocialLinks from "./AubeSocialLinks.vue";
import BenchChart from "./BenchChart.vue";
import EndevFooter from "./EndevFooter.vue";
import HomeLanding from "./HomeLanding.vue";
import "./custom.css";

export default {
  extends: DefaultTheme,
  Layout() {
    return h(DefaultTheme.Layout, null, {
      "layout-bottom": () => h(EndevFooter),
      "nav-bar-content-after": () => h(AubeSocialLinks),
      "nav-screen-content-after": () => h(AubeSocialLinks),
    });
  },
  enhanceApp({ app }) {
    app.component("BenchChart", BenchChart);
    app.component("HomeLanding", HomeLanding);
  },
} satisfies Theme;
