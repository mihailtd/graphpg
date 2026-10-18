import type { Theme } from "vitepress";
import DefaultTheme from "vitepress/theme";
import { h } from "vue";
import Mermaid from "./components/Mermaid.vue";
import ReleaseHighlights from "./components/ReleaseHighlights.vue";
import SiteFooter from "./components/SiteFooter.vue";
import TypeFlow from "./components/TypeFlow.vue";
import WritingCards from "./components/WritingCards.vue";
import "./custom.css";

export default {
  extends: DefaultTheme,
  // the site's own footer on every page, home and docs alike
  Layout: () => h(DefaultTheme.Layout, null, { "layout-bottom": () => h(SiteFooter) }),
  enhanceApp({ app }) {
    app.component("Mermaid", Mermaid);
    app.component("ReleaseHighlights", ReleaseHighlights);
    app.component("TypeFlow", TypeFlow);
    app.component("WritingCards", WritingCards);
  },
} satisfies Theme;
