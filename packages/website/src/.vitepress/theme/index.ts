import type { Theme } from "vitepress";
import DefaultTheme from "vitepress/theme";
import Mermaid from "./components/Mermaid.vue";
import ReleaseHighlights from "./components/ReleaseHighlights.vue";
import "./custom.css";

export default {
  extends: DefaultTheme,
  enhanceApp({ app }) {
    app.component("Mermaid", Mermaid);
    app.component("ReleaseHighlights", ReleaseHighlights);
  },
} satisfies Theme;
