import react from "@astrojs/react";
import starlight from "@astrojs/starlight";
import { defineConfig } from "astro/config";

export default defineConfig({
  integrations: [
    react(),
    starlight({
      title: "zwinger",
      logo: { src: "./src/assets/cage.svg" },
      favicon: "/favicon.svg",
      // theme.css comes first: its `@layer` statement must precede the layers of motiv and Starlight.
      customCss: ["./src/styles/theme.css", "@driif/motiv/styles.css"],
      components: {
        Sidebar: "./src/components/Sidebar.astro",
        ThemeSelect: "./src/components/ThemeSelect.astro",
        Hero: "./src/components/Hero.astro",
      },
      social: [{ icon: "github", label: "GitHub", href: "https://github.com/driif/zwinger" }],
      sidebar: [
        "get-started",
        "projects",
        "cages",
        "agents",
        "network",
        "tasks",
        "shortcuts",
        "updates",
      ],
    }),
  ],
});
